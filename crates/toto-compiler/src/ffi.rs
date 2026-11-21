//! Foreign-function execution (Phase 5, Stage 3): marshal Toto values
//! across real C calls on Linux x86_64 (libc + libm), plus the
//! callback trampolines, queue, and runtime-provided helpers.
//!
//! Design notes (ADR-006, D6):
//! - Rooting covers the call duration only: strings cross as fresh
//!   NUL-copies owned by the call frame; nothing reachable after
//!   return except through explicit handles (`malloc`/`free`, with
//!   ownership documented per binding in `docs/ffi.md`).
//! - The runtime never scans foreign stacks: C values that must
//!   survive are copied into Toto values (strings, numbers) or named
//!   by opaque handles (raw pointers are unforgeable `Ptr` values).
//! - Callbacks are static trampolines that only enqueue
//!   `(slot, arg_ptr)` and return NULL. They never touch scheduler
//!   or GC state; the evaluator drains the queue after the foreign
//!   call returns and invokes the Toto function with the raw pointer.
//!   Up to 8 callbacks may be outstanding (static trampoline pool);
//!   the 9th registration fails cleanly.
//! - `panic = "unwind"` is assumed: boundary panics are caught with
//!   `catch_unwind` and converted to errors. Under `panic = "abort"`
//!   a boundary panic aborts the process (documented, not tested).

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};

use crate::diag::Diag;
use crate::hir::{HExternFn, Ty};
use crate::interp::Value;
use crate::span::Span;

// ---------------------------------------------------------------------------
// Linked C functions (real system libraries, Linux x86_64).
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void;
    fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
    fn strlen(s: *const c_char) -> usize;
    fn strcmp(s1: *const c_char, s2: *const c_char) -> c_int;
    fn strtod(s: *const c_char, endptr: *mut *mut c_char) -> c_double;
    fn strerror(errnum: c_int) -> *mut c_char;
}

// POSIX-only surface (file descriptors, `errno`, threads, sleep):
// present in libc on Linux/macOS, absent from MSVC. Unix-only by
// construction — naming one on Windows is a clean E0301 at check
// time via `is_linked` (never a link failure, never a panic). The
// Windows-compat decision (Win32/pthread layer + UCRT fd/errno
// semantics) is recorded as deferred Wave 0 work, not designed here.
#[cfg(unix)]
unsafe extern "C" {
    // `open` is variadic in libc (`open(path, flags, ...)`); the
    // two-argument form is valid without `O_CREAT`, and Rust admits
    // the variadic declaration directly.
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn pthread_create(
        thread: *mut c_void,
        attr: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: u64, retval: *mut *mut c_void) -> c_int;
    fn sleep(seconds: c_int) -> c_int;
}

// `errno` accessor: `__errno_location` on glibc (Linux), `__error`
// on macOS. Both return the calling thread's slot.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn __errno_location() -> *mut c_int;
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn __error() -> *mut c_int;
}
#[cfg(unix)]
fn errno_slot() -> *mut c_int {
    unsafe {
        #[cfg(target_os = "linux")]
        {
            __errno_location()
        }
        #[cfg(target_os = "macos")]
        {
            __error()
        }
    }
}

#[cfg(unix)]
#[link(name = "m")]
unsafe extern "C" {
    fn pow(x: c_double, y: c_double) -> c_double;
}
// No `m` on Windows: `pow` lives in the UCRT/MSVCRT default libs,
// so no link attribute is needed (or valid) there.
#[cfg(windows)]
unsafe extern "C" {
    fn pow(x: c_double, y: c_double) -> c_double;
}

// ---------------------------------------------------------------------------
// Runtime-provided helpers: defined here, declared by user programs in
// `unsafe extern` blocks (see `docs/ffi.md` and `stdlib/ffi.gzv`).
// Raw memory access has no Toto-native spelling, so these three tiny
// functions are the sanctioned bridge. All reads/writes are exact
// width; `i64` values round-trip bit-exact (addresses and pthread
// handles may print negative — never print them in goldens).
// ---------------------------------------------------------------------------

/// Read an `i32` (errno slots, `strcmp`/`memcmp`-style results).
#[unsafe(no_mangle)]
pub extern "C" fn toto_rt_read_i32(p: *const c_void) -> c_int {
    // SAFETY: the caller guarantees `p` points at a live `c_int`
    // (malloc'd buffers and `__errno_location` in the proof).
    unsafe { (p as *const c_int).read_unaligned() }
}

/// Read 8 bytes as an `i64` bit pattern (pthread handles, data words).
/// Fixed-width `i64`, not `c_long`: `long` is 32 bits on Windows
/// (LLP64), which would silently truncate. LP64 platforms are
/// unaffected (`c_long` == `i64` there).
#[unsafe(no_mangle)]
pub extern "C" fn toto_rt_read_i64(p: *const c_void) -> i64 {
    // SAFETY: same contract as `toto_rt_read_i32`, for 8-byte slots.
    unsafe { (p as *const i64).read_unaligned() }
}

/// Write an `i64` bit pattern (thread data buffers).
#[unsafe(no_mangle)]
pub extern "C" fn toto_rt_write_i64(p: *mut c_void, v: i64) {
    // SAFETY: the caller guarantees a live, writable 8-byte slot.
    unsafe {
        (p as *mut i64).write_unaligned(v);
    }
}

/// Close a file descriptor. `close(2)` cannot be named in a Toto
/// `unsafe extern` block (`close` is the reserved channel builtin),
/// so this alias is the sanctioned spelling; `stdlib/ffi.gzv` wraps
/// it as `c_close`. POSIX-only with the rest of the fd surface.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn toto_rt_close(fd: c_int) -> c_int {
    // SAFETY: `fd` is just a number to the kernel; a bogus fd fails
    // with EBADF, never UB.
    unsafe { close(fd) }
}

// `toto_rt_read_str` (counted foreign bytes to a Toto string, UTF-8
// validated) is implemented entirely in its call handler below,
// like `strerror`: validation can fail, and a C ABI cannot carry
// that failure, so there is no direct symbol to link.

// ---------------------------------------------------------------------------
// Callback trampolines + queue.
// ---------------------------------------------------------------------------

/// One outstanding callback registration: evaluator-assigned key plus
/// the trampoline slot index (identical; the slot IS the identity).
pub type CallbackKey = u64;

/// A fired callback awaiting drain: `(slot, raw arg pointer)`.
pub type FiredCallback = (u64, u64);

fn queue() -> &'static Mutex<VecDeque<FiredCallback>> {
    static QUEUE: OnceLock<Mutex<VecDeque<FiredCallback>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Trampoline pool occupancy: `true` while a registration is live
/// (claimed at marshal, released by the evaluator's drain or on
/// call failure).
fn pool() -> &'static Mutex<[bool; 8]> {
    static POOL: OnceLock<Mutex<[bool; 8]>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new([false; 8]))
}

/// `claim_slot` failure: all 8 trampoline slots are live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolExhausted;

/// Claim a trampoline slot, or fail cleanly when all 8 are live.
pub fn claim_slot() -> Result<u64, PoolExhausted> {
    let mut pool = pool().lock().expect("callback pool poisoned");
    for (i, live) in pool.iter_mut().enumerate() {
        if !*live {
            *live = true;
            return Ok(i as u64);
        }
    }
    Err(PoolExhausted)
}

/// Release a trampoline slot (after drain, or on call failure).
pub fn release_slot(slot: u64) {
    if let Ok(mut pool) = pool().lock()
        && let Some(live) = pool.get_mut(slot as usize)
    {
        *live = false;
    }
}

/// Shared fire path: enqueue `(slot, arg)` and return NULL. Never
/// touches evaluator state (D6 prohibition by construction: it holds
/// no interpreter, scheduler, or GC handle).
fn fire(slot: u64, arg: *mut c_void) -> *mut c_void {
    if let Ok(mut q) = queue().lock() {
        q.push_back((slot, arg as u64));
    }
    std::ptr::null_mut()
}

// Eight static trampolines, one per pool slot: the trampoline's
// identity IS the registration's identity, so concurrent callbacks
// can never misroute (each `pthread_create` gets its own). Written
// out (no macro pasting): eight four-line functions.
extern "C" fn trampoline_0(arg: *mut c_void) -> *mut c_void {
    fire(0, arg)
}
extern "C" fn trampoline_1(arg: *mut c_void) -> *mut c_void {
    fire(1, arg)
}
extern "C" fn trampoline_2(arg: *mut c_void) -> *mut c_void {
    fire(2, arg)
}
extern "C" fn trampoline_3(arg: *mut c_void) -> *mut c_void {
    fire(3, arg)
}
extern "C" fn trampoline_4(arg: *mut c_void) -> *mut c_void {
    fire(4, arg)
}
extern "C" fn trampoline_5(arg: *mut c_void) -> *mut c_void {
    fire(5, arg)
}
extern "C" fn trampoline_6(arg: *mut c_void) -> *mut c_void {
    fire(6, arg)
}
extern "C" fn trampoline_7(arg: *mut c_void) -> *mut c_void {
    fire(7, arg)
}

/// Trampoline address for a claimed slot: the C function pointer
/// actually passed to `pthread_create` et al. (A Toto closure is not
/// a C function pointer, so substitution here is inherent and
/// documented; DATA pointers pass through verbatim.)
pub fn trampoline_addr(slot: u64) -> usize {
    match slot {
        0 => trampoline_0 as *const () as usize,
        1 => trampoline_1 as *const () as usize,
        2 => trampoline_2 as *const () as usize,
        3 => trampoline_3 as *const () as usize,
        4 => trampoline_4 as *const () as usize,
        5 => trampoline_5 as *const () as usize,
        6 => trampoline_6 as *const () as usize,
        7 => trampoline_7 as *const () as usize,
        _ => unreachable!("slot checked at claim time"),
    }
}

/// Drain all fired callbacks (FIFO). Called by the evaluator after
/// every foreign call returns; invocation itself is evaluator-side.
pub fn drain_queue() -> Vec<FiredCallback> {
    queue()
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Linked-name allowlist: the checker rejects calls to declared-but-
// unlinked foreign functions statically (E0301), so a typo or a
// missing library can never become a runtime surprise.
// ---------------------------------------------------------------------------

/// Every foreign symbol this binary provides: real libc/libm plus the
/// runtime helpers defined above.
pub const LINKED: &[&str] = &[
    "malloc",
    "free",
    "memset",
    "memcmp",
    "strlen",
    "strcmp",
    "strtod",
    "open",
    "close",
    "strerror",
    "__errno_location",
    "pthread_create",
    "pthread_join",
    "sleep",
    "pow",
    "toto_rt_read_i32",
    "toto_rt_read_i64",
    "toto_rt_write_i64",
    "toto_rt_close",
    "toto_rt_read_str",
    "read",
    // Wave 1 networking (unix-only — see POSIX_ONLY below): socket
    // setup/teardown plus readiness-gated I/O. DNS resolves on the
    // blocking pool; everything else parks on the M:N poller.
    "toto_rt_net_resolve",
    "toto_rt_net_connect_ip",
    "toto_rt_net_listen",
    "toto_rt_net_accept",
    "toto_rt_net_send",
    "toto_rt_net_recv",
    "toto_rt_net_close",
    "toto_rt_net_udp_bind",
    "toto_rt_net_udp_send_to",
    "toto_rt_net_udp_recv_from",
    "toto_rt_net_udp_peer_host",
    "toto_rt_net_udp_peer_port",
    "toto_rt_net_last_error",
    // Wave 1 crypto (unix-only — same gate as networking): one-shot
    // OpenSSL/LibreSSL calls, no retained state, no poller.
    "toto_rt_crypto_sha256",
    "toto_rt_crypto_sha512",
    "toto_rt_crypto_hmac256",
    "toto_rt_crypto_hmac512",
    "toto_rt_crypto_aead_encrypt",
    "toto_rt_crypto_aead_decrypt",
    "toto_rt_crypto_sign_keygen",
    "toto_rt_crypto_sign_sign",
    "toto_rt_crypto_sign_verify",
    "toto_rt_crypto_random",
    "toto_rt_crypto_x509_verify",
    "toto_rt_crypto_last_error",
    "toto_rt_crypto_version",
    // Wave 2 Stage 1 TLS (unix-only — same gate as sockets/crypto):
    // handshake upgrades of already-connected TCP fds, record I/O,
    // and teardown, over the system libssl. Sessions are retained
    // under the fd (explicit handle rule in `tlslib`); Toto sees
    // only `int` handles and `Array<int>` bytes.
    "toto_rt_tls_connect",
    "toto_rt_tls_accept",
    "toto_rt_tls_send",
    "toto_rt_tls_recv",
    "toto_rt_tls_close",
    "toto_rt_tls_last_error",
    // Wave 1 compression (platform-independent pure-Rust gzip: no
    // cfg gate, works on every platform including Windows).
    "toto_rt_gzip_compress",
    "toto_rt_gzip_decompress",
    "toto_rt_gzip_last_error",
];

/// POSIX-only FFI names: threads, file descriptors, `errno`, sleep,
/// sockets, crypto, TLS. No MSVC equivalents; naming one on Windows
/// is a clean E0301 at check time (the dispatch arms are `cfg(unix)`
/// to match, so the `unreachable!("linkage checked above")` tail
/// stays sound).
#[cfg(windows)]
const POSIX_ONLY: &[&str] = &[
    "open",
    "close",
    "read",
    "__errno_location",
    "pthread_create",
    "pthread_join",
    "sleep",
    "toto_rt_close",
    "toto_rt_net_resolve",
    "toto_rt_net_connect_ip",
    "toto_rt_net_listen",
    "toto_rt_net_accept",
    "toto_rt_net_send",
    "toto_rt_net_recv",
    "toto_rt_net_close",
    "toto_rt_net_udp_bind",
    "toto_rt_net_udp_send_to",
    "toto_rt_net_udp_recv_from",
    "toto_rt_net_udp_peer_host",
    "toto_rt_net_udp_peer_port",
    "toto_rt_net_last_error",
    "toto_rt_crypto_sha256",
    "toto_rt_crypto_sha512",
    "toto_rt_crypto_hmac256",
    "toto_rt_crypto_hmac512",
    "toto_rt_crypto_aead_encrypt",
    "toto_rt_crypto_aead_decrypt",
    "toto_rt_crypto_sign_keygen",
    "toto_rt_crypto_sign_sign",
    "toto_rt_crypto_sign_verify",
    "toto_rt_crypto_random",
    "toto_rt_crypto_x509_verify",
    "toto_rt_crypto_last_error",
    "toto_rt_crypto_version",
    "toto_rt_tls_connect",
    "toto_rt_tls_accept",
    "toto_rt_tls_send",
    "toto_rt_tls_recv",
    "toto_rt_tls_close",
    "toto_rt_tls_last_error",
    "toto_rt_gzip_compress",
    "toto_rt_gzip_decompress",
    "toto_rt_gzip_last_error",
];

pub fn is_linked(name: &str) -> bool {
    #[cfg(windows)]
    if POSIX_ONLY.contains(&name) {
        return false;
    }
    LINKED.contains(&name)
}

/// Names actually linked in this binary (drives the E0301 message so
/// it never lists names the platform cannot provide).
pub fn linked_names() -> Vec<&'static str> {
    LINKED
        .iter()
        .copied()
        .filter(|_n| {
            #[cfg(windows)]
            if POSIX_ONLY.contains(_n) {
                return false;
            }
            true
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Marshal + dispatch (shared by the interpreter and the executor; both
// convert through `interp::Value`).
// ---------------------------------------------------------------------------

fn fail(code: &'static str, message: String, span: Span) -> (Diag, Vec<crate::interp::TraceFrame>) {
    (Diag::new(code, message, span, None), Vec::new())
}

/// Convert one Toto argument to its C width, with dynamic range
/// checks (E0210, like `to_int`). `need` comes from the checked
/// signature, so only coercible pairs arrive here.
fn marshal_int(
    v: i64,
    need: &Ty,
    span: Span,
) -> Result<i64, (Diag, Vec<crate::interp::TraceFrame>)> {
    let (lo, hi, width) = match need {
        Ty::CChar => (i64::from(i8::MIN), i64::from(i8::MAX), "c_char"),
        Ty::CInt => (i64::from(i32::MIN), i64::from(i32::MAX), "c_int"),
        Ty::CLong => (i64::MIN, i64::MAX, "c_long"),
        _ => unreachable!("checker guarantees integer targets"),
    };
    if v < lo || v > hi {
        return Err(fail(
            "E0210",
            format!("foreign argument {v} does not fit in {width}"),
            span,
        ));
    }
    Ok(v)
}

/// A claimed callback slot paired with the argument index whose
/// function value claimed it. Returned alongside the call result so
/// the evaluator can map slots to callables for its drain.
pub type ClaimedSlots = Vec<(usize, u64)>;

/// Call with worker-claimed slots (runtime executor path): `preclaimed`
/// maps argument index to an already-claimed slot, used verbatim for
/// the trampoline address. Nothing is claimed or released here — the
/// worker owns the full slot lifecycle (release at drain or on call
/// failure); this keeps slot ownership single-owner across threads.
pub fn call_preclaimed(
    name: &str,
    sig: &HExternFn,
    args: &[Value],
    preclaimed: &[(usize, u64)],
    span: Span,
) -> Result<Value, (Diag, Vec<crate::interp::TraceFrame>)> {
    if !is_linked(name) {
        return Err(fail(
            "E0301",
            format!("foreign function `{name}` is not linked in this binary"),
            span,
        ));
    }
    let mut owned: Vec<CString> = Vec::new();
    let mut claimed: ClaimedSlots = Vec::new();
    let pre: std::collections::HashMap<usize, u64> = preclaimed.iter().cloned().collect();
    call_inner(name, sig, args, span, &mut owned, &mut claimed, &pre)
}

/// Call a linked foreign function. `sig` is the checked signature;
/// `args` are already evaluated Toto values. Owned C-side buffers
/// (the string NUL-copies) live in this frame, so rooting covers
/// exactly the call duration and nothing escapes. Claimed slots stay
/// live for the evaluator's drain; on call failure they release here
/// (no thread can fire through a failed call).
pub fn call(
    name: &str,
    sig: &HExternFn,
    args: &[Value],
    span: Span,
) -> Result<(Value, ClaimedSlots), (Diag, Vec<crate::interp::TraceFrame>)> {
    if !is_linked(name) {
        // Unreachable through the checker (E0301), which consults the
        // same table; defense in depth for direct API users.
        return Err(fail(
            "E0301",
            format!("foreign function `{name}` is not linked in this binary"),
            span,
        ));
    }
    // NUL-copies owned here: alive for the call, dropped after.
    let mut owned: Vec<CString> = Vec::new();
    let mut claimed: ClaimedSlots = Vec::new();

    let result = call_inner(
        name,
        sig,
        args,
        span,
        &mut owned,
        &mut claimed,
        &std::collections::HashMap::new(),
    );

    if result.is_err() {
        for (_, s) in &claimed {
            release_slot(*s);
        }
    }
    result.map(|v| (v, claimed))
}

// `claimed`/`preclaimed` currently serve only the `pthread_create`
// arm (unix-only): on Windows no arm consumes them. `allow` is scoped
// to Windows so a genuinely dead parameter on unix still warns.
#[cfg_attr(windows, allow(unused_variables))]
fn call_inner(
    name: &str,
    sig: &HExternFn,
    args: &[Value],
    span: Span,
    owned: &mut Vec<CString>,
    claimed: &mut ClaimedSlots,
    preclaimed: &std::collections::HashMap<usize, u64>,
) -> Result<Value, (Diag, Vec<crate::interp::TraceFrame>)> {
    let v = match name {
        "malloc" => {
            let n = size_arg(&args[0], span)?;
            let p = unsafe { malloc(n) };
            Ok(Value::Ptr(p as u64))
        }
        "free" => {
            let p = ptr_arg(&args[0], span)? as *mut c_void;
            unsafe { free(p) };
            Ok(Value::Unit)
        }
        "memset" => {
            let s = ptr_arg(&args[0], span)? as *mut c_void;
            let c = int_arg(&args[1], &sig.params[1].1, span)? as c_int;
            let n = size_arg(&args[2], span)?;
            let r = unsafe { memset(s, c, n) };
            Ok(Value::Ptr(r as u64))
        }
        "memcmp" => {
            let a = ptr_arg(&args[0], span)? as *const c_void;
            let b = ptr_arg(&args[1], span)? as *const c_void;
            let n = size_arg(&args[2], span)?;
            Ok(Value::Int(unsafe { memcmp(a, b, n) } as i64))
        }
        "strlen" => {
            let s = str_arg(&args[0], owned, span)? as *const c_char;
            Ok(Value::Int(unsafe { strlen(s) } as i64))
        }
        "strcmp" => {
            let a = str_arg(&args[0], owned, span)? as *const c_char;
            let b = str_arg(&args[1], owned, span)? as *const c_char;
            Ok(Value::Int(unsafe { strcmp(a, b) } as i64))
        }
        "strtod" => {
            let s = str_arg(&args[0], owned, span)? as *const c_char;
            // `None` (the null pointer) asks `strtod` to drop the end
            // pointer. Out-pointers need a protocol v1 does not
            // provide, so real pointers are rejected statically here.
            let end: *mut *mut c_char = match &args[1] {
                Value::None => std::ptr::null_mut(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot pass {} as the `strtod` end pointer (pass `None`)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            Ok(Value::Float(unsafe { strtod(s, end) }))
        }
        #[cfg(unix)]
        "open" => {
            let p = str_arg(&args[0], owned, span)? as *const c_char;
            let flags = int_arg(&args[1], &sig.params[1].1, span)? as c_int;
            // `errno` is thread-local and this call runs on a pool
            // thread: a later `errno_value()` call may run on a
            // different pool thread (or after another task's failing
            // call) and read the wrong thread's `errno`. Capture it
            // here, on this thread, immediately after the syscall, and
            // report it in-band: success is the fd (>= 0), failure is
            // `-errno` (never bare -1). Callers negate back and feed
            // the number to `strerror`, which needs no `errno` state.
            let fd = unsafe { open(p, flags) };
            if fd < 0 {
                let e = unsafe { *errno_slot() };
                Ok(Value::Int(-(e as i64)))
            } else {
                Ok(Value::Int(fd as i64))
            }
        }
        #[cfg(unix)]
        "close" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            Ok(Value::Int(unsafe { close(fd) } as i64))
        }
        #[cfg(unix)]
        "read" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let buf = ptr_arg(&args[1], span)? as *mut c_void;
            let n = size_arg(&args[2], span)?;
            // Same in-band `-errno` discipline as `open`: `errno`
            // is thread-local across the pool, so capture it here.
            // Success is the byte count (0 is EOF), never negative.
            let r = unsafe { read(fd, buf, n) };
            if r < 0 {
                let e = unsafe { *errno_slot() };
                Ok(Value::Int(-(e as i64)))
            } else {
                Ok(Value::Int(r as i64))
            }
        }
        #[cfg(unix)]
        "toto_rt_close" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let rc = toto_rt_close(fd);
            if rc < 0 {
                let e = unsafe { *errno_slot() };
                Ok(Value::Int(-(e as i64)))
            } else {
                Ok(Value::Int(0))
            }
        }
        "toto_rt_read_str" => {
            let p = ptr_arg(&args[0], span)? as *const u8;
            let n = size_arg(&args[1], span)?;
            if p.is_null() {
                return Err(fail(
                    "E0308",
                    "cannot read a string from the null pointer".to_string(),
                    span,
                ));
            }
            // SAFETY: the caller guarantees `p` points at `n` live
            // readable bytes (a just-`read` malloc'd buffer in the
            // proof); the copy below bounds the read at `n`.
            let bytes = unsafe { std::slice::from_raw_parts(p, n) };
            match std::str::from_utf8(bytes) {
                Ok(s) => Ok(Value::Str(Rc::new(s.to_string()))),
                Err(_) => Err(fail(
                    "E0210",
                    "foreign bytes are not valid UTF-8".to_string(),
                    span,
                )),
            }
        }
        "strerror" => {
            let e = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let p = unsafe { strerror(e) };
            Ok(c_string_to_value(p, span)?)
        }
        #[cfg(unix)]
        "__errno_location" => {
            // Historical name; on macOS this is the `__error` slot.
            let p = errno_slot();
            Ok(Value::Ptr(p as u64))
        }
        #[cfg(unix)]
        "pthread_create" => {
            let thread = ptr_arg(&args[0], span)? as *mut c_void;
            let attr = match &args[1] {
                Value::None => std::ptr::null(),
                Value::Ptr(p) => *p as *const c_void,
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot pass {} as pthread attributes (use `None` or a pointer)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            // The function-typed parameter position comes from the
            // signature (v1: only `pthread_create` has one, at index
            // 2); the checked call guarantees the value there is a
            // function.
            let fidx = sig
                .params
                .iter()
                .position(|(_, t)| matches!(t, Ty::Fn(..)))
                .unwrap_or(2);
            let start = fn_arg(&args[fidx], fidx, claimed, preclaimed, span)?;
            let f: extern "C" fn(*mut c_void) -> *mut c_void =
                unsafe { std::mem::transmute(start) };
            let arg = ptr_arg(&args[3], span)? as *mut c_void;
            Ok(Value::Int(
                unsafe { pthread_create(thread, attr, f, arg) } as i64
            ))
        }
        #[cfg(unix)]
        "pthread_join" => {
            let thread = int_arg(&args[0], &sig.params[0].1, span)? as u64;
            let retval = match &args[1] {
                Value::None => std::ptr::null_mut(),
                Value::Ptr(p) => *p as *mut *mut c_void,
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot pass {} as pthread join retval (use `None` or a pointer)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            Ok(Value::Int(unsafe { pthread_join(thread, retval) } as i64))
        }
        #[cfg(unix)]
        "sleep" => {
            let s = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            Ok(Value::Int(unsafe { sleep(s) } as i64))
        }
        "pow" => {
            let (x, y) = match (&args[0], &args[1]) {
                (Value::Float(x), Value::Float(y)) => (*x, *y),
                _ => {
                    return Err(fail(
                        "E0308",
                        "`pow` needs two floats (convert with `to_float`)".to_string(),
                        span,
                    ));
                }
            };
            Ok(Value::Float(unsafe { pow(x, y) }))
        }
        // Wave 1 networking (interpreter path: blocking syscalls — the
        // tree-walker has no scheduler to integrate with, so it is the
        // reference semantics; the runtime executor parks instead).
        // All arms are unix-only (see POSIX_ONLY).
        //
        // Error discipline (uniform): scalar ops return the fd/count or
        // -1; array ops return the payload or an empty array. Either
        // way the message lands in the call-duration error slot read
        // by `toto_rt_net_last_error` ("" = clean). The `gzv.std/net`
        // wrappers turn this into `Result<_, string>`.
        #[cfg(unix)]
        "toto_rt_net_resolve" => {
            let host = match &args[0] {
                Value::Str(s) => s.to_string(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot resolve {} (need a hostname string)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            // Comma-joined (see the `net.gzv` decl): IPv6 uses colons,
            // never commas, so `split(joined, ",")` recovers the list.
            match crate::netsys::resolve_blocking(&host) {
                Ok(addrs) => {
                    set_net_error(None);
                    Ok(Value::Str(std::rc::Rc::new(addrs.join(","))))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Str(std::rc::Rc::new(String::new())))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_connect_ip" => {
            let ip = match &args[0] {
                Value::Str(s) => s.to_string(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot connect to {} (need an IP string)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            let port = int_arg(&args[1], &sig.params[1].1, span)? as u16;
            match crate::netsys::connect_ip_blocking(&ip, port) {
                Ok(fd) => {
                    set_net_error(None);
                    Ok(Value::Int(fd as i64))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_listen" => {
            let port = int_arg(&args[0], &sig.params[0].1, span)? as u16;
            match crate::netsys::listen_loopback(port) {
                Ok(fd) => {
                    set_net_error(None);
                    Ok(Value::Int(fd as i64))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_accept" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            match crate::netsys::accept_blocking(fd) {
                Ok(c) => {
                    set_net_error(None);
                    Ok(Value::Int(c as i64))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_send" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let buf = bytes_arg(&args[1], span)?;
            match crate::netsys::send_all_blocking(fd, &buf) {
                Ok(n) => {
                    set_net_error(None);
                    Ok(Value::Int(n as i64))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_recv" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let max = size_arg(&args[1], span)?;
            match crate::netsys::recv_blocking(fd, max) {
                Ok(bytes) => {
                    set_net_error(None);
                    Ok(bytes_value(bytes))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_close" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            crate::netsys::forget_peer(fd);
            // A TLS session names this fd: free it first so a plain
            // `net_close` of a TLS connection never leaks the `SSL*`
            // nor leaves a stale entry for a recycled fd number.
            crate::tlslib::free_session(fd);
            match crate::netsys::close(fd) {
                Ok(()) => {
                    set_net_error(None);
                    Ok(Value::Int(0))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_udp_bind" => {
            let port = int_arg(&args[0], &sig.params[0].1, span)? as u16;
            match crate::netsys::bind_udp_loopback(port) {
                Ok(fd) => {
                    set_net_error(None);
                    Ok(Value::Int(fd as i64))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_udp_send_to" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let buf = bytes_arg(&args[1], span)?;
            let ip = match &args[2] {
                Value::Str(s) => s.to_string(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!("cannot send to {} (need an IP string)", other.type_name()),
                        span,
                    ));
                }
            };
            let port = int_arg(&args[3], &sig.params[3].1, span)? as u16;
            match crate::netsys::udp_send_to(fd, &buf, &ip, port) {
                Ok(n) => {
                    set_net_error(None);
                    Ok(Value::Int(n as i64))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_udp_recv_from" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let max = size_arg(&args[1], span)?;
            match crate::netsys::udp_recv_from(fd, max) {
                Ok((bytes, host, port)) => {
                    crate::netsys::record_peer(fd, host, port);
                    set_net_error(None);
                    Ok(bytes_value(bytes))
                }
                Err(e) => {
                    set_net_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_net_udp_peer_host" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            match crate::netsys::lookup_peer(fd) {
                Some((host, _)) => Ok(Value::Str(std::rc::Rc::new(host))),
                None => Ok(Value::Str(std::rc::Rc::new(String::new()))),
            }
        }
        #[cfg(unix)]
        "toto_rt_net_udp_peer_port" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            match crate::netsys::lookup_peer(fd) {
                Some((_, port)) => Ok(Value::Int(port as i64)),
                None => Ok(Value::Int(0)),
            }
        }
        #[cfg(unix)]
        "toto_rt_net_last_error" => Ok(Value::Str(std::rc::Rc::new(take_net_error()))),
        // Wave 1 crypto (interpreter path: one-shot OpenSSL calls —
        // no scheduler to integrate with, and none needed: nothing
        // here parks. Bytes cross as `Array<int>` (elements 0–255,
        // E0308 otherwise); scalar ops return the payload length or
        // -1; array ops return the payload or an empty array. Either
        // way the message lands in the call-duration error slot read
        // by `toto_rt_crypto_last_error` ("" = clean). `verify`-style
        // ops return 1/0 (valid/invalid, slot clean) or -1 + slot on
        // malformed input. The `gzv.std/crypto` wrappers turn this
        // into `Result<_, string>`.
        #[cfg(unix)]
        "toto_rt_crypto_sha256" => {
            let buf = bytes_arg(&args[0], span)?;
            match crate::cryptolib::sha256(&buf) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_sha512" => {
            let buf = bytes_arg(&args[0], span)?;
            match crate::cryptolib::sha512(&buf) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_hmac256" => {
            let key = bytes_arg(&args[0], span)?;
            let data = bytes_arg(&args[1], span)?;
            match crate::cryptolib::hmac256(&key, &data) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_hmac512" => {
            let key = bytes_arg(&args[0], span)?;
            let data = bytes_arg(&args[1], span)?;
            match crate::cryptolib::hmac512(&key, &data) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_aead_encrypt" => {
            let key = bytes_arg(&args[0], span)?;
            let nonce = bytes_arg(&args[1], span)?;
            let pt = bytes_arg(&args[2], span)?;
            let aad = bytes_arg(&args[3], span)?;
            match crate::cryptolib::aead_encrypt(&key, &nonce, &pt, &aad) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_aead_decrypt" => {
            let key = bytes_arg(&args[0], span)?;
            let nonce = bytes_arg(&args[1], span)?;
            let cttag = bytes_arg(&args[2], span)?;
            let aad = bytes_arg(&args[3], span)?;
            match crate::cryptolib::aead_decrypt(&key, &nonce, &cttag, &aad) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_sign_keygen" => match crate::cryptolib::sign_keygen() {
            Ok(out) => {
                set_crypto_error(None);
                Ok(bytes_value(out))
            }
            Err(e) => {
                set_crypto_error(Some(e));
                Ok(bytes_value(Vec::new()))
            }
        },
        #[cfg(unix)]
        "toto_rt_crypto_sign_sign" => {
            let secret = bytes_arg(&args[0], span)?;
            let msg = bytes_arg(&args[1], span)?;
            match crate::cryptolib::sign_sign(&secret, &msg) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_sign_verify" => {
            let public = bytes_arg(&args[0], span)?;
            let msg = bytes_arg(&args[1], span)?;
            let sig = bytes_arg(&args[2], span)?;
            match crate::cryptolib::sign_verify(&public, &msg, &sig) {
                Ok(true) => {
                    set_crypto_error(None);
                    Ok(Value::Int(1))
                }
                Ok(false) => {
                    set_crypto_error(None);
                    Ok(Value::Int(0))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_random" => {
            let n = int_arg(&args[0], &sig.params[0].1, span)?;
            if !(1..=65536).contains(&n) {
                set_crypto_error(Some(format!("random: need 1-65536 bytes, got {n}")));
                return Ok(bytes_value(Vec::new()));
            }
            match crate::cryptolib::random_bytes(n as usize) {
                Ok(out) => {
                    set_crypto_error(None);
                    Ok(bytes_value(out))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_x509_verify" => {
            let leaf = match &args[0] {
                Value::Str(s) => s.to_string(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!("cannot verify {} (need a PEM string)", other.type_name()),
                        span,
                    ));
                }
            };
            let ca = match &args[1] {
                Value::Str(s) => s.to_string(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot verify against {} (need a PEM string)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            match crate::cryptolib::x509_verify(&leaf, &ca) {
                Ok(true) => {
                    set_crypto_error(None);
                    Ok(Value::Int(1))
                }
                Ok(false) => {
                    set_crypto_error(None);
                    Ok(Value::Int(0))
                }
                Err(e) => {
                    set_crypto_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_crypto_last_error" => Ok(Value::Str(std::rc::Rc::new(take_crypto_error()))),
        #[cfg(unix)]
        "toto_rt_crypto_version" => Ok(Value::Str(std::rc::Rc::new(
            crate::cryptolib::openssl_version(),
        ))),
        // Wave 2 Stage 1 TLS (interpreter path: blocking libssl
        // calls — the tree-walker has no scheduler to integrate
        // with, so blocking calls are the reference semantics; the
        // runtime executor parks these on the blocking pool instead.
        // Same error discipline as net/crypto (scalar ops return the
        // fd/count or -1; array ops return the payload or an empty
        // array; the message lands in the call-duration slot read by
        // `toto_rt_tls_last_error`). The `gzv.std/tls` wrappers turn
        // this into `Result<_, string>`.
        #[cfg(unix)]
        "toto_rt_tls_connect" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let ca = match &args[1] {
                Value::Str(s) => s.to_string(),
                other => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot verify against {} (need a CA PEM string)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
            };
            match crate::tlslib::client_connect(fd, &ca) {
                Ok(()) => {
                    set_tls_error(None);
                    Ok(Value::Int(fd as i64))
                }
                Err(e) => {
                    set_tls_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_tls_accept" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let (cert, key) = match (&args[1], &args[2]) {
                (Value::Str(c), Value::Str(k)) => (c.to_string(), k.to_string()),
                (other, _) if !matches!(other, Value::Str(_)) => {
                    return Err(fail(
                        "E0308",
                        format!(
                            "cannot serve {} (need a certificate PEM string)",
                            other.type_name()
                        ),
                        span,
                    ));
                }
                (_, other) => {
                    return Err(fail(
                        "E0308",
                        format!("cannot use {} (need a key PEM string)", other.type_name()),
                        span,
                    ));
                }
            };
            match crate::tlslib::server_accept(fd, &cert, &key) {
                Ok(()) => {
                    set_tls_error(None);
                    Ok(Value::Int(fd as i64))
                }
                Err(e) => {
                    set_tls_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_tls_send" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let buf = bytes_arg(&args[1], span)?;
            match crate::tlslib::send(fd, &buf) {
                Ok(n) => {
                    set_tls_error(None);
                    Ok(Value::Int(n as i64))
                }
                Err(e) => {
                    set_tls_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_tls_recv" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            let max = size_arg(&args[1], span)?;
            match crate::tlslib::recv(fd, max) {
                Ok(bytes) => {
                    set_tls_error(None);
                    Ok(bytes_value(bytes))
                }
                Err(e) => {
                    set_tls_error(Some(e));
                    Ok(bytes_value(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_tls_close" => {
            let fd = int_arg(&args[0], &sig.params[0].1, span)? as c_int;
            crate::netsys::forget_peer(fd);
            match crate::tlslib::shutdown_close(fd) {
                Ok(()) => {
                    set_tls_error(None);
                    Ok(Value::Int(0))
                }
                Err(e) => {
                    set_tls_error(Some(e));
                    Ok(Value::Int(-1))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_tls_last_error" => Ok(Value::Str(std::rc::Rc::new(take_tls_error()))),
        // Wave 1 compression (interpreter path: one-shot gzip over
        // the pure-Rust backend — same call-duration error-slot
        // discipline as net/crypto; unix-gated like them).
        #[cfg(unix)]
        "toto_rt_gzip_compress" => {
            let buf = bytes_arg_gzip(&args[0], span)?;
            match crate::compress::gzip_compress(&buf) {
                Ok(out) => {
                    set_gzip_error(None);
                    Ok(bytes_value_gzip(out))
                }
                Err(e) => {
                    set_gzip_error(Some(e));
                    Ok(bytes_value_gzip(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_gzip_decompress" => {
            let buf = bytes_arg_gzip(&args[0], span)?;
            match crate::compress::gzip_decompress(&buf) {
                Ok(out) => {
                    set_gzip_error(None);
                    Ok(bytes_value_gzip(out))
                }
                Err(e) => {
                    set_gzip_error(Some(e));
                    Ok(bytes_value_gzip(Vec::new()))
                }
            }
        }
        #[cfg(unix)]
        "toto_rt_gzip_last_error" => Ok(Value::Str(std::rc::Rc::new(take_gzip_error()))),
        "toto_rt_read_i32" => {
            let p = ptr_arg(&args[0], span)? as *const c_void;
            Ok(Value::Int(toto_rt_read_i32(p) as i64))
        }
        "toto_rt_read_i64" => {
            let p = ptr_arg(&args[0], span)? as *const c_void;
            Ok(Value::Int(toto_rt_read_i64(p)))
        }
        "toto_rt_write_i64" => {
            let p = ptr_arg(&args[0], span)? as *mut c_void;
            let v = int_arg(&args[1], &sig.params[1].1, span)?;
            toto_rt_write_i64(p, v);
            Ok(Value::Unit)
        }
        _ => unreachable!("linkage checked above"),
    };
    // A `-> string` declaration promises the C side handed back a
    // NUL-terminated UTF-8 message this call (today: `strerror`
    // only); anything else is a binding bug, surfaced as a clean
    // E0308 rather than a mistyped value flowing downstream.
    let v = v?;
    if sig.ret == Ty::String && !matches!(v, Value::Str(_)) {
        return Err(fail(
            "E0308",
            format!(
                "foreign function `{name}` is declared `-> string` but did not return a string"
            ),
            span,
        ));
    }
    if matches!(sig.ret, Ty::Array(_)) && !matches!(v, Value::Array(_)) {
        return Err(fail(
            "E0308",
            format!(
                "foreign function `{name}` is declared `-> Array<int>` but did not return an array"
            ),
            span,
        ));
    }
    Ok(v)
}

/// Toto `int` to a C integer width (dynamic range check).
fn int_arg(
    v: &Value,
    need: &Ty,
    span: Span,
) -> Result<i64, (Diag, Vec<crate::interp::TraceFrame>)> {
    match v {
        Value::Int(n) => marshal_int(*n, need, span),
        other => Err(fail(
            "E0308",
            format!("cannot pass {} where {need} is expected", other.type_name()),
            span,
        )),
    }
}

/// Toto pointer (or string-coerced NUL-copy handled by `str_arg`) to a
/// raw address. `Value::None` is the null pointer.
fn ptr_arg(v: &Value, span: Span) -> Result<u64, (Diag, Vec<crate::interp::TraceFrame>)> {
    match v {
        Value::Ptr(p) => Ok(*p),
        Value::None => Ok(0),
        other => Err(fail(
            "E0308",
            format!(
                "cannot pass {} as a pointer (pointers are opaque; allocate with `malloc`)",
                other.type_name()
            ),
            span,
        )),
    }
}

/// A byte size for `malloc`/`memset`/`memcmp`: must be non-negative
/// (a negative size wrapping to `usize` would be heap corruption, so
/// it is a clean E0210 instead).
fn size_arg(v: &Value, span: Span) -> Result<usize, (Diag, Vec<crate::interp::TraceFrame>)> {
    match v {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        Value::Int(n) => Err(fail(
            "E0210",
            format!("foreign size must be non-negative, got {n}"),
            span,
        )),
        other => Err(fail(
            "E0308",
            format!("cannot pass {} as a byte size", other.type_name()),
            span,
        )),
    }
}

/// Toto string to a call-duration NUL-copy. Interior NUL bytes are a
/// clean E0210 (C strings cannot represent them).
fn str_arg(
    v: &Value,
    owned: &mut Vec<CString>,
    span: Span,
) -> Result<*const c_char, (Diag, Vec<crate::interp::TraceFrame>)> {
    match v {
        Value::Str(s) => match CString::new(s.as_str()) {
            Ok(c) => {
                let p = c.as_ptr();
                owned.push(c);
                Ok(p)
            }
            Err(_) => Err(fail(
                "E0210",
                "foreign string contains an interior NUL byte".to_string(),
                span,
            )),
        },
        other => Err(fail(
            "E0308",
            format!(
                "cannot pass {} where *c_char is expected",
                other.type_name()
            ),
            span,
        )),
    }
}

// Interpreter-side network error slot (Wave 1): the runtime
// executor keeps this per task (see `Slot.net_error`); the
// single-threaded tree-walker keeps it in a thread-local.
// Every net arm sets it (`None` = clean); `toto_rt_net_last_error`
// reads it. Never retains heap references — plain strings only.
#[cfg(unix)]
thread_local! {
    static NET_LAST_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn set_net_error(msg: Option<String>) {
    NET_LAST_ERROR.with(|s| *s.borrow_mut() = msg);
}

#[cfg(unix)]
fn take_net_error() -> String {
    NET_LAST_ERROR.with(|s| s.borrow_mut().take().unwrap_or_default())
}

// Interpreter-side crypto error slot (Wave 1): same discipline as
// the net slot, kept separate so interleaved net/crypto calls in one
// task never clobber each other's `last_error`.
#[cfg(unix)]
thread_local! {
    static CRYPTO_LAST_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn set_crypto_error(msg: Option<String>) {
    CRYPTO_LAST_ERROR.with(|s| *s.borrow_mut() = msg);
}

#[cfg(unix)]
fn take_crypto_error() -> String {
    CRYPTO_LAST_ERROR.with(|s| s.borrow_mut().take().unwrap_or_default())
}

// Interpreter-side TLS error slot (Wave 2, Stage 1): same
// discipline as the net/crypto slots, kept separate so interleaved
// net/TLS calls in one task never clobber each other's
// `last_error`.
#[cfg(unix)]
thread_local! {
    static TLS_LAST_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn set_tls_error(msg: Option<String>) {
    TLS_LAST_ERROR.with(|s| *s.borrow_mut() = msg);
}

#[cfg(unix)]
fn take_tls_error() -> String {
    TLS_LAST_ERROR.with(|s| s.borrow_mut().take().unwrap_or_default())
}

// Interpreter-side gzip error slot (Wave 1): same discipline as
// the net/crypto slots. (The backend is pure Rust and could run
// anywhere; it rides the unix gate for uniformity with the other
// Wave 1 surfaces — lifting it is cfg-only. Windows gets E0301.)
#[cfg(unix)]
thread_local! {
    static GZIP_LAST_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn set_gzip_error(msg: Option<String>) {
    GZIP_LAST_ERROR.with(|s| *s.borrow_mut() = msg);
}

#[cfg(unix)]
fn take_gzip_error() -> String {
    GZIP_LAST_ERROR.with(|s| s.borrow_mut().take().unwrap_or_default())
}

/// `Array<int>` to call-duration bytes for gzip (elements 0–255,
/// E0308 otherwise — same rule as the socket carve-out).
#[cfg(unix)]
fn bytes_arg_gzip(
    v: &Value,
    span: Span,
) -> Result<Vec<u8>, (Diag, Vec<crate::interp::TraceFrame>)> {
    match v {
        Value::Array(elems) => {
            let mut out = Vec::with_capacity(elems.borrow().len());
            for e in elems.borrow().iter() {
                match e {
                    Value::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                    other => {
                        return Err(fail(
                            "E0308",
                            format!(
                                "cannot pass {} as gzip bytes (elements must be ints 0-255)",
                                other.type_name()
                            ),
                            span,
                        ));
                    }
                }
            }
            Ok(out)
        }
        other => Err(fail(
            "E0308",
            format!(
                "cannot pass {} as gzip bytes (need `Array<int>` of 0-255)",
                other.type_name()
            ),
            span,
        )),
    }
}

/// Owned bytes back to a fresh Toto `Array<int>`.
#[cfg(unix)]
fn bytes_value_gzip(bytes: Vec<u8>) -> Value {
    Value::Array(std::rc::Rc::new(std::cell::RefCell::new(
        bytes.into_iter().map(|b| Value::Int(b as i64)).collect(),
    )))
}

/// A Toto `Array<int>` to call-duration bytes (Wave 1 amendment
/// 2026-11-19, user-approved): every element must be 0–255; anything
/// else (non-int elements, out-of-range values) is a clean E0308.
/// The copy lives only for the call — never retained past return.
#[cfg(unix)]
fn bytes_arg(v: &Value, span: Span) -> Result<Vec<u8>, (Diag, Vec<crate::interp::TraceFrame>)> {
    match v {
        Value::Array(elems) => {
            let mut out = Vec::with_capacity(elems.borrow().len());
            for e in elems.borrow().iter() {
                match e {
                    Value::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                    other => {
                        return Err(fail(
                            "E0308",
                            format!(
                                "cannot pass {} as socket bytes (elements must be ints 0-255)",
                                other.type_name()
                            ),
                            span,
                        ));
                    }
                }
            }
            Ok(out)
        }
        other => Err(fail(
            "E0308",
            format!(
                "cannot pass {} as socket bytes (need `Array<int>` of 0-255)",
                other.type_name()
            ),
            span,
        )),
    }
}

/// Owned bytes back to a fresh Toto `Array<int>`.
#[cfg(unix)]
fn bytes_value(bytes: Vec<u8>) -> Value {
    Value::Array(std::rc::Rc::new(std::cell::RefCell::new(
        bytes.into_iter().map(|b| Value::Int(b as i64)).collect(),
    )))
}

/// A Toto function value to a C function pointer: claim a trampoline
/// slot and hand out its address. Slots stay live for the evaluator's
/// drain (which maps them to callables and releases them); on call
/// failure `call` releases everything it claimed. Unix-only caller
/// (`pthread_create` arm); no Windows caller yet.
#[cfg_attr(windows, allow(dead_code))]
fn fn_arg(
    v: &Value,
    arg_idx: usize,
    claimed: &mut ClaimedSlots,
    preclaimed: &std::collections::HashMap<usize, u64>,
    span: Span,
) -> Result<usize, (Diag, Vec<crate::interp::TraceFrame>)> {
    // Worker-claimed slots (executor path) pass through verbatim.
    if let Some(s) = preclaimed.get(&arg_idx) {
        return Ok(trampoline_addr(*s));
    }
    match v {
        Value::Func(_) | Value::Closure(_) => match claim_slot() {
            Ok(s) => {
                claimed.push((arg_idx, s));
                Ok(trampoline_addr(s))
            }
            Err(PoolExhausted) => Err(fail(
                "E0217",
                "all 8 callback slots are live; drain running callbacks (join threads) before registering more".to_string(),
                span,
            )),
        },
        other => Err(fail(
            "E0308",
            format!(
                "cannot pass {} as a callback (need a `fn` value with the declared type)",
                other.type_name()
            ),
            span,
        )),
    }
}

/// A C string back to a Toto string: copies, validates UTF-8, and
/// rejects invalid bytes cleanly (never silent corruption).
fn c_string_to_value(
    p: *mut c_char,
    span: Span,
) -> Result<Value, (Diag, Vec<crate::interp::TraceFrame>)> {
    if p.is_null() {
        return Err(fail(
            "E0308",
            "foreign function returned NULL where a string was expected".to_string(),
            span,
        ));
    }
    // SAFETY: linked C functions in the proof return valid NUL-
    // terminated strings (`strerror`); a garbage pointer would be a
    // binding bug, and the byte walk below bounds the read at the
    // first NUL.
    let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(Value::Str(Rc::new(s.to_string()))),
        Err(_) => Err(fail(
            "E0210",
            "foreign string is not valid UTF-8".to_string(),
            span,
        )),
    }
}
