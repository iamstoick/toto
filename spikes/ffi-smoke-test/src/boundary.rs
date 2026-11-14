//! Safe boundary over the C test file. Raw pointers appear only as
//! call-scoped borrows; nothing in this API retains one.

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::sync::Mutex;

use spike_1_gc_scheduler::heap::{GcRef, GcReport, Heap};
use spike_1_gc_scheduler::roots::ShadowStack;

#[link(name = "toto_ffi_test")]
unsafe extern "C" {
    fn ffi_add(a: i32, b: i32) -> i32;
    fn ffi_read_first(p: *const u8, len: usize) -> u8;
    fn ffi_copy_greet(
        input: *const c_char,
        in_len: usize,
        out: *mut c_char,
        out_cap: usize,
    ) -> usize;
    fn ffi_bad_bytes(out_len: *mut usize) -> *const u8;
    fn ffi_emit_all(cb: extern "C" fn(i32, *const c_char));
    fn ffi_fallible(x: i32) -> i32;
    fn ffi_last_error() -> *const c_char;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FfiError {
    /// C produced bytes that are not valid UTF-8.
    InvalidUtf8,
    /// Handle unknown or already released.
    DeadHandle(u32),
    /// C error slot: code + message.
    CError { code: i32, message: String },
    /// A Rust panic was caught at the boundary (carries the code).
    PanicCaught(i32),
}

pub const ERR_C_FAILURE: i32 = -1;
pub const ERR_DIV_ZERO: i32 = -100;

/// Plain C call: arguments and return cross by value.
pub fn add(a: i32, b: i32) -> i32 {
    // SAFETY: linked by build.rs; pure function, no aliasing.
    unsafe { ffi_add(a, b) }
}

/// Read the first byte through a caller-owned borrow. The borrow
/// (backed by a rooted heap object in tests) must outlive the call;
/// C only reads during the call and retains nothing.
pub fn read_first_byte(bytes: &[u8]) -> u8 {
    // SAFETY: `bytes` is alive for the call; C reads `len` bytes max.
    unsafe { ffi_read_first(bytes.as_ptr(), bytes.len()) }
}

/// Copy a length-prefixed Rust string for C (bounded, NUL-terminated
/// on the C side), then validate the bytes coming back as UTF-8.
pub fn greet(input: &str) -> Result<String, FfiError> {
    let mut out = [0 as c_char; 64];
    // SAFETY: `input` is alive for the call; `out` has capacity 64.
    let n = unsafe {
        ffi_copy_greet(
            input.as_ptr() as *const c_char,
            input.len(),
            out.as_mut_ptr(),
            out.len(),
        )
    };
    // SAFETY: C wrote exactly `n <= 63` bytes into `out`, NUL after.
    let bytes = unsafe { std::slice::from_raw_parts(out.as_ptr() as *const u8, n) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| FfiError::InvalidUtf8)
}

/// Read C-owned bytes and validate them as UTF-8. The invalid-UTF-8
/// probe must come back as `Err`, never as a panic or UB.
pub fn bad_bytes() -> Result<String, FfiError> {
    let mut len = 0usize;
    // SAFETY: static storage on the C side; `len` out-param written.
    let p = unsafe { ffi_bad_bytes(&mut len) };
    // SAFETY: `len` bytes of static storage, copied out immediately.
    let bytes = unsafe { std::slice::from_raw_parts(p, len) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| FfiError::InvalidUtf8)
}

// -- callback shape: static fn + plain queue, no re-entry ----------

static QUEUE: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

/// Static `extern "C"` callback. It performs only infallible
/// operations (copy the message, push to the queue) and never touches
/// scheduler/GC state; the `catch_unwind` guard converts a surprise
/// Rust panic into a queued marker instead of unwinding through C.
extern "C" fn on_event(code: i32, msg: *const c_char) {
    let r = std::panic::catch_unwind(|| {
        // SAFETY: C passes NUL-terminated literals here.
        let m = unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned();
        QUEUE.lock().expect("queue lock").push((code, m));
    });
    if r.is_err() {
        QUEUE
            .lock()
            .expect("queue lock")
            .push((-1, "callback panic".to_owned()));
    }
}

/// Run the C emitter, then drain what the callback queued.
pub fn emit_all() {
    // SAFETY: `on_event` is a static fn with no GC/scheduler touch.
    unsafe { ffi_emit_all(on_event) };
}

pub fn drain_queue() -> Vec<(i32, String)> {
    std::mem::take(&mut *QUEUE.lock().expect("queue lock"))
}

// -- error slot ------------------------------------------------------

/// C failure via error code + last-error slot; success path untouched.
pub fn fallible_double(x: i32) -> Result<i32, FfiError> {
    // SAFETY: linked by build.rs; slot is static storage.
    let r = unsafe { ffi_fallible(x) };
    if r < 0 {
        // SAFETY: `ffi_last_error` returns static NUL-terminated storage.
        let msg = unsafe { CStr::from_ptr(ffi_last_error()) }
            .to_string_lossy()
            .into_owned();
        return Err(FfiError::CError {
            code: r,
            message: msg,
        });
    }
    Ok(r)
}

// -- panic boundary --------------------------------------------------

/// Rust logic behind an `extern "C"` boundary (what C would call).
/// A Rust panic becomes an error code; nothing unwinds through C.
/// Requires `panic = "unwind"` (this crate's default).
pub extern "C" fn rust_div(a: i32, b: i32) -> i32 {
    match std::panic::catch_unwind(|| {
        if b == 0 {
            panic!("toto: integer division by zero");
        }
        a / b
    }) {
        Ok(v) => v,
        Err(_) => ERR_DIV_ZERO,
    }
}

// -- handle rule for retained pointers --------------------------------

/// Integer-handle table: the only way to retain a heap object across
/// calls. Each entry roots its object in `roots` for exactly the
/// entry's lifetime. There is deliberately no API that hands a raw
/// pointer to C for retention — retention is a `u32`, by
/// construction.
pub struct HandleTable {
    table: HashMap<u32, GcRef>,
    roots: ShadowStack,
    next: u32,
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            table: HashMap::new(),
            roots: ShadowStack::new(),
            next: 1,
        }
    }

    pub fn retain(&mut self, r: GcRef) -> u32 {
        let h = self.next;
        self.next += 1;
        self.table.insert(h, r);
        self.roots.push(r);
        h
    }

    pub fn resolve(&self, heap: &Heap, h: u32) -> Result<GcRef, FfiError> {
        let r = self.table.get(&h).copied().ok_or(FfiError::DeadHandle(h))?;
        // Liveness re-checked against the heap: after release + GC
        // the slot is gone, so a stale handle fails cleanly here.
        heap.get(r).map(|_| r).ok_or(FfiError::DeadHandle(h))
    }

    pub fn release(&mut self, h: u32) -> bool {
        match self.table.remove(&h) {
            Some(r) => {
                self.roots.remove_one(r);
                true
            }
            None => false,
        }
    }

    pub fn root_set(&self) -> &ShadowStack {
        &self.roots
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Forced collection over explicit root sets. Every root the test
/// wants kept must be in one of `shadows` — the collector sees
/// nothing else.
pub fn gc_collect(heap: &mut Heap, shadows: &[&ShadowStack]) -> GcReport {
    let mut roots = Vec::new();
    for s in shadows {
        roots.extend(s.iter());
    }
    heap.gc_collect_for_test(&roots)
}
