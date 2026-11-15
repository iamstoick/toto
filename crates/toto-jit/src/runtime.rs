//! Gate runtime: production-heap-backed entry points with the frozen
//! ADR-008 names and semantics.
//!
//! C-level type choices (the ADR freezes names and semantics, not C
//! types — recorded here, not in the ADR): every entry point takes the
//! context first; fallible points take the call-site span as trailing
//! `(start, end)` words so panics carry spans; values traffic as `u64`
//! bit patterns (`int` as two's-complement bits, `float` as IEEE-754
//! bits, `bool` as 0/1, heap references as the ADR-008 handle codec,
//! `unit`/`none` as 0) with an explicit kind tag where the callee must
//! construct a value; messages traffic as `(ptr, len)` to gate-owned
//! static bytes. `toto_yield` uses the frozen RESERVED name with a
//! gate-only signature (reserved signatures are explicitly unfrozen);
//! Phase 3/4 designs the real one.

use toto_runtime::heap::{GcRef, Heap, Payload};
use toto_runtime::roots::ShadowStack;
use toto_runtime::value::RtVal;

/// Numeric Toto codes as `u64` (`202` = `E0202`, `201` = `E0201`,
/// `209` = `E0209`, `204` = `E0204`).
pub const E_DIV_ZERO: u64 = 202;
pub const E_OVERFLOW: u64 = 201;
pub const E_OUT_OF_BOUNDS: u64 = 209;
pub const E_INTERNAL: u64 = 204;

/// Value-payload tags for `toto_write_field`.
pub const TAG_INT: u64 = 1;
pub const TAG_FLOAT: u64 = 2;
pub const TAG_BOOL: u64 = 3;
pub const TAG_HANDLE: u64 = 4;
pub const TAG_UNIT: u64 = 5;
pub const TAG_NONE: u64 = 6;

/// What a `toto_panic` call carried.
#[derive(Clone, Debug)]
pub struct PanicInfo {
    pub code: u64,
    pub start: u64,
    pub end: u64,
    pub message: String,
    pub func: String,
}

/// Gate context: everything generated code touches. Lives on the Rust
/// side; generated code receives only an opaque pointer. Single
/// OS thread (the frozen switch contract): at most one of (JIT code,
/// helper) runs at a time, so the `*mut` round-trip is contained here.
pub struct JitRt {
    pub heap: Heap,
    pub shadow: ShadowStack,
    pub threshold: u64,
    /// Function whose code is running (panic-trace frame, gate depth).
    pub func: String,
    pub last_panic: Option<PanicInfo>,
    /// Divergence after recording. Default prints the spec-§6 shape
    /// and exits 1 (the real contract — panic tests assert it in a
    /// subprocess since unwinding through JIT frames is unsound:
    /// `cranelift-jit` emits no unwind tables).
    pub on_panic: fn(&PanicInfo) -> !,
    /// Gate-only: coroutine yielder for `toto_yield` (the reserved
    /// name with a gate-only signature). Set by the harness around
    /// execution, cleared after; used only by the running coroutine.
    pub yielder: Option<*mut corosensei::Yielder<u64, u64>>,
    /// Gate-only: finalized gate-function pointers by name, for
    /// `'static` coroutine bodies (which cannot borrow the `Jit`).
    pub entries: std::collections::HashMap<String, usize>,
}

impl JitRt {
    pub fn new() -> Self {
        Self {
            heap: Heap::new(),
            shadow: ShadowStack::new(),
            threshold: u64::MAX,
            func: String::from("<jit>"),
            last_panic: None,
            on_panic: default_on_panic,
            yielder: None,
            entries: std::collections::HashMap::new(),
        }
    }

    pub fn with_threshold(mut self, bytes: u64) -> Self {
        self.threshold = bytes;
        self
    }

    pub fn ctx_ptr(&mut self) -> *mut JitRt {
        self as *mut JitRt
    }

    /// Gate-function pointer for a coroutine body (`entries` is filled
    /// by the harness after `compile`).
    pub fn entry(&self, name: &str) -> *const u8 {
        self.entries[name] as *const u8
    }
}

impl Default for JitRt {
    fn default() -> Self {
        Self::new()
    }
}

fn default_on_panic(info: &PanicInfo) -> ! {
    eprintln!(
        "panic[E{:04}]: {}\n --> <jit>:{}..{}\nstack trace:\n  in {} at <jit>:{}..{}",
        info.code, info.message, info.start, info.end, info.func, info.start, info.end
    );
    std::process::exit(1);
}

/// ADR-008 handle codec: `(generation << 32) | index`.
pub fn encode(r: GcRef) -> u64 {
    ((r.generation() as u64) << 32) | r.index() as u64
}

pub fn decode(v: u64) -> GcRef {
    GcRef::from_parts(v as u32, (v >> 32) as u32)
}

fn rt<'r>(ctx: *mut JitRt) -> &'r mut JitRt {
    // SAFETY: single-threaded gate; the pointer is the live `JitRt`
    // the test handed to the JIT call, never stored elsewhere, and no
    // helper re-enters while another borrow is alive.
    unsafe { &mut *ctx }
}

/// Allocation safepoint (Spike-1 rule, frozen): collect BEFORE
/// inserting when the threshold is reached, so the fresh (still
/// unrooted) object is never at risk. Live values must already be
/// published — the publish-before-safepoint rule, unchanged.
fn allocation_safepoint(rt: &mut JitRt) {
    if rt.heap.bytes_since_gc() >= rt.threshold {
        let roots: Vec<GcRef> = rt.shadow.iter().collect();
        rt.heap.collect(&roots);
    }
}

fn do_panic(rt: &mut JitRt, code: u64, start: u64, end: u64, message: String) -> ! {
    let info = PanicInfo {
        code,
        start,
        end,
        message,
        func: rt.func.clone(),
    };
    rt.last_panic = Some(info.clone());
    (rt.on_panic)(&info);
}

/// Owned decode (total: unknown tags are `Err`, never a Rust panic —
/// panicking across the JIT boundary would unwind through JIT frames,
/// which carry no unwind tables).
fn decode_tagged(tag: u64, value: u64) -> Result<RtVal, ()> {
    match tag {
        TAG_INT => Ok(RtVal::Int(value as i64)),
        TAG_FLOAT => Ok(RtVal::Float(f64::from_bits(value))),
        TAG_BOOL => Ok(RtVal::Bool(value != 0)),
        TAG_HANDLE => Ok(RtVal::Heap(decode(value))),
        TAG_UNIT => Ok(RtVal::Unit),
        TAG_NONE => Ok(RtVal::None),
        _ => Err(()),
    }
}

/// Owned encode (total: untrafficable values are `None`).
fn encode_val(v: &RtVal) -> Option<u64> {
    match v {
        RtVal::Int(i) => Some(*i as u64),
        RtVal::Float(f) => Some(f.to_bits()),
        RtVal::Bool(b) => Some(u64::from(*b)),
        RtVal::Unit | RtVal::None => Some(0),
        RtVal::Heap(r) => Some(encode(*r)),
        _ => None,
    }
}

/// Owned array-shape snapshot: ends the heap borrow before the caller
/// diverges, so no borrow is ever alive across a `do_panic`.
enum Shape {
    Array(usize),
    Stale,
    Other,
}

fn shape_of(heap: &Heap, obj: u64) -> Shape {
    match heap.get(decode(obj)) {
        Some(Payload::Array(elems)) => Shape::Array(elems.len()),
        Some(_) => Shape::Other,
        None => Shape::Stale,
    }
}

/// Allocate an array. `Int(0)`-filled: every gate array is fully
/// initialized by construction (the compiler emits a set per element
/// before any safepoint or read), so no uninitialized slot is ever
/// observable. Threshold checked BEFORE inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_array(ctx: *mut JitRt, len: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    if len > (isize::MAX as u64) {
        do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("array length {len} exceeds address space (gate limit)"),
        );
    }
    allocation_safepoint(rt);
    encode(rt.heap.alloc_array(vec![RtVal::Int(0); len as usize]))
}

/// Initialize one element (ADR-008: initialization through entry
/// points, never direct writes by generated code). Bounds-checked
/// (`E0209`); stale object or stale handle value diverges `E0204`.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_write_field(
    ctx: *mut JitRt,
    obj: u64,
    field: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) {
    let rt = rt(ctx);
    let len = match shape_of(&rt.heap, obj) {
        Shape::Array(n) => n,
        Shape::Stale => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in write_field (object {obj:#x} is not live)"),
        ),
        Shape::Other => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("write_field on a non-array (gate supports arrays only)"),
        ),
    };
    let n = field as i64;
    if n < 0 || (n as usize) >= len {
        // Exact interpreter message (`ops::index_read`).
        do_panic(
            rt,
            E_OUT_OF_BOUNDS,
            start,
            end,
            format!("index {n} out of bounds for array of length {len}"),
        );
    }
    let i = n as usize;
    let v = match decode_tagged(tag, value) {
        Ok(v) => v,
        Err(()) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!(
                "write_field with unknown value tag {tag} (gate carries int/float/bool/handle/unit/none)"
            ),
        ),
    };
    if let RtVal::Heap(r) = v
        && rt.heap.get(r).is_none()
    {
        do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle stored by write_field (value {value:#x} is not live)"),
        );
    }
    // Validated above (single-threaded, no mutation since): the slot is
    // an array with room for `i`. The fallback names a gate invariant
    // violation rather than a user-reachable failure.
    match rt.heap.get_mut(decode(obj)) {
        Some(Payload::Array(elems)) => elems[i] = v,
        _ => unreachable!("write_field: array shape validated above"),
    }
}

/// Read one element. Bounds-checked (`E0209`); stale object diverges
/// `E0204`. Returns payload bits; the caller decodes by static type.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_read_field(
    ctx: *mut JitRt,
    obj: u64,
    field: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let len = match shape_of(&rt.heap, obj) {
        Shape::Array(n) => n,
        Shape::Stale => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in read_field (object {obj:#x} is not live)"),
        ),
        Shape::Other => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("read_field on a non-array (gate supports arrays only)"),
        ),
    };
    let n = field as i64;
    if n < 0 || (n as usize) >= len {
        // Exact interpreter message (`ops::index_read`).
        do_panic(
            rt,
            E_OUT_OF_BOUNDS,
            start,
            end,
            format!("index {n} out of bounds for array of length {len}"),
        );
    }
    let i = n as usize;
    // Validated above: array with room for `i` (see write_field).
    let v = match rt.heap.get(decode(obj)) {
        Some(Payload::Array(elems)) => elems[i].clone(),
        _ => unreachable!("read_field: array shape validated above"),
    };
    match encode_val(&v) {
        Some(w) => w,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("read_field value is not JIT-trafficable (out of gate subset)"),
        ),
    }
}

/// Array length. Stale object diverges `E0204`.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_array_len(ctx: *mut JitRt, obj: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    if rt.heap.get(decode(obj)).is_none() {
        do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in array_len (object {obj:#x} is not live)"),
        );
    }
    match rt.heap.get(decode(obj)) {
        Some(Payload::Array(elems)) => elems.len() as u64,
        _ => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("array_len on a non-array (gate supports arrays only)"),
        ),
    }
}

/// Allocate a string by copying `(ptr, len)` host bytes. The gate
/// passes addresses of test-owned buffers as immediates (gate-only;
/// Phase 4 uses rodata). Invalid UTF-8 diverges `E0204` (real
/// invalid-UTF-8 paths belong to FFI, Phase 5).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_str(
    ctx: *mut JitRt,
    ptr: u64,
    len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("invalid UTF-8 in alloc_str (gate only accepts valid UTF-8)"),
        ),
    };
    allocation_safepoint(rt);
    encode(rt.heap.alloc_str(s))
}

/// Publish one live handle to the shadow stack.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_shadow_push(ctx: *mut JitRt, r: u64) {
    rt(ctx).shadow.push(decode(r));
}

/// Current shadow depth (generated prologues record a base).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_shadow_len(ctx: *mut JitRt) -> u64 {
    rt(ctx).shadow.len() as u64
}

/// Release roots pushed after `len` (generated epilogues).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_shadow_pop_to(ctx: *mut JitRt, len: u64) {
    rt(ctx).shadow.pop_to(len as usize);
}

/// Explicit collection safepoint: publish-before precondition (the
/// caller publishes first). Returns live object count.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_gc_collect(ctx: *mut JitRt) -> u64 {
    let rt = rt(ctx);
    let roots: Vec<GcRef> = rt.shadow.iter().collect();
    rt.heap.collect(&roots).live as u64
}

/// Explicit-check failure: record the Toto panic and diverge through
/// the configured handler (default: spec-§6 shape on stderr, exit 1).
/// `msg_ptr`/`msg_len` address gate-owned static message bytes.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_panic(
    ctx: *mut JitRt,
    code: u64,
    start: u64,
    end: u64,
    msg_ptr: u64,
    msg_len: u64,
) -> ! {
    let rt = rt(ctx);
    let bytes = unsafe { std::slice::from_raw_parts(msg_ptr as *const u8, msg_len as usize) };
    let message = String::from_utf8_lossy(bytes).into_owned();
    do_panic(rt, code, start, end, message)
}

/// Gate-only signature of the frozen RESERVED name `toto_yield`
/// (reserved signatures are explicitly unfrozen): suspend the calling
/// coroutine, yielding a token; the resume value is returned. The
/// harness installs the running coroutine's yielder in the context.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_yield(ctx: *mut JitRt) -> u64 {
    let rt = rt(ctx);
    match rt.yielder {
        Some(y) => unsafe { (*y).suspend(0x9e37) },
        None => do_panic(
            rt,
            E_INTERNAL,
            0,
            0,
            String::from("toto_yield with no yielder installed (gate harness bug)"),
        ),
    }
}
