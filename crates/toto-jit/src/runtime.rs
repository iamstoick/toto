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
/// `209` = `E0209`, `204` = `E0204`, `203` = `E0203`, `212` = `E0212`).
pub const E_DIV_ZERO: u64 = 202;
pub const E_OVERFLOW: u64 = 201;
pub const E_OUT_OF_BOUNDS: u64 = 209;
pub const E_INTERNAL: u64 = 204;
pub const E_MISMATCH: u64 = 203;
pub const E_FALLOFF: u64 = 212;

/// Value-payload tags for field writes and builtin arguments.
/// Heap families (`String`/`Array`/struct/`Range`/`Result`/`Option`/
/// closure) all traffic as `TAG_HANDLE`: the heap descriptor tells
/// them apart, so no per-family tag is needed.
pub const TAG_INT: u64 = 1;
pub const TAG_FLOAT: u64 = 2;
pub const TAG_BOOL: u64 = 3;
pub const TAG_HANDLE: u64 = 4;
pub const TAG_UNIT: u64 = 5;
pub const TAG_NONE: u64 = 6;
pub const TAG_CHAN: u64 = 7;
pub const TAG_FUNC: u64 = 8;
/// Dynamically-typed `fn` word: indices stay below 2^32, closure
/// handles never do (generations start at 1), so the helper tells
/// them apart by range.
pub const TAG_FN_ANY: u64 = 9;

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
    /// Top-level function names by index (indirect calls resolve
    /// `Func` values through `entries`). Set by the harness.
    pub func_names: Vec<String>,
    /// Compiled closure/go-body entry points by body id. Set by the
    /// harness after finalization.
    pub bodies: std::collections::HashMap<u32, usize>,
    /// `print`/`println` output (owned by the caller, like the
    /// interpreter's output buffer).
    pub output: Vec<u8>,
    /// Top-level function declarations for value display (`<fn name>`
    /// in `print`). Set by the harness from the parsed program.
    pub fns: Vec<toto_compiler::ast::FnDecl>,
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
            output: Vec::new(),
            fns: Vec::new(),
            func_names: Vec::new(),
            bodies: std::collections::HashMap::new(),
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
        // Word 0 is never a live handle (generations start at 1), so
        // a zero handle-word is `None` (dynamically-typed `Option`
        // words), never stale.
        TAG_HANDLE if value == 0 => Ok(RtVal::None),
        TAG_HANDLE => Ok(RtVal::Heap(decode(value))),
        TAG_UNIT => Ok(RtVal::Unit),
        TAG_NONE => Ok(RtVal::None),
        TAG_CHAN => Ok(RtVal::Chan(toto_runtime::channel::ChannelId(
            value as usize,
        ))),
        TAG_FUNC => Ok(RtVal::Func(value as usize)),
        TAG_FN_ANY if value < 0x1_0000_0000 => Ok(RtVal::Func(value as usize)),
        TAG_FN_ANY => Ok(RtVal::Heap(decode(value))),
        _ => Err(()),
    }
}

/// Owned encode (total: untrafficable values are `None`). JIT-built
/// heaps hold only these variants (ranges/wrappers/closures are heap
/// payloads behind `Heap` handles; immediates of those families never
/// appear), so `None` is an unreachable backstop.
fn encode_val(v: &RtVal) -> Option<u64> {
    match v {
        RtVal::Int(i) => Some(*i as u64),
        RtVal::Float(f) => Some(f.to_bits()),
        RtVal::Bool(b) => Some(u64::from(*b)),
        RtVal::Unit | RtVal::None => Some(0),
        RtVal::Heap(r) => Some(encode(*r)),
        RtVal::Chan(c) => Some(c.0 as u64),
        RtVal::Func(i) => Some(*i as u64),
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

/// Allocate an empty struct with declaration `name` (frozen family
/// name). Fields are installed by `toto_struct_init`, one per literal
/// field, before any safepoint or read — fully initialized by
/// construction, like arrays. Threshold checked BEFORE inserting
/// (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_struct(
    ctx: *mut JitRt,
    name_ptr: u64,
    name_len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let bytes = unsafe { std::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let name = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("invalid UTF-8 in alloc_struct name (unreachable: checked code)"),
        ),
    };
    allocation_safepoint(rt);
    encode(rt.heap.alloc_struct(name, Vec::new()))
}

/// Allocate a `Range` value (frozen family name). Total: bounds are
/// plain words. Threshold checked BEFORE inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_range(ctx: *mut JitRt, lo: u64, hi: u64) -> u64 {
    let rt = rt(ctx);
    allocation_safepoint(rt);
    encode(rt.heap.alloc_range(lo as i64, hi as i64))
}

/// Decode one tagged word for wrapper/closure construction (shared
/// by the `alloc_result`/`alloc_some`/`alloc_closure` family).
fn decode_inner(rt: &mut JitRt, who: &str, tag: u64, value: u64, start: u64, end: u64) -> RtVal {
    let v = match decode_tagged(tag, value) {
        Ok(v) => v,
        Err(()) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("{who} with unknown value tag {tag} (unreachable: checked code)"),
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
            format!("stale handle stored by {who} (value {value:#x} is not live)"),
        );
    }
    v
}

/// Allocate an `Ok`/`Err` value (frozen family name): `is_ok` selects
/// the variant, `(tag, value)` carries the inner word. Threshold
/// checked BEFORE inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_result(
    ctx: *mut JitRt,
    is_ok: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let inner = decode_inner(rt, "alloc_result", tag, value, start, end);
    allocation_safepoint(rt);
    encode(rt.heap.alloc_result(is_ok != 0, inner))
}

/// Allocate a `Some` value (frozen family name). `None` is the
/// immediate 0 and never allocates. Threshold checked BEFORE
/// inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_some(
    ctx: *mut JitRt,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let inner = decode_inner(rt, "alloc_some", tag, value, start, end);
    allocation_safepoint(rt);
    encode(rt.heap.alloc_some(inner))
}

/// Allocate a closure value (frozen family name): `body` is the
/// compiled-body id, `(caps_ptr, ncaps)` addresses `(tag, word)`
/// pairs in generated-code stack memory. Captures are shared
/// references (spec §5.7) traced by the collector. Threshold checked
/// BEFORE inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_closure(
    ctx: *mut JitRt,
    body: u64,
    caps_ptr: u64,
    ncaps: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let n = ncaps as usize;
    let words = unsafe { std::slice::from_raw_parts(caps_ptr as *const u64, n.saturating_mul(2)) };
    let mut captures = Vec::with_capacity(n);
    for i in 0..n {
        captures.push(decode_inner(
            rt,
            "alloc_closure",
            words[2 * i],
            words[2 * i + 1],
            start,
            end,
        ));
    }
    allocation_safepoint(rt);
    encode(rt.heap.alloc_closure(body as u32, captures))
}

/// Owned field-name snapshot: ends the raw borrow before the caller
/// diverges, so no borrow is ever alive across a `do_panic`.
fn read_name(rt: &mut JitRt, who: &str, ptr: u64, len: u64, start: u64, end: u64) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("invalid UTF-8 in {who} name (unreachable: checked code)"),
        ),
    }
}

enum StructMiss {
    Stale,
    Other,
}

/// Owned struct-shape snapshot (see `Shape`): declaration name plus a
/// field clone, so callers never diverge holding a heap borrow.
fn struct_snapshot(heap: &Heap, obj: u64) -> Result<(String, Vec<(String, RtVal)>), StructMiss> {
    match heap.get(decode(obj)) {
        Some(Payload::Struct { name, fields }) => Ok((name.clone(), fields.clone())),
        Some(_) => Err(StructMiss::Other),
        None => Err(StructMiss::Stale),
    }
}

/// Install one literal field during `StructLit` construction: appends,
/// replacing on duplicate (checked code sets each field exactly once;
/// the replace is a deterministic backstop, never reached).
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn toto_struct_init(
    ctx: *mut JitRt,
    obj: u64,
    name_ptr: u64,
    name_len: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let field = read_name(rt, "struct_init", name_ptr, name_len, start, end);
    let v = decode_inner(rt, "struct_init", tag, value, start, end);
    let at = match struct_snapshot(&rt.heap, obj) {
        Ok((_, fields)) => fields.iter().position(|(n, _)| *n == field),
        Err(StructMiss::Stale) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in struct_init (object {obj:#x} is not live)"),
        ),
        Err(StructMiss::Other) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("struct_init on a non-struct (unreachable: checked code)"),
        ),
    };
    match rt.heap.get_mut(decode(obj)) {
        Some(Payload::Struct { fields, .. }) => match at {
            Some(i) => fields[i].1 = v,
            None => fields.push((field, v)),
        },
        _ => unreachable!("struct_init: struct shape validated above"),
    }
    0
}

/// Read one field by name. Missing field and non-struct base panic
/// with the exact interpreter messages (`ops::field_read`); stale
/// objects diverge `E0204`.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_struct_get(
    ctx: *mut JitRt,
    obj: u64,
    name_ptr: u64,
    name_len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let field = read_name(rt, "struct_get", name_ptr, name_len, start, end);
    let found = match struct_snapshot(&rt.heap, obj) {
        Ok((name, fields)) => match fields.iter().find(|(n, _)| *n == field) {
            Some((_, v)) => v.clone(),
            None => do_panic(
                rt,
                E_OUT_OF_BOUNDS,
                start,
                end,
                format!("struct {name} has no field `{field}`"),
            ),
        },
        Err(StructMiss::Stale) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in struct_get (object {obj:#x} is not live)"),
        ),
        Err(StructMiss::Other) => {
            let tn = RtVal::Heap(decode(obj)).type_name(&rt.heap);
            do_panic(
                rt,
                E_OUT_OF_BOUNDS,
                start,
                end,
                format!("cannot access field of {tn}"),
            )
        }
    };
    match encode_val(&found) {
        Some(w) => w,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from(
                "struct_get value is not JIT-trafficable (unreachable in JIT-built heaps)",
            ),
        ),
    }
}

/// Store one field by name. Missing field and non-struct base panic
/// with the exact interpreter messages (`ops::field_write`); stale
/// objects diverge `E0204`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn toto_struct_set(
    ctx: *mut JitRt,
    obj: u64,
    name_ptr: u64,
    name_len: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let field = read_name(rt, "struct_set", name_ptr, name_len, start, end);
    let v = decode_inner(rt, "struct_set", tag, value, start, end);
    let at = match struct_snapshot(&rt.heap, obj) {
        Ok((name, fields)) => match fields.iter().position(|(n, _)| *n == field) {
            Some(i) => i,
            None => do_panic(
                rt,
                E_OUT_OF_BOUNDS,
                start,
                end,
                format!("struct {name} has no field `{field}`"),
            ),
        },
        Err(StructMiss::Stale) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in struct_set (object {obj:#x} is not live)"),
        ),
        Err(StructMiss::Other) => {
            let tn = RtVal::Heap(decode(obj)).type_name(&rt.heap);
            do_panic(
                rt,
                E_OUT_OF_BOUNDS,
                start,
                end,
                format!("cannot assign field on {tn}"),
            )
        }
    };
    match rt.heap.get_mut(decode(obj)) {
        Some(Payload::Struct { fields, .. }) => fields[at].1 = v,
        _ => unreachable!("struct_set: struct shape validated above"),
    }
    0
}

/// Owned payload snapshot: ends the heap borrow before the caller
/// diverges (the small-payload version of the `Shape` pattern).
fn payload_owned(heap: &Heap, obj: u64) -> Option<Payload> {
    heap.get(decode(obj)).cloned()
}

/// Concatenate two strings (frozen `Bin` `Add` on strings). Both
/// operands are statically strings; anything else is an unreachable
/// backstop. Threshold checked BEFORE inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_str_concat(
    ctx: *mut JitRt,
    a: u64,
    b: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let sa = match payload_owned(&rt.heap, a) {
        Some(Payload::Str(s)) => s,
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("str_concat on a non-string (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in str_concat (object {a:#x} is not live)"),
        ),
    };
    let sb = match payload_owned(&rt.heap, b) {
        Some(Payload::Str(s)) => s,
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("str_concat on a non-string (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in str_concat (object {b:#x} is not live)"),
        ),
    };
    allocation_safepoint(rt);
    encode(rt.heap.alloc_str(&format!("{sa}{sb}")))
}

/// Lower range bound. Stale objects diverge `E0204`; anything else
/// is an unreachable backstop (statically a range).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_range_lo(ctx: *mut JitRt, obj: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    match payload_owned(&rt.heap, obj) {
        Some(Payload::Range { lo, .. }) => lo as u64,
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("range_lo on a non-range (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in range_lo (object {obj:#x} is not live)"),
        ),
    }
}

/// Upper range bound (see `toto_range_lo`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_range_hi(ctx: *mut JitRt, obj: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    match payload_owned(&rt.heap, obj) {
        Some(Payload::Range { hi, .. }) => hi as u64,
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("range_hi on a non-range (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in range_hi (object {obj:#x} is not live)"),
        ),
    }
}

/// `1` for `Ok`, `0` for `Err`. Stale objects diverge `E0204`;
/// anything else is an unreachable backstop (statically a `Result`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_result_is_ok(ctx: *mut JitRt, obj: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    match payload_owned(&rt.heap, obj) {
        Some(Payload::Result { is_ok, .. }) => u64::from(is_ok),
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("result_is_ok on a non-Result (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in result_is_ok (object {obj:#x} is not live)"),
        ),
    }
}

/// Inner word of an `Ok`/`Err`/`Some` value. Stale objects diverge
/// `E0204`; anything else is an unreachable backstop.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_wrap_inner(ctx: *mut JitRt, obj: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    let inner = match payload_owned(&rt.heap, obj) {
        Some(Payload::Result { inner, .. }) | Some(Payload::Option { inner }) => inner,
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("wrap_inner on a non-wrapper (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in wrap_inner (object {obj:#x} is not live)"),
        ),
    };
    match encode_val(&inner) {
        Some(w) => w,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from(
                "wrap_inner value is not JIT-trafficable (unreachable in JIT-built heaps)",
            ),
        ),
    }
}

/// Compiled-body id of a closure value. Stale objects diverge
/// `E0204`; anything else is an unreachable backstop.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_closure_body(ctx: *mut JitRt, obj: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    match payload_owned(&rt.heap, obj) {
        Some(Payload::Closure { body, .. }) => body as u64,
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("closure_body on a non-closure (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in closure_body (object {obj:#x} is not live)"),
        ),
    }
}

/// One capture word of a closure value by static index. Stale
/// objects diverge `E0204`; anything else is an unreachable backstop
/// (codegen indexes by the static capture layout).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_closure_cap(
    ctx: *mut JitRt,
    obj: u64,
    idx: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let cap = match payload_owned(&rt.heap, obj) {
        Some(Payload::Closure { captures, .. }) => match captures.get(idx as usize).cloned() {
            Some(v) => v,
            None => do_panic(
                rt,
                E_INTERNAL,
                start,
                end,
                format!("closure capture index {idx} out of range (unreachable: checked code)"),
            ),
        },
        Some(_) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from("closure_cap on a non-closure (unreachable: checked code)"),
        ),
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in closure_cap (object {obj:#x} is not live)"),
        ),
    };
    match encode_val(&cap) {
        Some(w) => w,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            String::from(
                "closure_cap value is not JIT-trafficable (unreachable in JIT-built heaps)",
            ),
        ),
    }
}

/// Owned equality operand: ends heap borrows before any `E0203`
/// divergence. Comparison itself (`jit_values_equal`) is pure and
/// panic-free, like `ops::values_equal`.
struct EqOperand {
    kind: EqKind,
    word: u64,
    struct_name: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EqKind {
    Int,
    Float,
    Bool,
    Unit,
    Str,
    Range,
    Func,
    Closure,
    Chan,
    Array,
    Struct,
    Ok,
    Err,
    Some,
    None,
    /// Unreachable wrapper (exotic immediates inside JIT heaps, or
    /// a handle that died mid-compare — impossible: comparison never
    /// collects). Compares false against everything.
    Other,
}

fn eq_snapshot(rt: &mut JitRt, who: &str, tag: u64, word: u64, start: u64, end: u64) -> EqOperand {
    let plain = |kind| EqOperand {
        kind,
        word,
        struct_name: None,
    };
    match tag {
        TAG_INT => plain(EqKind::Int),
        TAG_FLOAT => plain(EqKind::Float),
        TAG_BOOL => plain(EqKind::Bool),
        TAG_UNIT => plain(EqKind::Unit),
        TAG_NONE => plain(EqKind::None),
        TAG_CHAN => plain(EqKind::Chan),
        TAG_FUNC => plain(EqKind::Func),
        // Statically fn-typed words traffic as `TAG_FN_ANY`: small
        // words are function indices, the rest are closure handles
        // (same 2^32 discrimination as `decode_tagged`; frozen D8).
        TAG_FN_ANY if word < 0x1_0000_0000 => plain(EqKind::Func),
        TAG_HANDLE | TAG_FN_ANY => match payload_owned(&rt.heap, word) {
            Some(Payload::Str(_)) => plain(EqKind::Str),
            Some(Payload::Array(_)) => plain(EqKind::Array),
            Some(Payload::Struct { name, .. }) => EqOperand {
                kind: EqKind::Struct,
                word,
                struct_name: Some(name),
            },
            Some(Payload::Range { .. }) => plain(EqKind::Range),
            Some(Payload::Result { is_ok, .. }) => {
                plain(if is_ok { EqKind::Ok } else { EqKind::Err })
            }
            Some(Payload::Option { .. }) => plain(EqKind::Some),
            Some(Payload::Closure { .. }) => plain(EqKind::Closure),
            None => do_panic(
                rt,
                E_INTERNAL,
                start,
                end,
                format!("stale handle in {who} (value {word:#x} is not live)"),
            ),
        },
        _ => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("{who} with unknown value tag {tag} (unreachable: checked code)"),
        ),
    }
}

fn eq_name(kind: EqKind) -> &'static str {
    match kind {
        EqKind::Int => "int",
        EqKind::Float => "float",
        EqKind::Bool => "bool",
        EqKind::Unit => "unit",
        EqKind::Str => "string",
        EqKind::Range => "Range",
        EqKind::Func | EqKind::Closure => "function",
        EqKind::Chan => "chan",
        EqKind::Array => "Array",
        EqKind::Struct => "struct",
        EqKind::Ok | EqKind::Err => "Result",
        EqKind::Some | EqKind::None => "Option",
        EqKind::Other => "unknown",
    }
}

/// The `ops::equality` same-type rule, mirrored exactly (struct names
/// must match; closures never match `Func`; `Ok`/`Err` and
/// `Some`/`None` are families; otherwise the kinds must agree).
fn eq_same_type(a: &EqOperand, b: &EqOperand) -> bool {
    match (a.kind, b.kind) {
        (EqKind::Struct, EqKind::Struct) => a.struct_name == b.struct_name,
        (EqKind::Closure, EqKind::Func) | (EqKind::Func, EqKind::Closure) => false,
        (EqKind::Ok | EqKind::Err, EqKind::Ok | EqKind::Err) => true,
        (EqKind::Some | EqKind::None, EqKind::Some | EqKind::None) => true,
        _ => a.kind == b.kind,
    }
}

fn operand_of_rtval(heap: &Heap, v: &RtVal) -> EqOperand {
    let plain = |kind| EqOperand {
        kind,
        word: 0,
        struct_name: None,
    };
    match v {
        RtVal::Int(i) => EqOperand {
            kind: EqKind::Int,
            word: *i as u64,
            struct_name: None,
        },
        RtVal::Float(f) => EqOperand {
            kind: EqKind::Float,
            word: f.to_bits(),
            struct_name: None,
        },
        RtVal::Bool(b) => EqOperand {
            kind: EqKind::Bool,
            word: u64::from(*b),
            struct_name: None,
        },
        RtVal::Unit => plain(EqKind::Unit),
        RtVal::None => plain(EqKind::None),
        RtVal::Chan(_) => plain(EqKind::Chan),
        RtVal::Func(i) => EqOperand {
            kind: EqKind::Func,
            word: *i as u64,
            struct_name: None,
        },
        RtVal::Heap(r) => {
            let word = encode(*r);
            match heap.get(*r) {
                Some(Payload::Str(_)) => EqOperand {
                    kind: EqKind::Str,
                    word,
                    struct_name: None,
                },
                Some(Payload::Array(_)) => EqOperand {
                    kind: EqKind::Array,
                    word,
                    struct_name: None,
                },
                Some(Payload::Struct { name, .. }) => EqOperand {
                    kind: EqKind::Struct,
                    word,
                    struct_name: Some(name.clone()),
                },
                Some(Payload::Range { .. }) => EqOperand {
                    kind: EqKind::Range,
                    word,
                    struct_name: None,
                },
                Some(Payload::Result { is_ok, .. }) => EqOperand {
                    kind: if *is_ok { EqKind::Ok } else { EqKind::Err },
                    word,
                    struct_name: None,
                },
                Some(Payload::Option { .. }) => EqOperand {
                    kind: EqKind::Some,
                    word,
                    struct_name: None,
                },
                Some(Payload::Closure { .. }) => EqOperand {
                    kind: EqKind::Closure,
                    word,
                    struct_name: None,
                },
                None => plain(EqKind::Other),
            }
        }
        RtVal::Range(..) | RtVal::Ok(_) | RtVal::Err(_) | RtVal::Some(_) | RtVal::Closure(_) => {
            plain(EqKind::Other)
        }
    }
}

/// The `ops::values_equal` rule, mirrored exactly: depth-100 cycle
/// guard with identity fallback, same-handle shortcut for
/// arrays/structs, deep structural comparison. Pure (never diverges),
/// so heap borrows are safe. `(Chan, Chan)` is `true`: the runtime
/// executor converts both to `"<chan>"` before comparing, and the JIT
/// matches it (parity quirk, documented — no corpus case covers it).
fn jit_values_equal(heap: &Heap, a: &EqOperand, b: &EqOperand, depth: usize) -> bool {
    if depth > 100 {
        return match (a.kind, b.kind) {
            (EqKind::Array, EqKind::Array) | (EqKind::Struct, EqKind::Struct) => a.word == b.word,
            _ => false,
        };
    }
    match (a.kind, b.kind) {
        (EqKind::Int, EqKind::Int) => a.word as i64 == b.word as i64,
        (EqKind::Float, EqKind::Float) => f64::from_bits(a.word) == f64::from_bits(b.word),
        (EqKind::Bool, EqKind::Bool) => (a.word != 0) == (b.word != 0),
        (EqKind::Unit, EqKind::Unit) => true,
        (EqKind::Str, EqKind::Str) => {
            match (payload_owned(heap, a.word), payload_owned(heap, b.word)) {
                (Some(Payload::Str(x)), Some(Payload::Str(y))) => x == y,
                _ => false,
            }
        }
        (EqKind::Range, EqKind::Range) => {
            match (payload_owned(heap, a.word), payload_owned(heap, b.word)) {
                (
                    Some(Payload::Range { lo: a0, hi: a1 }),
                    Some(Payload::Range { lo: b0, hi: b1 }),
                ) => a0 == b0 && a1 == b1,
                _ => false,
            }
        }
        (EqKind::Closure, EqKind::Closure) => a.word == b.word,
        (EqKind::Func, EqKind::Func) => a.word == b.word,
        (EqKind::Chan, EqKind::Chan) => true,
        (EqKind::Ok, EqKind::Ok) | (EqKind::Err, EqKind::Err) => {
            match (payload_owned(heap, a.word), payload_owned(heap, b.word)) {
                (
                    Some(Payload::Result { inner: x, .. }),
                    Some(Payload::Result { inner: y, .. }),
                ) => {
                    let xo = operand_of_rtval(heap, &x);
                    let yo = operand_of_rtval(heap, &y);
                    jit_values_equal(heap, &xo, &yo, depth + 1)
                }
                _ => false,
            }
        }
        (EqKind::Ok, EqKind::Err) | (EqKind::Err, EqKind::Ok) => false,
        (EqKind::Some, EqKind::Some) => {
            match (payload_owned(heap, a.word), payload_owned(heap, b.word)) {
                (Some(Payload::Option { inner: x }), Some(Payload::Option { inner: y })) => {
                    let xo = operand_of_rtval(heap, &x);
                    let yo = operand_of_rtval(heap, &y);
                    jit_values_equal(heap, &xo, &yo, depth + 1)
                }
                _ => false,
            }
        }
        (EqKind::None, EqKind::None) => true,
        (EqKind::Some, EqKind::None) | (EqKind::None, EqKind::Some) => false,
        (EqKind::Array, EqKind::Array) => {
            if a.word == b.word {
                return true;
            }
            match (payload_owned(heap, a.word), payload_owned(heap, b.word)) {
                (Some(Payload::Array(x)), Some(Payload::Array(y))) => {
                    x.len() == y.len()
                        && x.iter().zip(y.iter()).all(|(e, f)| {
                            let eo = operand_of_rtval(heap, e);
                            let fo = operand_of_rtval(heap, f);
                            jit_values_equal(heap, &eo, &fo, depth + 1)
                        })
                }
                _ => false,
            }
        }
        (EqKind::Struct, EqKind::Struct) => {
            if a.word == b.word {
                return true;
            }
            match (payload_owned(heap, a.word), payload_owned(heap, b.word)) {
                (
                    Some(Payload::Struct {
                        name: xn,
                        fields: x,
                    }),
                    Some(Payload::Struct {
                        name: yn,
                        fields: y,
                    }),
                ) => {
                    xn == yn
                        && x.len() == y.len()
                        && x.iter().zip(y.iter()).all(|((n, e), (m, f))| {
                            n == m && {
                                let eo = operand_of_rtval(heap, e);
                                let fo = operand_of_rtval(heap, f);
                                jit_values_equal(heap, &eo, &fo, depth + 1)
                            }
                        })
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// `==`/`!=` with the exact interpreter rule (`ops::equality`):
/// same-type check first (`E0203`), then deep comparison, then
/// `want_eq` selects the sense. The int/float help is unreachable in
/// checked programs (the checker rejects mixed operands statically),
/// so the gate divergence carries the message only.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn toto_values_eq(
    ctx: *mut JitRt,
    tag_a: u64,
    a: u64,
    tag_b: u64,
    b: u64,
    want_eq: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let oa = eq_snapshot(rt, "values_eq", tag_a, a, start, end);
    let ob = eq_snapshot(rt, "values_eq", tag_b, b, start, end);
    if !eq_same_type(&oa, &ob) {
        do_panic(
            rt,
            E_MISMATCH,
            start,
            end,
            format!(
                "cannot compare {} with {}",
                eq_name(oa.kind),
                eq_name(ob.kind)
            ),
        );
    }
    let eq = jit_values_equal(&rt.heap, &oa, &ob, 0);
    u64::from(if want_eq != 0 { eq } else { !eq })
}

/// `<`/`<=`/`>`/`>=` with the exact interpreter rule
/// (`ops::ordering`): `op` is 0/1/2/3 for `Lt`/`LtEq`/`Gt`/`GtEq`.
/// `int`, `float` (NaN compares false), and `string` order; channels
/// compare equal (the executor-conv quirk, as in equality);
/// anything else is `E0203`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn toto_values_order(
    ctx: *mut JitRt,
    op: u64,
    tag_a: u64,
    a: u64,
    tag_b: u64,
    b: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let oa = eq_snapshot(rt, "values_order", tag_a, a, start, end);
    let ob = eq_snapshot(rt, "values_order", tag_b, b, start, end);
    let ord = match (oa.kind, ob.kind) {
        (EqKind::Int, EqKind::Int) => (a as i64).cmp(&(b as i64)),
        (EqKind::Float, EqKind::Float) => match f64::from_bits(a).partial_cmp(&f64::from_bits(b)) {
            Some(o) => o,
            None => return 0,
        },
        (EqKind::Str, EqKind::Str) => {
            match (payload_owned(&rt.heap, a), payload_owned(&rt.heap, b)) {
                (Some(Payload::Str(x)), Some(Payload::Str(y))) => x.cmp(&y),
                _ => return 0,
            }
        }
        (EqKind::Chan, EqKind::Chan) => std::cmp::Ordering::Equal,
        _ => do_panic(
            rt,
            E_MISMATCH,
            start,
            end,
            format!("cannot order {} and {}", eq_name(oa.kind), eq_name(ob.kind)),
        ),
    };
    let result = match op {
        0 => ord == std::cmp::Ordering::Less,
        1 => ord != std::cmp::Ordering::Greater,
        2 => ord == std::cmp::Ordering::Greater,
        3 => ord != std::cmp::Ordering::Less,
        _ => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("values_order with unknown operator {op} (unreachable: checked code)"),
        ),
    };
    u64::from(result)
}

/// Builtin name ids: positions in the frozen `BUILTIN_NAMES` array —
/// the single source of truth shared by codegen and dispatch. (The
/// array holds 27 names; the "28" in prose is an off-by-one. Counted,
/// not trusted.)
pub fn builtin_id(name: &str) -> Option<u64> {
    toto_compiler::builtins::BUILTIN_NAMES
        .iter()
        .position(|n| *n == name)
        .map(|i| i as u64)
}

fn builtin_name(id: u64) -> Option<&'static str> {
    toto_compiler::builtins::BUILTIN_NAMES
        .get(id as usize)
        .copied()
}

fn builtin_arity(rt: &mut JitRt, name: &str, got: usize, want: usize, start: u64, end: u64) -> ! {
    do_panic(
        rt,
        E_INTERNAL,
        start,
        end,
        format!("wrong number of arguments: `{name}` takes {want}, got {got}"),
    )
}

/// Native `len` (messages mirror the shared builtin exactly):
/// strings count chars, arrays count elements.
fn builtin_len(rt: &mut JitRt, decoded: &[RtVal], start: u64, end: u64) -> u64 {
    if decoded.len() != 1 {
        builtin_arity(rt, "len", decoded.len(), 1, start, end);
    }
    let out: Option<u64> = match &decoded[0] {
        RtVal::Heap(r) => match rt.heap.get(*r) {
            Some(Payload::Str(s)) => Some(s.chars().count() as u64),
            Some(Payload::Array(elems)) => Some(elems.len() as u64),
            _ => None,
        },
        _ => None,
    };
    match out {
        Some(n) => n,
        None => {
            let tn = decoded[0].type_name(&rt.heap);
            do_panic(
                rt,
                210,
                start,
                end,
                format!("`len` needs a string or array, got {tn}"),
            )
        }
    }
}

/// Native `push`: mutates the original array (aliasing-preserving,
/// like the MIR executor's native path), returns the new length.
fn builtin_push(rt: &mut JitRt, decoded: &[RtVal], start: u64, end: u64) -> u64 {
    if decoded.len() != 2 {
        builtin_arity(rt, "push", decoded.len(), 2, start, end);
    }
    let arr = match &decoded[0] {
        RtVal::Heap(r) => *r,
        other => {
            let tn = other.type_name(&rt.heap);
            do_panic(
                rt,
                210,
                start,
                end,
                format!("`push` needs an array, got {tn}"),
            )
        }
    };
    if !matches!(rt.heap.get(arr), Some(Payload::Array(_))) {
        let tn = RtVal::Heap(arr).type_name(&rt.heap);
        do_panic(
            rt,
            210,
            start,
            end,
            format!("`push` needs an array, got {tn}"),
        )
    }
    match rt.heap.get_mut(arr) {
        Some(Payload::Array(elems)) => {
            elems.push(decoded[1].clone());
            elems.len() as u64
        }
        _ => unreachable!("push: array shape validated above"),
    }
}

/// Native `pop`: removes the last element of the original array,
/// returns it.
fn builtin_pop(rt: &mut JitRt, decoded: &[RtVal], start: u64, end: u64) -> u64 {
    if decoded.len() != 1 {
        builtin_arity(rt, "pop", decoded.len(), 1, start, end);
    }
    let arr = match &decoded[0] {
        RtVal::Heap(r) => *r,
        other => {
            let tn = other.type_name(&rt.heap);
            do_panic(
                rt,
                210,
                start,
                end,
                format!("`pop` needs an array, got {tn}"),
            )
        }
    };
    if !matches!(rt.heap.get(arr), Some(Payload::Array(_))) {
        let tn = RtVal::Heap(arr).type_name(&rt.heap);
        do_panic(
            rt,
            210,
            start,
            end,
            format!("`pop` needs an array, got {tn}"),
        )
    }
    let popped = match rt.heap.get_mut(arr) {
        Some(Payload::Array(elems)) => elems.pop(),
        _ => unreachable!("pop: array shape validated above"),
    };
    match popped {
        Some(v) => match encode_val(&v) {
            Some(w) => w,
            None => do_panic(
                rt,
                E_INTERNAL,
                start,
                end,
                String::from("pop value is not JIT-trafficable (unreachable in JIT-built heaps)"),
            ),
        },
        None => do_panic(rt, 210, start, end, String::from("pop from empty array")),
    }
}

/// Shared-implementation builtins: convert with one memo, dispatch
/// through `builtins::call_builtin` (exact messages/codes by
/// construction), convert the result back. File builtins run inline
/// here (the gate harness is sequential, like the tree-walker).
fn builtin_conv(
    rt: &mut JitRt,
    name: &'static str,
    decoded: Vec<RtVal>,
    span: toto_compiler::span::Span,
    start: u64,
    end: u64,
) -> u64 {
    use std::collections::HashMap;
    let mut memo = HashMap::new();
    let mut args = Vec::with_capacity(decoded.len());
    for v in &decoded {
        match toto_runtime::conv::to_value_memo(v, &rt.heap, &mut memo) {
            Ok(c) => args.push(c),
            Err(_) => do_panic(
                rt,
                E_INTERNAL,
                start,
                end,
                format!("builtin `{name}` argument is not convertible (unreachable: checked code)"),
            ),
        }
    }
    let out = toto_compiler::builtins::call_builtin(name, args, span, &rt.fns, &mut rt.output);
    match out {
        Ok(v) => {
            let back = match toto_runtime::conv::from_value(&v, &mut rt.heap) {
                Ok(r) => r,
                Err(_) => do_panic(
                    rt,
                    E_INTERNAL,
                    start,
                    end,
                    String::from(
                        "internal error: function from builtin (unreachable in checked programs)",
                    ),
                ),
            };
            match encode_val(&back) {
                Some(w) => w,
                None => do_panic(
                    rt,
                    E_INTERNAL,
                    start,
                    end,
                    String::from(
                        "builtin result is not JIT-trafficable (unreachable: checked code)",
                    ),
                ),
            }
        }
        Err((diag, _)) => {
            let code = diag
                .code
                .strip_prefix('E')
                .and_then(|c| c.parse::<u64>().ok())
                .unwrap_or(E_INTERNAL);
            do_panic(
                rt,
                code,
                diag.span.start as u64,
                diag.span.end as u64,
                diag.message,
            )
        }
    }
}

/// Dispatch one builtin call: `name_id` is a `builtin_id`, `args_ptr`
/// addresses `(tag, word)` pairs in generated-code stack memory.
/// `len`/`push`/`pop` run natively (aliasing-preserving);
/// everything else dispatches through the shared implementation.
/// `chan`/`close`/`closed` never arrive here (native scheduler
/// paths); a defensive internal error names them if they do.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_builtin(
    ctx: *mut JitRt,
    name_id: u64,
    args_ptr: u64,
    nargs: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    let name = match builtin_name(name_id) {
        Some(n) => n,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("builtin id {name_id} out of range (unreachable: checked code)"),
        ),
    };
    if matches!(name, "chan" | "close" | "closed") {
        do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("builtin `{name}` reached generic dispatch (unreachable: checked code)"),
        );
    }
    let n = nargs as usize;
    let words = unsafe { std::slice::from_raw_parts(args_ptr as *const u64, n.saturating_mul(2)) };
    // Decode to owned values first (ends all borrows): pair `i` is
    // `(tag, word)`, 1-based in messages.
    let mut decoded = Vec::with_capacity(n);
    for i in 0..n {
        let (tag, word) = (words[2 * i], words[2 * i + 1]);
        let v = match decode_tagged(tag, word) {
            Ok(v) => v,
            Err(()) => do_panic(
                rt,
                E_INTERNAL,
                start,
                end,
                format!(
                    "builtin `{name}` argument {} has unknown tag {tag} (unreachable: checked code)",
                    i + 1
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
                format!(
                    "stale handle in builtin `{name}` argument {} (unreachable: checked code)",
                    i + 1
                ),
            );
        }
        decoded.push(v);
    }
    let span = toto_compiler::span::Span::new(start as u32, end as u32);
    match name {
        "len" => builtin_len(rt, &decoded, start, end),
        "push" => builtin_push(rt, &decoded, start, end),
        "pop" => builtin_pop(rt, &decoded, start, end),
        _ => builtin_conv(rt, name, decoded, span, start, end),
    }
}

/// Array length for `MInstr::Len` (arrays only, `E0209` — unlike the
/// `E0210` builtin `len`, which also takes strings). Stale objects
/// diverge `E0204`.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_len(
    ctx: *mut JitRt,
    obj: u64,
    tag: u64,
    start: u64,
    end: u64,
) -> u64 {
    let rt = rt(ctx);
    if tag != TAG_HANDLE {
        let tn = match tag {
            TAG_INT => "int",
            TAG_FLOAT => "float",
            TAG_BOOL => "bool",
            TAG_UNIT => "unit",
            TAG_NONE => "Option",
            TAG_CHAN => "chan",
            TAG_FUNC => "function",
            _ => "unknown",
        };
        do_panic(
            rt,
            E_OUT_OF_BOUNDS,
            start,
            end,
            format!("`len` needs an array, got {tn}"),
        );
    }
    let shape: Option<Option<usize>> = match rt.heap.get(decode(obj)) {
        Some(Payload::Array(elems)) => Some(Some(elems.len())),
        Some(_) => Some(None),
        None => None,
    };
    match shape {
        Some(Some(n)) => n as u64,
        Some(None) => {
            let tn = RtVal::Heap(decode(obj)).type_name(&rt.heap);
            do_panic(
                rt,
                E_OUT_OF_BOUNDS,
                start,
                end,
                format!("`len` needs an array, got {tn}"),
            )
        }
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in len (object {obj:#x} is not live)"),
        ),
    }
}

/// Type name of a tagged word for `E0209` messages (mirrors
/// `ops::index_read` names exactly).
fn index_operand_name(rt: &JitRt, tag: u64, word: u64) -> &'static str {
    match tag {
        TAG_INT => "int",
        TAG_FLOAT => "float",
        TAG_BOOL => "bool",
        TAG_UNIT => "unit",
        TAG_NONE => "Option",
        TAG_CHAN => "chan",
        TAG_FUNC => "function",
        TAG_HANDLE => RtVal::Heap(decode(word)).type_name(&rt.heap),
        _ => "unknown",
    }
}

/// Read one array element with the exact interpreter messages
/// (`ops::index_read`): out-of-bounds at `span`, non-int index at
/// `ispan`, non-array base at `span`. Stale objects diverge `E0204`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn toto_index_read(
    ctx: *mut JitRt,
    obj: u64,
    btag: u64,
    itag: u64,
    idx: u64,
    start: u64,
    end: u64,
    istart: u64,
    iend: u64,
) -> u64 {
    let rt = rt(ctx);
    if itag != TAG_INT {
        let tn = index_operand_name(rt, itag, idx);
        do_panic(
            rt,
            E_OUT_OF_BOUNDS,
            istart,
            iend,
            format!("array index must be int, got {tn}"),
        );
    }
    let n = idx as i64;
    let found: Option<RtVal> = match rt.heap.get(decode(obj)) {
        Some(Payload::Array(elems)) => {
            if n < 0 || (n as usize) >= elems.len() {
                None
            } else {
                Some(elems[n as usize].clone())
            }
        }
        _ => None,
    };
    match found {
        Some(v) => match encode_val(&v) {
            Some(w) => w,
            None => do_panic(
                rt,
                E_INTERNAL,
                start,
                end,
                String::from("index value is not JIT-trafficable (unreachable in JIT-built heaps)"),
            ),
        },
        None => {
            let shape: Option<Option<usize>> = match rt.heap.get(decode(obj)) {
                Some(Payload::Array(elems)) => Some(Some(elems.len())),
                Some(_) => Some(None),
                None => None,
            };
            match shape {
                Some(Some(len)) => do_panic(
                    rt,
                    E_OUT_OF_BOUNDS,
                    start,
                    end,
                    format!("index {n} out of bounds for array of length {len}"),
                ),
                Some(None) => {
                    let tn = index_operand_name(rt, btag, obj);
                    do_panic(
                        rt,
                        E_OUT_OF_BOUNDS,
                        start,
                        end,
                        format!("cannot index {tn}"),
                    )
                }
                None => do_panic(
                    rt,
                    E_INTERNAL,
                    start,
                    end,
                    format!("stale handle in index_read (object {obj:#x} is not live)"),
                ),
            }
        }
    }
}

/// Store one array element with the exact interpreter messages
/// (`ops::index_write`). Stale objects diverge `E0204`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn toto_index_write(
    ctx: *mut JitRt,
    obj: u64,
    btag: u64,
    itag: u64,
    idx: u64,
    vtag: u64,
    vword: u64,
    start: u64,
    end: u64,
    istart: u64,
    iend: u64,
) -> u64 {
    let rt = rt(ctx);
    if itag != TAG_INT {
        let tn = index_operand_name(rt, itag, idx);
        do_panic(
            rt,
            E_OUT_OF_BOUNDS,
            istart,
            iend,
            format!("array index must be int, got {tn}"),
        );
    }
    let v = match decode_tagged(vtag, vword) {
        Ok(v) => v,
        Err(()) => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("index_write with unknown value tag {vtag} (unreachable: checked code)"),
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
            format!("stale handle stored by index_write (value {vword:#x} is not live)"),
        );
    }
    let n = idx as i64;
    enum WriteOut {
        Done,
        Oob(usize),
        NotArray,
        Stale,
    }
    let out = match rt.heap.get_mut(decode(obj)) {
        Some(Payload::Array(elems)) => {
            if n < 0 || (n as usize) >= elems.len() {
                WriteOut::Oob(elems.len())
            } else {
                elems[n as usize] = v;
                WriteOut::Done
            }
        }
        Some(_) => WriteOut::NotArray,
        None => WriteOut::Stale,
    };
    match out {
        WriteOut::Done => 0,
        WriteOut::Oob(len) => do_panic(
            rt,
            E_OUT_OF_BOUNDS,
            start,
            end,
            format!("index {n} out of bounds for array of length {len}"),
        ),
        WriteOut::NotArray => {
            let tn = index_operand_name(rt, btag, obj);
            do_panic(
                rt,
                E_OUT_OF_BOUNDS,
                start,
                end,
                format!("cannot index {tn}"),
            )
        }
        WriteOut::Stale => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("stale handle in index_write (object {obj:#x} is not live)"),
        ),
    }
}

/// Resolve a top-level function index to its compiled entry address
/// (indirect calls through `Func` values). Unknown indices diverge
/// `E0204` (unreachable: indices come from live `FuncRef`s, all
/// in-range for the compiled program).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_func_entry(ctx: *mut JitRt, idx: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    let name = rt.func_names.get(idx as usize).cloned();
    let name = match name {
        Some(n) => n,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("function index {idx} out of range (unreachable: checked code)"),
        ),
    };
    let entry = rt.entries.get(&name).copied();
    match entry {
        Some(e) => e as u64,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("function `{name}` has no compiled entry (unreachable: checked code)"),
        ),
    }
}

/// Resolve a compiled-body id to its entry address (indirect calls
/// through closure values). Unknown ids diverge `E0204`
/// (unreachable: ids come from live `MakeClosure`s).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_body_entry(ctx: *mut JitRt, body: u64, start: u64, end: u64) -> u64 {
    let rt = rt(ctx);
    let entry = rt.bodies.get(&(body as u32)).copied();
    match entry {
        Some(e) => e as u64,
        None => do_panic(
            rt,
            E_INTERNAL,
            start,
            end,
            format!("unknown call body id {body} (unreachable: checked code)"),
        ),
    }
}
