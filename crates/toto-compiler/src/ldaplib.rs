// `ldaplib` — LDAP client via the system libldap (Enterprise
// Wave 3, Stage 3).
//
// Toto calls these through `toto_rt_ldap_*` foreign functions
// (declared in `gzv.std/ldap`, allowlisted in `ffi.rs`, refused
// by the JIT like the other pool surfaces). Every operation
// connects, binds, operates, and unbinds on the dedicated
// blocking pool (never inline on a worker, never on the poller
// — an LDAP round-trip blocks; same policy as DB). The
// interpreter calls these inline (its blocking-call discipline,
// same as DB/TLS). No handle cache in v1 (documented): each
// call is self-contained.
//
// FFI-first, no Toto-implemented protocol anywhere: libldap is
// the mature native client (Linux-only — no system libldap the
// macOS probe can link, so the module is
// `cfg(target_os = "linux")` to match). Only long-stable,
// opaque-pointer-only APIs are used (`ldap_initialize`,
// `ldap_simple_bind_s`, `ldap_search_s`, the entry/attribute
// walk, `ldap_unbind_ext_s`). Structs-by-value, dlopen, and
// binding generation stay deferred. No directory logic of
// Toto's own.
//
// Results cross back as a JSON array string (parsed by
// `gzv.std/json` — no new framing, JSON is the Wave-1
// carrier): `[{"dn":"...","attrs":{"cn":["a"],"mail":[...]}}]`.
// Binary attribute values arrive lossy (UTF-8 lossy, like the
// DB blob rule — no blob in v1, documented).

#[cfg(target_os = "linux")]
pub use inner::*;
#[cfg(target_os = "linux")]
mod inner {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};

    enum Ldap {}
    enum LdapMsg {}
    enum Ber {}

    /// `struct berval { ber_len_t bv_len; char *bv_val; }`
    /// (LP64: 8-byte len + 8-byte pointer — stable ABI since the
    /// 1990s; read field-by-field, never constructed here).
    #[repr(C)]
    struct BerVal {
        len: usize,
        ptr: *const c_char,
    }

    const SUCCESS: c_int = 0;
    const SCOPE_SUBTREE: c_int = 2;
    // `ldap_set_option` selectors (stable numbers).
    const OPT_PROTOCOL_VERSION: c_int = 0x11;
    const VERSION3: c_int = 3;

    #[link(name = "ldap")]
    unsafe extern "C" {
        fn ldap_initialize(ldp: *mut *mut Ldap, uri: *const c_char) -> c_int;
        fn ldap_set_option(ld: *mut Ldap, option: c_int, invalue: *const c_void) -> c_int;
        fn ldap_simple_bind_s(ld: *mut Ldap, who: *const c_char, passwd: *const c_char) -> c_int;
        #[allow(clippy::too_many_arguments)]
        fn ldap_search_s(
            ld: *mut Ldap,
            base: *const c_char,
            scope: c_int,
            filter: *const c_char,
            attrs: *mut *mut c_char,
            attrsonly: c_int,
            res: *mut *mut LdapMsg,
        ) -> c_int;
        fn ldap_first_entry(ld: *mut Ldap, res: *mut LdapMsg) -> *mut LdapMsg;
        fn ldap_next_entry(ld: *mut Ldap, entry: *mut LdapMsg) -> *mut LdapMsg;
        fn ldap_get_dn(ld: *mut Ldap, entry: *mut LdapMsg) -> *mut c_char;
        fn ldap_first_attribute(
            ld: *mut Ldap,
            entry: *mut LdapMsg,
            ber: *mut *mut Ber,
        ) -> *mut c_char;
        fn ldap_next_attribute(ld: *mut Ldap, entry: *mut LdapMsg, ber: *mut Ber) -> *mut c_char;
        fn ldap_get_values_len(
            ld: *mut Ldap,
            entry: *mut LdapMsg,
            attr: *const c_char,
        ) -> *mut *mut BerVal;
        fn ldap_value_free_len(vals: *mut *mut BerVal);
        fn ldap_memfree(p: *mut c_void);
        fn ber_free(ber: *mut Ber, freebuf: c_int);
        fn ldap_msgfree(res: *mut LdapMsg) -> c_int;
        fn ldap_unbind_ext_s(
            ld: *mut Ldap,
            serverctrls: *mut c_void,
            clientctrls: *mut c_void,
        ) -> c_int;
        fn ldap_err2string(err: c_int) -> *const c_char;
    }

    fn err_str(code: c_int, what: &str) -> String {
        // SAFETY: `err2string` returns a static NUL string.
        let msg = unsafe {
            let p = ldap_err2string(code);
            if p.is_null() {
                "unknown LDAP error".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        format!("ldap {what}: {msg} (code {code})")
    }

    /// JSON-escape one string (ASCII fast path; other planes go
    /// through `\\uXXXX` with surrogate pairs — same contract as
    /// the Toto JSON writer for control characters).
    fn json_escape(s: &str, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }

    /// Owned C strings for one operation (every borrowed pointer
    /// below lives as long as this bundle).
    struct OpStrings {
        base: CString,
        filter: CString,
        attrs: Vec<CString>,
    }

    fn op_strings(
        base: &str,
        filter: &str,
        attrs_csv: &str,
    ) -> Result<(OpStrings, Vec<*mut c_char>), String> {
        let cstr = |s: &str, what: &str| {
            CString::new(s).map_err(|_| format!("bad ldap {what} (NUL byte)"))
        };
        let attrs: Vec<CString> = attrs_csv
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| cstr(a, "attribute"))
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*mut c_char> = attrs.iter().map(|a| a.as_ptr() as *mut c_char).collect();
        ptrs.push(std::ptr::null_mut());
        Ok((
            OpStrings {
                base: cstr(base, "base DN")?,
                filter: cstr(filter, "filter")?,
                attrs,
            },
            ptrs,
        ))
    }

    /// Connect + simple bind. Returns the live handle (single
    /// owner — the calling pool job closes it on every path).
    fn connect(host: &str, port: u16, bind_dn: &str, password: &str) -> Result<*mut Ldap, String> {
        let uri = CString::new(format!("ldap://{host}:{port}"))
            .map_err(|_| "bad ldap uri (NUL byte)".to_string())?;
        let dn = CString::new(bind_dn).map_err(|_| "bad ldap bind DN (NUL byte)".to_string())?;
        let pw = CString::new(password).map_err(|_| "bad ldap password (NUL byte)".to_string())?;
        let mut ld: *mut Ldap = std::ptr::null_mut();
        // SAFETY: `uri` outlives the call; `ld` null-checked.
        let r = unsafe { ldap_initialize(&mut ld as *mut *mut Ldap, uri.as_ptr()) };
        if r != SUCCESS || ld.is_null() {
            return Err(err_str(r, "initialize"));
        }
        // LDAPv3 (the library defaults to v2, which servers
        // reject with a protocol error).
        let ver: c_int = VERSION3;
        // SAFETY: `ld` is live; `ver` outlives the call.
        let r = unsafe {
            ldap_set_option(
                ld,
                OPT_PROTOCOL_VERSION,
                &ver as *const c_int as *const c_void,
            )
        };
        if r != SUCCESS {
            let e = err_str(r, "version");
            unsafe {
                ldap_unbind_ext_s(ld, std::ptr::null_mut(), std::ptr::null_mut());
            }
            return Err(e);
        }
        // SAFETY: `ld` is live; strings outlive the call.
        let r = unsafe { ldap_simple_bind_s(ld, dn.as_ptr(), pw.as_ptr()) };
        if r != SUCCESS {
            let e = err_str(r, "bind");
            unsafe {
                ldap_unbind_ext_s(ld, std::ptr::null_mut(), std::ptr::null_mut());
            }
            return Err(e);
        }
        Ok(ld)
    }

    fn close(ld: *mut Ldap) {
        // SAFETY: live handle, single owner (pool job).
        unsafe {
            ldap_unbind_ext_s(ld, std::ptr::null_mut(), std::ptr::null_mut());
        }
    }

    /// Read one entry's attributes into JSON (`"attrs"` object).
    /// Attribute names are library-provided UTF-8; values are
    /// lossy-decoded (binary values have no Toto representation —
    /// same rule as DB blobs, documented).
    ///
    /// SAFETY: `ld`/`entry` are live for the whole walk; every
    /// library-allocated string is freed on its path. The caller
    /// upholds this (pool job holding the live handle).
    fn entry_json(ld: *mut Ldap, entry: *mut LdapMsg, out: &mut String) {
        // SAFETY: the whole walk dereferences library-managed
        // pointers; every allocation is freed on its path below.
        let dn_ptr = unsafe { ldap_get_dn(ld, entry) };
        let dn = if dn_ptr.is_null() {
            String::new()
        } else {
            let s = unsafe { CStr::from_ptr(dn_ptr).to_string_lossy().into_owned() };
            unsafe { ldap_memfree(dn_ptr as *mut c_void) };
            s
        };
        out.push_str("{\"dn\":");
        json_escape(&dn, out);
        out.push_str(",\"attrs\":{");
        let mut ber: *mut Ber = std::ptr::null_mut();
        let mut attr = unsafe { ldap_first_attribute(ld, entry, &mut ber as *mut *mut Ber) };
        let mut first_attr = true;
        while !attr.is_null() {
            if !first_attr {
                out.push(',');
            }
            first_attr = false;
            let name = unsafe { CStr::from_ptr(attr).to_string_lossy().into_owned() };
            json_escape(&name, out);
            out.push(':');
            out.push('[');
            let vals = unsafe { ldap_get_values_len(ld, entry, attr) };
            if !vals.is_null() {
                let mut i = 0;
                let mut first_val = true;
                loop {
                    let v = unsafe { *vals.add(i) };
                    if v.is_null() {
                        break;
                    }
                    if !first_val {
                        out.push(',');
                    }
                    first_val = false;
                    let bytes =
                        unsafe { std::slice::from_raw_parts((*v).ptr as *const u8, (*v).len) };
                    json_escape(&String::from_utf8_lossy(bytes), out);
                    i += 1;
                }
                unsafe { ldap_value_free_len(vals) };
            }
            out.push(']');
            let next = attr;
            attr = unsafe { ldap_next_attribute(ld, entry, ber) };
            unsafe { ldap_memfree(next as *mut c_void) };
        }
        if !ber.is_null() {
            unsafe { ber_free(ber, 0) };
        }
        out.push_str("}}");
    }

    /// Bind-only check (authentication without search).
    pub fn bind(host: &str, port: u16, bind_dn: &str, password: &str) -> Result<(), String> {
        let ld = connect(host, port, bind_dn, password)?;
        close(ld);
        Ok(())
    }

    /// Subtree search returning a JSON array string.
    pub fn search(
        host: &str,
        port: u16,
        bind_dn: &str,
        password: &str,
        base: &str,
        filter: &str,
        attrs_csv: &str,
    ) -> Result<String, String> {
        let (strings, mut attr_ptrs) = op_strings(base, filter, attrs_csv)?;
        let ld = connect(host, port, bind_dn, password)?;
        let r = (|| {
            // SAFETY: `ld` and `strings` are live; `attr_ptrs` is a
            // null-terminated pointer array (or null for all attrs).
            let attrs_arg = if strings.attrs.is_empty() {
                std::ptr::null_mut()
            } else {
                attr_ptrs.as_mut_ptr()
            };
            let mut res: *mut LdapMsg = std::ptr::null_mut();
            // SAFETY: synchronous call; `res` freed below on every
            // path (success reads it, failure frees it).
            let r = unsafe {
                ldap_search_s(
                    ld,
                    strings.base.as_ptr(),
                    SCOPE_SUBTREE,
                    strings.filter.as_ptr(),
                    attrs_arg,
                    0,
                    &mut res as *mut *mut LdapMsg,
                )
            };
            if r != SUCCESS {
                if !res.is_null() {
                    unsafe {
                        ldap_msgfree(res);
                    }
                }
                return Err(err_str(r, "search"));
            }
            let mut out = String::from("[");
            let mut first = true;
            // SAFETY: `res` holds the result set for this walk.
            unsafe {
                let mut entry = ldap_first_entry(ld, res);
                while !entry.is_null() {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    entry_json(ld, entry, &mut out);
                    entry = ldap_next_entry(ld, entry);
                }
                ldap_msgfree(res);
            }
            out.push(']');
            Ok(out)
        })();
        close(ld);
        r
    }
}
