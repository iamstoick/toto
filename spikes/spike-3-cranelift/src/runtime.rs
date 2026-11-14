//! Runtime entry points for generated code, over the Spike 1 heap.
//!
//! All helpers are `extern "C"` and take the JIT context pointer
//! first. The context selects the heap and the shadow stack — the
//! same two roots a collection scans. Single-threaded: at most one
//! of (JIT code, helper) runs at a time, so the `*mut` round-trip is
//! contained here with `unsafe` (callers pass the pointer straight
//! back; it is never stored elsewhere).

use spike_1_gc_scheduler::heap::{GcRef, Heap};
use spike_1_gc_scheduler::roots::ShadowStack;

/// Owns everything one JIT session may touch. Lives on the Rust
/// side; generated code receives only an opaque pointer to it.
pub struct JitRuntime {
    pub heap: Heap,
    pub shadow: ShadowStack,
    pub threshold: u64,
}

impl JitRuntime {
    pub fn new() -> Self {
        Self {
            heap: Heap::new(),
            shadow: ShadowStack::new(),
            threshold: u64::MAX,
        }
    }

    pub fn with_threshold(mut self, bytes: u64) -> Self {
        self.threshold = bytes;
        self
    }

    pub fn ctx_ptr(&mut self) -> *mut JitRuntime {
        self as *mut JitRuntime
    }
}

impl Default for JitRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a handle for the JIT boundary: `(generation << 32) | index`.
pub fn encode(r: GcRef) -> u64 {
    ((r.generation() as u64) << 32) | r.index() as u64
}

/// Decode a JIT-side handle. A stale handle resolves to `None` at
/// use sites — never to a stranger's object.
pub fn decode(v: u64) -> GcRef {
    GcRef::from_parts(v as u32, (v >> 32) as u32)
}

fn rt<'r>(ctx: *mut JitRuntime) -> &'r mut JitRuntime {
    // SAFETY: single-threaded spike; the pointer is the live
    // `JitRuntime` the test handed to the JIT call, and no helper
    // re-enters while another borrow is alive (each helper takes and
    // drops its borrow; generated code holds no borrow).
    unsafe { &mut *ctx }
}

/// Allocation safepoint, shared by the allocating helpers: collect
/// BEFORE inserting when the threshold is reached, so the fresh
/// (still unrooted) object is never at risk. Live values must
/// already be published — the Spike 1 rule, unchanged.
fn allocation_safepoint(rt: &mut JitRuntime) {
    if rt.heap.bytes_since_gc() >= rt.threshold {
        let roots: Vec<GcRef> = rt.shadow.iter().collect();
        rt.heap.gc_collect_for_test(&roots);
    }
}

/// Allocate an int cell. Unrooted: publish before the next safepoint.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_int(ctx: *mut JitRuntime, v: i64) -> u64 {
    let rt = rt(ctx);
    allocation_safepoint(rt);
    encode(rt.heap.alloc_int(v))
}

/// Allocate a pair. Unrooted: publish before the next safepoint.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_alloc_pair(ctx: *mut JitRuntime, head: u64, tail: u64) -> u64 {
    let rt = rt(ctx);
    allocation_safepoint(rt);
    encode(rt.heap.alloc_pair(decode(head), decode(tail)))
}

/// Publish a live handle to the shadow stack.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_shadow_push(ctx: *mut JitRuntime, r: u64) {
    rt(ctx).shadow.push(decode(r));
}

/// Current shadow depth (lets generated prologues record a base).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_shadow_len(ctx: *mut JitRuntime) -> u64 {
    rt(ctx).shadow.len() as u64
}

/// Release roots pushed after `len` (generated epilogues / test reset).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_shadow_pop_to(ctx: *mut JitRuntime, len: u64) {
    rt(ctx).shadow.truncate(len as usize);
}

/// Explicit safepoint: collect now. Returns live object count.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_gc_collect(ctx: *mut JitRuntime) -> u64 {
    let rt = rt(ctx);
    let roots: Vec<GcRef> = rt.shadow.iter().collect();
    rt.heap.gc_collect_for_test(&roots).live_after as u64
}

/// Read pair field `idx` (0/1). Returns the encoded field handle.
/// The marker follows pair fields by descriptor — no root needed for
/// the read itself; root the result before the next safepoint.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_read_field(ctx: *mut JitRuntime, obj: u64, idx: u64) -> u64 {
    let rt = rt(ctx);
    let (a, b) = rt
        .heap
        .get_pair(decode(obj))
        .expect("toto_read_field on a live pair");
    encode(if idx == 0 { a } else { b })
}

/// Overwrite pair field `idx`. Returns 1 on success, 0 on stale/dead.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_write_field(
    ctx: *mut JitRuntime,
    obj: u64,
    idx: u64,
    val: u64,
) -> u64 {
    u64::from(
        rt(ctx)
            .heap
            .set_pair_field(decode(obj), idx as usize, decode(val)),
    )
}

/// Read an int cell, or `default` when the handle is dead. Lets
/// generated code OBSERVE reclamation (missed-root control).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn toto_get_int_or(ctx: *mut JitRuntime, obj: u64, default: i64) -> i64 {
    rt(ctx).heap.get_int(decode(obj)).unwrap_or(default)
}
