// `tlslib` — TLS via the system libssl (Enterprise Wave 2, Stage 1).
//
// Toto calls these through `toto_rt_tls_*` foreign functions
// (declared in `gzv.std/tls`, allowlisted in `ffi.rs`, refused by
// the JIT like the net/crypto surface). The TLS state machine
// itself runs inside the linked system libssl (OpenSSL 3 on Linux,
// LibreSSL on macOS — the same load-time link model as libcrypto;
// see `docs/ffi.md` and the `toto-compiler` build script). Nothing
// here implements TLS, record framing, or certificate logic: every
// handshake byte, record byte, and chain verdict comes out of the
// library. This module is handle/bytes plumbing only: opaque
// pointers that never cross into Toto (Toto sees `int` fds and
// `Array<int>` bytes, exactly like `gzv.std/net`), and
// `Result<_, String>` errors. Unix-only like the net surface;
// Windows gets clean check-time `E0301` (see `POSIX_ONLY`).
//
// Symbol shapes (STOP flag (d) discharge): this machine carries no
// libssl headers (`libssl-dev` absent), so every declaration below
// was confirmed against the installed runtime instead — each name
// is present in the dynamic symbol table (`nm -D libssl.so.3`:
// `T` for every `SSL_*`/`TLS_*` name used here; the two
// private-key helpers are `T` in libcrypto.so.3, already linked
// by Wave 1). Only long-stable, opaque-pointer-only APIs are used
// (`TLS_client_method`/`TLS_server_method`, `SSL_CTX_new/free`,
// `SSL_new/free`, `SSL_set_fd`, `SSL_connect/accept/read/write/`
// `shutdown/get_error/get_verify_result`, `SSL_CTX_use_certificate`
// /`use_PrivateKey`/`check_private_key`/`get_cert_store` — all
// pointers, ints, and one long, in every OpenSSL 1.1.1+/3.x and
// LibreSSL release. `SSL_set_tlsext_host_name` is deliberately NOT
// used: upstream it is a C macro (not an exported symbol), so it
// cannot be linked; Stage 1 sends no SNI (loopback only).
//
// Verification reuses the Wave 1 trust model: the caller supplies
// the same CA PEM fixtures (`ca.pem`), the library builds the same
// `X509_STORE` chain check, and the handshake enforces it
// (`SSL_VERIFY_PEER`). Hostname checking stays out exactly as in
// Wave 1 `x509_verify` (chain only) — a recorded follow-up, not a
// silent addition.
//
// Blocking discipline: every handshake/read/write call blocks (the
// fd is forced blocking on entry — see `ensure_blocking`, harmless
// when the interpreter already uses blocking sockets). The
// interpreter calls these inline; the runtime executor runs them on
// the dedicated blocking pool (never inline, never on the poller —
// an SSL step is not a `poll()` readiness wait, so §17.5 parks it
// on the pool like DNS). `WANT_READ`/`WANT_WRITE` cannot fire on a
// blocking fd; the retry loop through `poll_wait` is defense only.
//
// Sessions are retained (an `SSL*` lives from handshake to close),
// which D6 allows with an explicit handle rule — here it is: the
// `SSL*` is keyed by its fd in a process-global table, Toto names
// only the fd, `tls_close` (or any `net_close`) frees the session,
// and a second close names the missing session loudly instead of
// touching a recycled fd. Concurrent use of one connection from
// two tasks is a user race (OpenSSL `SSL*` objects are not
// thread-safe for concurrent use), same as sharing one fd today.

#[cfg(unix)]
pub use inner::*;
#[cfg(unix)]
mod inner {
    use std::collections::HashMap;
    use std::os::raw::{c_char, c_int, c_long, c_void};
    use std::sync::{LazyLock, Mutex};

    // Opaque OpenSSL types: pointers only, never constructed here.
    // `X509`/`X509Store`/`EvpPkey` and the BIO/PEM helpers live in
    // `cryptolib` (the single declaration site for libcrypto
    // symbols); only libssl symbols are declared here.
    use crate::cryptolib::{EvpPkey, X509, X509Store};
    enum SslMethod {}
    enum SslCtx {}
    enum Ssl {}

    // `SSL_VERIFY_PEER` (stable value, both families).
    const VERIFY_PEER: c_int = 0x01;
    // `SSL_ERROR_WANT_READ` / `SSL_ERROR_WANT_WRITE` (stable values).
    const WANT_READ: c_int = 2;
    const WANT_WRITE: c_int = 3;
    // `X509_V_OK` (stable value).
    const VERIFY_OK: c_long = 0;

    #[link(name = "ssl")]
    unsafe extern "C" {
        fn TLS_client_method() -> *const SslMethod;
        fn TLS_server_method() -> *const SslMethod;
        fn SSL_CTX_new(meth: *const SslMethod) -> *mut SslCtx;
        fn SSL_CTX_free(ctx: *mut SslCtx);
        fn SSL_CTX_set_verify(
            ctx: *mut SslCtx,
            mode: c_int,
            cb: Option<unsafe extern "C" fn(c_int, *mut c_void) -> c_int>,
        );
        fn SSL_CTX_get_cert_store(ctx: *const SslCtx) -> *mut c_void;
        fn SSL_CTX_use_certificate(ctx: *mut SslCtx, x: *mut X509) -> c_int;
        fn SSL_CTX_use_PrivateKey(ctx: *mut SslCtx, pkey: *mut EvpPkey) -> c_int;
        fn SSL_CTX_check_private_key(ctx: *const SslCtx) -> c_int;
        fn SSL_new(ctx: *mut SslCtx) -> *mut Ssl;
        fn SSL_free(ssl: *mut Ssl);
        fn SSL_set_fd(ssl: *mut Ssl, fd: c_int) -> c_int;
        fn SSL_connect(ssl: *mut Ssl) -> c_int;
        fn SSL_accept(ssl: *mut Ssl) -> c_int;
        fn SSL_read(ssl: *mut Ssl, buf: *mut c_void, num: c_int) -> c_int;
        fn SSL_write(ssl: *mut Ssl, buf: *const c_void, num: c_int) -> c_int;
        fn SSL_shutdown(ssl: *mut Ssl) -> c_int;
        fn SSL_get_error(ssl: *const Ssl, ret: c_int) -> c_int;
        fn SSL_get_verify_result(ssl: *const Ssl) -> c_long;
    }

    /// Drain the OpenSSL error queue into one line (`ctx` names the
    /// call); empty queue falls back to the context alone. Same
    /// discipline as the private `cryptolib::ossl_err`; fixed
    /// prefixes everywhere goldens pin, never bare library wording.
    fn ossl_err(ctx: &str) -> String {
        unsafe extern "C" {
            fn ERR_get_error() -> u64;
            fn ERR_error_string_n(e: u64, buf: *mut c_char, len: usize);
        }
        let mut parts = Vec::new();
        for _ in 0..8 {
            let code = unsafe { ERR_get_error() };
            if code == 0 {
                break;
            }
            let mut buf = vec![0 as c_char; 256];
            unsafe { ERR_error_string_n(code, buf.as_mut_ptr(), buf.len()) };
            let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            parts.push(s);
        }
        if parts.is_empty() {
            ctx.to_string()
        } else {
            format!("{ctx}: {}", parts.join("; "))
        }
    }

    /// A live `SSL*` session handle. The pointer is dereferenced
    /// only by the task that owns the fd's session (one task per
    /// connection at a time — the same discipline as the fd itself),
    /// and freed only after removal from the table under the lock,
    /// so sharing the table across pool/interpreter threads is
    /// sound. Concurrent use of one connection from two tasks is a
    /// user race (OpenSSL `SSL*` objects are not thread-safe for
    /// concurrent use), reported loudly, never silently.
    struct Session(*mut Ssl);
    // SAFETY: as documented above — single-owner use, removal
    // before free, table lock for every insert/remove/lookup.
    unsafe impl Send for Session {}
    unsafe impl Sync for Session {}

    /// Live TLS sessions: fd → `SSL*`. The `SSL*` never crosses into
    /// Toto (Toto names only the fd); freed by `tls_close` or by any
    /// `net_close` of the same fd (`free_session` is idempotent).
    static SESSIONS: LazyLock<Mutex<HashMap<c_int, Session>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Force the fd blocking (harmless when already blocking, as on
    /// the interpreter path). TLS steps run on the blocking pool /
    /// in blocking calls, never on the poller, so blocking is the
    /// correct mode for the whole session lifetime.
    fn ensure_blocking(fd: c_int) -> Result<(), String> {
        crate::netsys::set_blocking(fd)
    }

    /// Retry a `WANT_READ`/`WANT_WRITE` through `poll` (defense only:
    /// cannot fire on a blocking fd). Any other error drains the
    /// queue into the message.
    fn want_retry(ssl: *mut Ssl, fd: c_int, ret: c_int, ctx: &str) -> Result<(), String> {
        // SAFETY: `ssl` is live for the whole handshake/I-O call.
        let err = unsafe { SSL_get_error(ssl as *const Ssl, ret) };
        if err == WANT_READ {
            crate::netsys::poll_wait(&[(fd, true, false)], -1)
                .map_err(|e| format!("{ctx}: {e}"))?;
            return Ok(());
        }
        if err == WANT_WRITE {
            crate::netsys::poll_wait(&[(fd, false, true)], -1)
                .map_err(|e| format!("{ctx}: {e}"))?;
            return Ok(());
        }
        Err(ossl_err(ctx))
    }

    /// Client upgrade: run the handshake on a connected TCP fd,
    /// verifying the server chain against `ca_pem` (Wave 1 trust
    /// model, enforced by the library at handshake time). On success
    /// the session is stored under the fd; the fd stays the handle.
    pub fn client_connect(fd: c_int, ca_pem: &str) -> Result<(), String> {
        if fd < 0 {
            return Err("tls handshake: bad file descriptor".to_string());
        }
        ensure_blocking(fd)?;
        // SAFETY: every foreign pointer below is null-checked; the
        // CTX and parsed cert are freed on every path; the `SSL*`
        // escapes only into the session table on success.
        unsafe {
            let ca = crate::cryptolib::pem_to_x509(ca_pem, "tls ca")?;
            let r = (|| {
                let ctx = tls_client_method_checked()?;
                let r = (|| {
                    tls_ctx_set_verify_peer(ctx);
                    let store = SSL_CTX_get_cert_store(ctx);
                    if store.is_null() {
                        return Err("tls handshake: no trust store".to_string());
                    }
                    // add_cert bumps the refcount on success; the
                    // parsed `ca` is still ours to free.
                    if crate::cryptolib::X509_STORE_add_cert(store as *mut X509Store, ca) != 1 {
                        return Err(ossl_err("tls trust add"));
                    }
                    let ssl = SSL_new(ctx);
                    if ssl.is_null() {
                        return Err(ossl_err("tls handshake alloc"));
                    }
                    let r = ssl_handshake(ssl, fd, true);
                    if r.is_err() {
                        SSL_free(ssl);
                    }
                    r
                })();
                SSL_CTX_free(ctx);
                r
            })();
            crate::cryptolib::X509_free(ca);
            r
        }
    }

    /// Server upgrade: run the handshake on an accepted TCP fd with
    /// the given certificate chain + private key. No client-cert
    /// verification (one-way TLS, stated in the report).
    pub fn server_accept(fd: c_int, cert_pem: &str, key_pem: &str) -> Result<(), String> {
        if fd < 0 {
            return Err("tls handshake: bad file descriptor".to_string());
        }
        ensure_blocking(fd)?;
        // SAFETY: same contract as `client_connect`.
        unsafe {
            let cert = crate::cryptolib::pem_to_x509(cert_pem, "tls certificate")?;
            let r = (|| {
                let key = crate::cryptolib::pem_to_pkey(key_pem, "tls key")?;
                let r = (|| {
                    let ctx = tls_server_method_checked()?;
                    let r = (|| {
                        if SSL_CTX_use_certificate(ctx, cert) != 1 {
                            return Err(ossl_err("tls certificate use"));
                        }
                        if SSL_CTX_use_PrivateKey(ctx, key) != 1 {
                            return Err(ossl_err("tls key use"));
                        }
                        if SSL_CTX_check_private_key(ctx) != 1 {
                            return Err("tls handshake: key does not match certificate".to_string());
                        }
                        let ssl = SSL_new(ctx);
                        if ssl.is_null() {
                            return Err(ossl_err("tls handshake alloc"));
                        }
                        let r = ssl_handshake(ssl, fd, false);
                        if r.is_err() {
                            SSL_free(ssl);
                        }
                        r
                    })();
                    SSL_CTX_free(ctx);
                    r
                })();
                crate::cryptolib::EVP_PKEY_free(key);
                r
            })();
            // `use_certificate` bumps the refcount on success, so the
            // parsed cert is still ours on every path.
            crate::cryptolib::X509_free(cert);
            r
        }
    }

    /// Allocate a client `SSL_CTX` (null-checked constructor).
    fn tls_client_method_checked() -> Result<*mut SslCtx, String> {
        // SAFETY: null-checked constructor; freed by the caller.
        unsafe {
            let ctx = SSL_CTX_new(TLS_client_method());
            if ctx.is_null() {
                return Err(ossl_err("tls context alloc"));
            }
            Ok(ctx)
        }
    }

    /// Allocate a server `SSL_CTX` (null-checked constructor).
    fn tls_server_method_checked() -> Result<*mut SslCtx, String> {
        // SAFETY: null-checked constructor; freed by the caller.
        unsafe {
            let ctx = SSL_CTX_new(TLS_server_method());
            if ctx.is_null() {
                return Err(ossl_err("tls context alloc"));
            }
            Ok(ctx)
        }
    }

    /// `SSL_CTX_set_verify(ctx, VERIFY_PEER, NULL)`: the library
    /// verifies the peer chain during the handshake; no callback
    /// (no retained Toto state, no re-entry — D6 holds).
    fn tls_ctx_set_verify_peer(ctx: *mut SslCtx) {
        // SAFETY: `ctx` is live; NULL callback asks the library to
        // enforce with its default verify behavior.
        unsafe {
            SSL_CTX_set_verify(ctx, VERIFY_PEER, None);
        }
    }

    /// Bind `SSL*` to the fd, run the handshake to completion, and
    /// store the live session under the fd. `is_client` selects
    /// `SSL_connect` vs `SSL_accept`; on the client side the chain
    /// verdict is re-checked explicitly (fixed string, no library
    /// wording in goldens).
    fn ssl_handshake(ssl: *mut Ssl, fd: c_int, is_client: bool) -> Result<(), String> {
        // SAFETY: `ssl` is a live object owned by this call until
        // stored below; freed by the caller on error.
        unsafe {
            if SSL_set_fd(ssl, fd) != 1 {
                return Err(ossl_err("tls handshake attach"));
            }
            let ctx = if is_client {
                "tls handshake (client)"
            } else {
                "tls handshake (server)"
            };
            loop {
                let r = if is_client {
                    SSL_connect(ssl)
                } else {
                    SSL_accept(ssl)
                };
                if r == 1 {
                    break;
                }
                if r == 0 {
                    // Controlled failure (e.g. peer rejected the
                    // chain): queue text when present, fixed context
                    // otherwise so goldens hold on both families.
                    return Err(ossl_err(ctx));
                }
                want_retry(ssl, fd, r, ctx)?;
            }
            if is_client && SSL_get_verify_result(ssl) != VERIFY_OK {
                let code = SSL_get_verify_result(ssl);
                return Err(format!("{ctx}: certificate rejected (verify code {code})"));
            }
            SESSIONS
                .lock()
                .expect("tls session lock")
                .insert(fd, Session(ssl));
            Ok(())
        }
    }

    /// Look up the live session for an fd (loud when missing — never
    /// a null dereference, never silent).
    fn session(fd: c_int) -> Result<*mut Ssl, String> {
        SESSIONS
            .lock()
            .expect("tls session lock")
            .get(&fd)
            .map(|s| s.0)
            .ok_or_else(|| "tls: not a TLS connection".to_string())
    }

    /// One `SSL_write` (may short-write, like `net_send` — the Toto
    /// wrapper loops to full delivery). Empty input writes nothing
    /// (`Ok(0)`), mirroring the net convention.
    pub fn send(fd: c_int, buf: &[u8]) -> Result<usize, String> {
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() > c_int::MAX as usize {
            return Err("tls send: input too long".to_string());
        }
        let ssl = session(fd)?;
        ensure_blocking(fd)?;
        // SAFETY: `ssl` is live (removed only by a close this task
        // sequenced before/after); `buf` outlives the call.
        unsafe {
            loop {
                let r = SSL_write(ssl, buf.as_ptr() as *const c_void, buf.len() as c_int);
                if r > 0 {
                    return Ok(r as usize);
                }
                if r == 0 {
                    return Err(ossl_err("tls send"));
                }
                want_retry(ssl, fd, r, "tls send")?;
            }
        }
    }

    /// One `SSL_read` (up to `max` bytes). Empty is orderly shutdown
    /// (peer `close_notify` or TCP FIN), NOT an error — the same
    /// contract as `net_recv`.
    pub fn recv(fd: c_int, max: usize) -> Result<Vec<u8>, String> {
        if max == 0 {
            return Err("tls recv: invalid max (want >= 1)".to_string());
        }
        if max > c_int::MAX as usize {
            return Err("tls recv: max too large".to_string());
        }
        let ssl = session(fd)?;
        ensure_blocking(fd)?;
        // SAFETY: same contract as `send`; `out` is owned and sized.
        unsafe {
            let mut out = vec![0u8; max];
            loop {
                let r = SSL_read(ssl, out.as_mut_ptr() as *mut c_void, max as c_int);
                if r > 0 {
                    out.truncate(r as usize);
                    return Ok(out);
                }
                if r == 0 {
                    return Ok(Vec::new());
                }
                want_retry(ssl, fd, r, "tls recv")?;
            }
        }
    }

    /// Full teardown: `close_notify`, free the session, close the fd.
    /// A missing session is a loud `Err` (never a double-free, never
    /// silent); the fd is left untouched on that path so the caller
    /// can still `net_close` it.
    pub fn shutdown_close(fd: c_int) -> Result<(), String> {
        let ssl = session(fd)?;
        // SAFETY: `ssl` is live and removed from the table below
        // before freeing, so no later lookup can alias it.
        unsafe {
            // Best-effort `close_notify` so the peer sees orderly
            // shutdown (its `recv` returns empty, not an error).
            // `0` means "call again"; `<0` is ignored — the fd is
            // closing either way.
            for _ in 0..4 {
                let r = SSL_shutdown(ssl);
                if r != 0 {
                    break;
                }
            }
            SESSIONS.lock().expect("tls session lock").remove(&fd);
            SSL_free(ssl);
        }
        crate::netsys::close(fd)
    }

    /// Drop the session without I/O (the fd is closing elsewhere —
    /// the shared `net_close` path). Idempotent: missing sessions
    /// are fine here, since every close funnels through this.
    pub fn free_session(fd: c_int) {
        let ssl = SESSIONS.lock().expect("tls session lock").remove(&fd);
        if let Some(Session(s)) = ssl {
            // SAFETY: removed from the table under the lock, so this
            // thread owns the last reference; freeing is sound.
            unsafe {
                SSL_free(s);
            }
        }
    }
}
