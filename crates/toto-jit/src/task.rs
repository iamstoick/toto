//! Production task context and entry points (Phase 4, Stage 1):
//! run JIT-compiled code on scheduler tasks through the same
//! `ToSched`/`ToTask` protocol the MIR executor speaks.
//!
//! Two deliberate differences from the gate (`runtime.rs`):
//! - The heap/shadows/output live behind the scheduler's `Shared`
//!   locks (M:N: helpers may run on any worker), not in the context.
//! - Panics RECORD (`failed` + `Diag`) and return a dummy word
//!   instead of diverging: unwinding through JIT frames is unsound
//!   (no unwind tables), so generated code checks `toto_check_failed`
//!   after every call and unwinds explicitly, appending one trace
//!   frame per level (caller appends the callee frame, exactly like
//!   the executor pushes at call time). The task entry assembles the
//!   final `TaskResult`.
//!
//! Suspension (`Send`/`Recv`/`Select`/`Close`/`Closed`/`Blocking`)
//! rides the reserved `toto_yield` name with an op-dispatched
//! signature (reserved signatures are explicitly unfrozen);
//! `Spawn`/`ChanNew` ride the reserved `toto_spawn`/`toto_chan_make`
//! names. All other imports share the gate's names; arities match
//! the gate except `toto_index_read`/`toto_index_write`, which carry
//! the tags and spans the full `E0209` messages need.

use std::collections::HashMap;
use std::sync::Arc;

use corosensei::Yielder;
use toto_compiler::ast::{FnDecl, ItemKind, Program};
use toto_compiler::diag::Diag;
use toto_compiler::hir::{CheckedProgram, HStructDecl, Ty};
use toto_compiler::interp::TraceFrame;
use toto_compiler::span::Span;
use toto_runtime::channel::{ChannelId, TaskId};
use toto_runtime::exec::TaskResult;
use toto_runtime::heap::{Heap, Payload};
use toto_runtime::sched::{
    BlockingOp, ExecCode, RunStats, Scheduler, ScopeId, SelReq, Shared, ToSched, ToTask,
};
use toto_runtime::value::RtVal;

use crate::runtime::{
    TAG_BOOL, TAG_CHAN, TAG_FLOAT, TAG_FN_ANY, TAG_FUNC, TAG_HANDLE, TAG_INT, TAG_NONE, TAG_UNIT,
    decode, encode,
};

/// Per-task production context. Owned by a `Box` whose pointer
/// crosses into the task as the `ctx` word; the task entry frees it
/// when the task finishes. Only the RUNNING task touches its context
/// (the scheduler's `Running` checkout guarantees no two workers run
/// one task), so the `*mut` round-trip needs no lock; everything
/// shared goes through `Shared`.
pub struct JitTaskCtx {
    pub shared: Arc<Shared>,
    /// Assigned task id and scope (filled by the task entry: the
    /// scheduler assigns them after the context is built).
    pub task: TaskId,
    pub scope: ScopeId,
    /// Running coroutine's yielder (filled by the task entry).
    /// Points into the task's own coroutine stack, which migrates
    /// with it — the same audit as the executor's `&Yielder`.
    pub yielder: Option<*const Yielder<ToTask, ToSched>>,
    /// Compiled entry for this task (`main` or a go-body id): takes
    /// the task-context pointer, returns the result word (ignored
    /// when `failed` is set).
    pub entry: usize,
    /// Go-body capture words (plain words; the body decodes them by
    /// its static capture types). Empty for `main`.
    pub captures: Vec<u64>,
    /// Root trace frame + result shape (the runner/spawner knows
    /// whether this task is `main`).
    pub root_name: String,
    pub root_span: Span,
    pub is_main: bool,
    /// Sentinel-panic state: set by the first recording failure,
    /// consumed by the task entry.
    pub failed: bool,
    pub diag: Option<Diag>,
    /// Skip the next callee-frame append (set only by a blocking
    /// `Cancelled`: mirrors the executor popping its builtin frame
    /// there). Consumed by `toto_check_failed`.
    pub skip_frame: bool,
    /// Trace frames appended innermost-first during unwinding (the
    /// root frame is pushed first); the entry reverses the tail so
    /// the final trace reads outermost-first like the executor's.
    pub trace: Vec<TraceFrame>,
    /// Shared compiled-program tables (cheap clones per spawn).
    pub fns: Arc<Vec<toto_compiler::ast::FnDecl>>,
    pub entries: Arc<HashMap<String, usize>>,
    pub bodies: Arc<HashMap<u32, usize>>,
    pub func_names: Arc<Vec<String>>,
    pub chan_types: Arc<Vec<toto_compiler::hir::Ty>>,
}

fn tctx<'a>(ctx: *mut JitTaskCtx) -> &'a mut JitTaskCtx {
    // SAFETY: only the running task touches its context (see the
    // struct docs), the pointer is the live box the spawner built,
    // and no helper re-enters while another borrow is alive.
    unsafe { &mut *ctx }
}

/// Record a Toto panic: first failure wins (generated code unwinds
/// after the first record, so later records would only mask it).
fn record(
    ctx: &mut JitTaskCtx,
    code: &'static str,
    message: String,
    span: Span,
    help: Option<String>,
) {
    if ctx.diag.is_none() {
        ctx.diag = Some(Diag::new(code, message, span, help));
    }
    ctx.failed = true;
}

/// Numeric codes from generated checks to `Diag` codes (total:
/// unknown codes keep their message under `E0204`).
fn code_str(code: u64) -> &'static str {
    match code {
        201 => "E0201",
        202 => "E0202",
        203 => "E0203",
        204 => "E0204",
        209 => "E0209",
        212 => "E0212",
        _ => "E0204",
    }
}

fn internal(ctx: &mut JitTaskCtx, what: String, span: Span) {
    record(
        ctx,
        "E0204",
        format!("internal error: {what} (unreachable in checked programs)"),
        span,
        None,
    );
}

fn scope_failed(ctx: &JitTaskCtx) -> bool {
    ctx.shared
        .scopes
        .lock()
        .expect("scopes lock")
        .is_failed_subtree(ctx.scope)
}

/// Collection check before inserting (mirrors `exec::maybe_collect`
/// exactly, minus the publish: generated code publishes at
/// safepoints, so roots are already in the shadow slot). Holding NO
/// locks at the rendezvous, as the contract requires.
fn maybe_collect(ctx: &JitTaskCtx) {
    let need = ctx.shared.force_collect_for_test
        || {
            let st = ctx.shared.state.lock().expect("shared state lock");
            st.heap.bytes_since_gc() >= ctx.shared.threshold
        }
        || { ctx.shared.gc.lock().expect("gc lock").requested };
    if need {
        ctx.shared.rendezvous_collect();
    }
}

/// Owned payload snapshot: ends the heap borrow before the caller
/// records, so no lock guard is ever held across a record.
fn payload_owned(heap: &Heap, obj: u64) -> Option<Payload> {
    heap.get(decode(obj)).cloned()
}

/// Decode one tagged word (shared by task constructors). Stale
/// handles and unknown tags record `E0204` (unreachable in checked
/// code: words come from live locals).
fn decode_inner(
    ctx: &mut JitTaskCtx,
    who: &str,
    tag: u64,
    value: u64,
    span: Span,
) -> Option<RtVal> {
    let v = match tag {
        TAG_INT => RtVal::Int(value as i64),
        TAG_FLOAT => RtVal::Float(f64::from_bits(value)),
        TAG_BOOL => RtVal::Bool(value != 0),
        // Word 0 is never a live handle (see the gate's decode).
        TAG_HANDLE if value == 0 => RtVal::None,
        TAG_HANDLE => RtVal::Heap(decode(value)),
        TAG_UNIT => RtVal::Unit,
        TAG_NONE => RtVal::None,
        TAG_CHAN => RtVal::Chan(ChannelId(value as usize)),
        TAG_FUNC => RtVal::Func(value as usize),
        TAG_FN_ANY if value < 0x1_0000_0000 => RtVal::Func(value as usize),
        TAG_FN_ANY => RtVal::Heap(decode(value)),
        _ => {
            internal(ctx, format!("{who} with unknown value tag {tag}"), span);
            return None;
        }
    };
    if let RtVal::Heap(r) = v {
        let live = ctx
            .shared
            .state
            .lock()
            .expect("shared state lock")
            .heap
            .get(r)
            .is_some();
        if !live {
            internal(
                ctx,
                format!("stale handle stored by {who} (value {value:#x} is not live)"),
                span,
            );
            return None;
        }
    }
    Some(v)
}

/// Owned encode (total: untrafficable values are `None`, an
/// unreachable backstop in JIT-built heaps).
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

/// Publish one live handle to this task's shadow slot (missing slot
/// is silently skipped, like the executor's publish).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_shadow_push(ctx: *mut JitTaskCtx, r: u64) {
    let ctx = tctx(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    if let Some(shadow) = st.shadows.get_mut(ctx.task.0) {
        shadow.push(decode(r));
    }
}

/// Current shadow depth of this task's slot.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_shadow_len(ctx: *mut JitTaskCtx) -> u64 {
    let ctx = tctx(ctx);
    let st = ctx.shared.state.lock().expect("shared state lock");
    st.shadows.get(ctx.task.0).map(|s| s.len()).unwrap_or(0) as u64
}

/// Release roots pushed after `len` in this task's slot.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_shadow_pop_to(ctx: *mut JitTaskCtx, len: u64) {
    let ctx = tctx(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    if let Some(shadow) = st.shadows.get_mut(ctx.task.0) {
        shadow.pop_to(len as usize);
    }
}

/// Explicit collection safepoint: roots are already published by
/// generated code (publish-before precondition, as in the gate).
/// Returns the live object count.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_gc_collect(ctx: *mut JitTaskCtx) -> u64 {
    let ctx = tctx(ctx);
    maybe_collect(ctx);
    let st = ctx.shared.state.lock().expect("shared state lock");
    st.heap.stats().objects_live as u64
}

/// Allocate an array (see the gate's `toto_alloc_array` for the
/// by-construction initialization argument). Threshold checked
/// BEFORE inserting (frozen rule).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_array(
    ctx: *mut JitTaskCtx,
    len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    if len > (isize::MAX as u64) {
        internal(
            ctx,
            format!("array length {len} exceeds address space (task limit)"),
            span,
        );
        return 0;
    }
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_array(vec![RtVal::Int(0); len as usize]))
}

/// Allocate a string by copying `(ptr, len)` bytes (see the gate's
/// `toto_alloc_str`). Threshold checked BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_str(
    ctx: *mut JitTaskCtx,
    ptr: u64,
    len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            internal(ctx, String::from("invalid UTF-8 in alloc_str"), span);
            return 0;
        }
    };
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_str(s))
}

/// Owned field-name snapshot (see the gate's `read_name`).
fn read_name(ctx: &mut JitTaskCtx, who: &str, ptr: u64, len: u64, span: Span) -> Option<String> {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s.to_string()),
        Err(_) => {
            internal(ctx, format!("invalid UTF-8 in {who} name"), span);
            None
        }
    }
}

/// Allocate an empty struct (see the gate's `toto_alloc_struct`).
/// Threshold checked BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_struct(
    ctx: *mut JitTaskCtx,
    name_ptr: u64,
    name_len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let name = match read_name(ctx, "alloc_struct", name_ptr, name_len, span) {
        Some(n) => n,
        None => return 0,
    };
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_struct(&name, Vec::new()))
}

/// Allocate a `Range` value (frozen family name). Total.
/// Threshold checked BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_range(ctx: *mut JitTaskCtx, lo: u64, hi: u64) -> u64 {
    let ctx = tctx(ctx);
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_range(lo as i64, hi as i64))
}

/// Allocate an `Ok`/`Err` value (frozen family name). Threshold
/// checked BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_result(
    ctx: *mut JitTaskCtx,
    is_ok: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let inner = match decode_inner(ctx, "alloc_result", tag, value, span) {
        Some(v) => v,
        None => return 0,
    };
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_result(is_ok != 0, inner))
}

/// Allocate a `Some` value (frozen family name). Threshold checked
/// BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_some(
    ctx: *mut JitTaskCtx,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let inner = match decode_inner(ctx, "alloc_some", tag, value, span) {
        Some(v) => v,
        None => return 0,
    };
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_some(inner))
}

/// Allocate a closure value (frozen family name). Threshold checked
/// BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_alloc_closure(
    ctx: *mut JitTaskCtx,
    body: u64,
    caps_ptr: u64,
    ncaps: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let n = ncaps as usize;
    let words = unsafe { std::slice::from_raw_parts(caps_ptr as *const u64, n.saturating_mul(2)) };
    let mut captures = Vec::with_capacity(n);
    for i in 0..n {
        match decode_inner(ctx, "alloc_closure", words[2 * i], words[2 * i + 1], span) {
            Some(v) => captures.push(v),
            None => return 0,
        }
    }
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_closure(body as u32, captures))
}

/// Type name of a tagged word for `E0209` messages (mirrors
/// `ops::index_read` names exactly).
fn index_operand_name(ctx: &JitTaskCtx, tag: u64, word: u64) -> &'static str {
    match tag {
        TAG_INT => "int",
        TAG_FLOAT => "float",
        TAG_BOOL => "bool",
        TAG_UNIT => "unit",
        TAG_NONE => "Option",
        TAG_CHAN => "chan",
        TAG_FUNC => "function",
        TAG_HANDLE => {
            let st = ctx.shared.state.lock().expect("shared state lock");
            RtVal::Heap(decode(word)).type_name(&st.heap)
        }
        _ => "unknown",
    }
}

/// Read one array element with the exact interpreter messages
/// (`ops::index_read`): out-of-bounds at `span`, non-int index at
/// `ispan`, non-array base at `span`. Stale objects record `E0204`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_index_read(
    ctx: *mut JitTaskCtx,
    obj: u64,
    btag: u64,
    itag: u64,
    idx: u64,
    start: u64,
    end: u64,
    istart: u64,
    iend: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let ispan = Span::new(istart as u32, iend as u32);
    if itag != TAG_INT {
        let tn = index_operand_name(ctx, itag, idx);
        record(
            ctx,
            "E0209",
            format!("array index must be int, got {tn}"),
            ispan,
            None,
        );
        return 0;
    }
    let n = idx as i64;
    let found: Option<RtVal> = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        match st.heap.get(decode(obj)) {
            Some(Payload::Array(elems)) => {
                if n < 0 || (n as usize) >= elems.len() {
                    None
                } else {
                    Some(elems[n as usize].clone())
                }
            }
            _ => None,
        }
    };
    match found {
        Some(v) => match encode_val(&v) {
            Some(w) => w,
            None => {
                internal(
                    ctx,
                    String::from("index value is not JIT-trafficable"),
                    span,
                );
                0
            }
        },
        None => {
            // Distinguish out-of-bounds from shape failures for the
            // exact message (owned snapshot first, then record).
            let shape: Option<Option<usize>> = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                match st.heap.get(decode(obj)) {
                    Some(Payload::Array(elems)) => Some(Some(elems.len())),
                    Some(_) => Some(None),
                    None => None,
                }
            };
            match shape {
                Some(Some(len)) => record(
                    ctx,
                    "E0209",
                    format!("index {n} out of bounds for array of length {len}"),
                    span,
                    None,
                ),
                Some(None) => {
                    let tn = index_operand_name(ctx, btag, obj);
                    record(ctx, "E0209", format!("cannot index {tn}"), span, None)
                }
                None => {
                    internal(
                        ctx,
                        format!("stale handle in index_read (object {obj:#x} is not live)"),
                        span,
                    );
                }
            }
            0
        }
    }
}

/// Store one array element with the exact interpreter messages
/// (`ops::index_write`). Stale objects record `E0204`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_index_write(
    ctx: *mut JitTaskCtx,
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
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let ispan = Span::new(istart as u32, iend as u32);
    if itag != TAG_INT {
        let tn = index_operand_name(ctx, itag, idx);
        record(
            ctx,
            "E0209",
            format!("array index must be int, got {tn}"),
            ispan,
            None,
        );
        return 0;
    }
    let v = match decode_inner(ctx, "index_write", vtag, vword, span) {
        Some(v) => v,
        None => return 0,
    };
    let n = idx as i64;
    // One exclusive guard for validate + write (atomic under M:N:
    // no check-then-act race with concurrent mutators), owned
    // outcome, then record with no guard alive.
    enum WriteOut {
        Done,
        Oob(usize),
        NotArray,
        Stale,
    }
    let out = {
        let mut st = ctx.shared.state.lock().expect("shared state lock");
        match st.heap.get_mut(decode(obj)) {
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
        }
    };
    match out {
        WriteOut::Done => 0,
        WriteOut::Oob(len) => {
            record(
                ctx,
                "E0209",
                format!("index {n} out of bounds for array of length {len}"),
                span,
                None,
            );
            0
        }
        WriteOut::NotArray => {
            let tn = index_operand_name(ctx, btag, obj);
            record(ctx, "E0209", format!("cannot index {tn}"), span, None);
            0
        }
        WriteOut::Stale => {
            internal(
                ctx,
                format!("stale handle in index_write (object {obj:#x} is not live)"),
                span,
            );
            0
        }
    }
}

enum StructMiss {
    Stale,
    Other,
}

/// Owned struct-shape snapshot (see the gate's `struct_snapshot`).
fn struct_snapshot(heap: &Heap, obj: u64) -> Result<(String, Vec<(String, RtVal)>), StructMiss> {
    match heap.get(decode(obj)) {
        Some(Payload::Struct { name, fields }) => Ok((name.clone(), fields.clone())),
        Some(_) => Err(StructMiss::Other),
        None => Err(StructMiss::Stale),
    }
}

/// Install one literal field during `StructLit` construction (see
/// the gate's `toto_struct_init`).
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_struct_init(
    ctx: *mut JitTaskCtx,
    obj: u64,
    name_ptr: u64,
    name_len: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let field = match read_name(ctx, "struct_init", name_ptr, name_len, span) {
        Some(n) => n,
        None => return 0,
    };
    let v = match decode_inner(ctx, "struct_init", tag, value, span) {
        Some(v) => v,
        None => return 0,
    };
    // One exclusive guard for validate + write (atomic under M:N),
    // owned outcome, then record with no guard alive.
    enum InitOut {
        Done,
        Stale,
        Other,
    }
    let out = {
        let mut st = ctx.shared.state.lock().expect("shared state lock");
        match st.heap.get_mut(decode(obj)) {
            Some(Payload::Struct { fields, .. }) => {
                match fields.iter().position(|(n, _)| *n == field) {
                    Some(i) => fields[i].1 = v,
                    None => fields.push((field, v)),
                }
                InitOut::Done
            }
            Some(_) => InitOut::Other,
            None => InitOut::Stale,
        }
    };
    match out {
        InitOut::Done => 0,
        InitOut::Stale => {
            internal(
                ctx,
                format!("stale handle in struct_init (object {obj:#x} is not live)"),
                span,
            );
            0
        }
        InitOut::Other => {
            internal(ctx, String::from("struct_init on a non-struct"), span);
            0
        }
    }
}

/// Read one field by name with the exact interpreter messages
/// (`ops::field_read`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_struct_get(
    ctx: *mut JitTaskCtx,
    obj: u64,
    name_ptr: u64,
    name_len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let field = match read_name(ctx, "struct_get", name_ptr, name_len, span) {
        Some(n) => n,
        None => return 0,
    };
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        struct_snapshot(&st.heap, obj)
    };
    let found: Option<RtVal> = match snap {
        Ok((name, fields)) => match fields.iter().find(|(n, _)| *n == field) {
            Some((_, v)) => Some(v.clone()),
            None => {
                record(
                    ctx,
                    "E0209",
                    format!("struct {name} has no field `{field}`"),
                    span,
                    None,
                );
                return 0;
            }
        },
        Err(StructMiss::Stale) => {
            internal(
                ctx,
                format!("stale handle in struct_get (object {obj:#x} is not live)"),
                span,
            );
            return 0;
        }
        Err(StructMiss::Other) => None,
    };
    match found {
        Some(v) => match encode_val(&v) {
            Some(w) => w,
            None => {
                internal(
                    ctx,
                    String::from("struct_get value is not JIT-trafficable"),
                    span,
                );
                0
            }
        },
        None => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                RtVal::Heap(decode(obj)).type_name(&st.heap)
            };
            record(
                ctx,
                "E0209",
                format!("cannot access field of {tn}"),
                span,
                None,
            );
            0
        }
    }
}

/// Store one field by name with the exact interpreter messages
/// (`ops::field_write`).
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_struct_set(
    ctx: *mut JitTaskCtx,
    obj: u64,
    name_ptr: u64,
    name_len: u64,
    tag: u64,
    value: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let field = match read_name(ctx, "struct_set", name_ptr, name_len, span) {
        Some(n) => n,
        None => return 0,
    };
    let v = match decode_inner(ctx, "struct_set", tag, value, span) {
        Some(v) => v,
        None => return 0,
    };
    // One exclusive guard for validate + write (atomic under M:N),
    // owned outcome, then record with no guard alive.
    enum SetOut {
        Done,
        Missing(String),
        Stale,
        Other,
    }
    let out = {
        let mut st = ctx.shared.state.lock().expect("shared state lock");
        match st.heap.get_mut(decode(obj)) {
            Some(Payload::Struct { name, fields }) => {
                match fields.iter().position(|(n, _)| *n == field) {
                    Some(i) => {
                        fields[i].1 = v;
                        SetOut::Done
                    }
                    None => SetOut::Missing(name.clone()),
                }
            }
            Some(_) => SetOut::Other,
            None => SetOut::Stale,
        }
    };
    match out {
        SetOut::Done => 0,
        SetOut::Missing(struct_name) => {
            record(
                ctx,
                "E0209",
                format!("struct {struct_name} has no field `{field}`"),
                span,
                None,
            );
            0
        }
        SetOut::Stale => {
            internal(
                ctx,
                format!("stale handle in struct_set (object {obj:#x} is not live)"),
                span,
            );
            0
        }
        SetOut::Other => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                RtVal::Heap(decode(obj)).type_name(&st.heap)
            };
            record(
                ctx,
                "E0209",
                format!("cannot assign field on {tn}"),
                span,
                None,
            );
            0
        }
    }
}

/// Array length for `MInstr::Len` (arrays only, `E0209` — unlike the
/// `E0210` builtin `len`, which also takes strings). Stale objects
/// record `E0204`.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_len(
    ctx: *mut JitTaskCtx,
    obj: u64,
    tag: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
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
        record(
            ctx,
            "E0209",
            format!("`len` needs an array, got {tn}"),
            span,
            None,
        );
        return 0;
    }
    let shape: Option<Option<usize>> = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        match st.heap.get(decode(obj)) {
            Some(Payload::Array(elems)) => Some(Some(elems.len())),
            Some(_) => Some(None),
            None => None,
        }
    };
    match shape {
        Some(Some(n)) => n as u64,
        Some(None) => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                RtVal::Heap(decode(obj)).type_name(&st.heap)
            };
            record(
                ctx,
                "E0209",
                format!("`len` needs an array, got {tn}"),
                span,
                None,
            );
            0
        }
        None => {
            internal(
                ctx,
                format!("stale handle in len (object {obj:#x} is not live)"),
                span,
            );
            0
        }
    }
}

/// Concatenate two strings (see the gate's `toto_str_concat`).
/// Threshold checked BEFORE inserting.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_str_concat(
    ctx: *mut JitTaskCtx,
    a: u64,
    b: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let pair = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        (payload_owned(&st.heap, a), payload_owned(&st.heap, b))
    };
    let (sa, sb) = match pair {
        (Some(Payload::Str(x)), Some(Payload::Str(y))) => (x, y),
        (None, _) => {
            internal(
                ctx,
                format!("stale handle in str_concat (object {a:#x} is not live)"),
                span,
            );
            return 0;
        }
        (_, None) => {
            internal(
                ctx,
                format!("stale handle in str_concat (object {b:#x} is not live)"),
                span,
            );
            return 0;
        }
        _ => {
            internal(ctx, String::from("str_concat on a non-string"), span);
            return 0;
        }
    };
    maybe_collect(ctx);
    let mut st = ctx.shared.state.lock().expect("shared state lock");
    encode(st.heap.alloc_str(&format!("{sa}{sb}")))
}

/// Lower range bound (see the gate's `toto_range_lo`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_range_lo(
    ctx: *mut JitTaskCtx,
    obj: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        payload_owned(&st.heap, obj)
    };
    match snap {
        Some(Payload::Range { lo, .. }) => lo as u64,
        Some(_) => {
            internal(ctx, String::from("range_lo on a non-range"), span);
            0
        }
        None => {
            internal(
                ctx,
                format!("stale handle in range_lo (object {obj:#x} is not live)"),
                span,
            );
            0
        }
    }
}

/// Upper range bound (see the gate's `toto_range_hi`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_range_hi(
    ctx: *mut JitTaskCtx,
    obj: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        payload_owned(&st.heap, obj)
    };
    match snap {
        Some(Payload::Range { hi, .. }) => hi as u64,
        Some(_) => {
            internal(ctx, String::from("range_hi on a non-range"), span);
            0
        }
        None => {
            internal(
                ctx,
                format!("stale handle in range_hi (object {obj:#x} is not live)"),
                span,
            );
            0
        }
    }
}

/// `1` for `Ok`, `0` for `Err` (see the gate's `toto_result_is_ok`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_result_is_ok(
    ctx: *mut JitTaskCtx,
    obj: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        payload_owned(&st.heap, obj)
    };
    match snap {
        Some(Payload::Result { is_ok, .. }) => u64::from(is_ok),
        Some(_) => {
            internal(ctx, String::from("result_is_ok on a non-Result"), span);
            0
        }
        None => {
            internal(
                ctx,
                format!("stale handle in result_is_ok (object {obj:#x} is not live)"),
                span,
            );
            0
        }
    }
}

/// Inner word of an `Ok`/`Err`/`Some` value (see the gate's
/// `toto_wrap_inner`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_wrap_inner(
    ctx: *mut JitTaskCtx,
    obj: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        payload_owned(&st.heap, obj)
    };
    let inner = match snap {
        Some(Payload::Result { inner, .. }) | Some(Payload::Option { inner }) => inner,
        Some(_) => {
            internal(ctx, String::from("wrap_inner on a non-wrapper"), span);
            return 0;
        }
        None => {
            internal(
                ctx,
                format!("stale handle in wrap_inner (object {obj:#x} is not live)"),
                span,
            );
            return 0;
        }
    };
    match encode_val(&inner) {
        Some(w) => w,
        None => {
            internal(
                ctx,
                String::from("wrap_inner value is not JIT-trafficable"),
                span,
            );
            0
        }
    }
}

/// Compiled-body id of a closure value (see the gate's
/// `toto_closure_body`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_closure_body(
    ctx: *mut JitTaskCtx,
    obj: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        payload_owned(&st.heap, obj)
    };
    match snap {
        Some(Payload::Closure { body, .. }) => body as u64,
        Some(_) => {
            internal(ctx, String::from("closure_body on a non-closure"), span);
            0
        }
        None => {
            internal(
                ctx,
                format!("stale handle in closure_body (object {obj:#x} is not live)"),
                span,
            );
            0
        }
    }
}

/// One capture word of a closure value by static index (see the
/// gate's `toto_closure_cap`).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_closure_cap(
    ctx: *mut JitTaskCtx,
    obj: u64,
    idx: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let snap = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        payload_owned(&st.heap, obj)
    };
    let cap = match snap {
        Some(Payload::Closure { captures, .. }) => match captures.get(idx as usize).cloned() {
            Some(v) => v,
            None => {
                internal(
                    ctx,
                    format!("closure capture index {idx} out of range"),
                    span,
                );
                return 0;
            }
        },
        Some(_) => {
            internal(ctx, String::from("closure_cap on a non-closure"), span);
            return 0;
        }
        None => {
            internal(
                ctx,
                format!("stale handle in closure_cap (object {obj:#x} is not live)"),
                span,
            );
            return 0;
        }
    };
    match encode_val(&cap) {
        Some(w) => w,
        None => {
            internal(
                ctx,
                String::from("closure_cap value is not JIT-trafficable"),
                span,
            );
            0
        }
    }
}

/// Owned equality operand (see the gate's `EqOperand`): ends heap
/// borrows before any `E0203` record. Comparison itself is pure.
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
    Other,
}

fn eq_snapshot(
    ctx: &mut JitTaskCtx,
    who: &str,
    tag: u64,
    word: u64,
    span: Span,
) -> Option<EqOperand> {
    let plain = |kind| EqOperand {
        kind,
        word,
        struct_name: None,
    };
    match tag {
        TAG_INT => Some(plain(EqKind::Int)),
        TAG_FLOAT => Some(plain(EqKind::Float)),
        TAG_BOOL => Some(plain(EqKind::Bool)),
        TAG_UNIT => Some(plain(EqKind::Unit)),
        TAG_NONE => Some(plain(EqKind::None)),
        TAG_CHAN => Some(plain(EqKind::Chan)),
        TAG_FUNC => Some(plain(EqKind::Func)),
        TAG_HANDLE => {
            let snap = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                payload_owned(&st.heap, word)
            };
            match snap {
                Some(Payload::Str(_)) => Some(plain(EqKind::Str)),
                Some(Payload::Array(_)) => Some(plain(EqKind::Array)),
                Some(Payload::Struct { name, .. }) => Some(EqOperand {
                    kind: EqKind::Struct,
                    word,
                    struct_name: Some(name),
                }),
                Some(Payload::Range { .. }) => Some(plain(EqKind::Range)),
                Some(Payload::Result { is_ok, .. }) => {
                    Some(plain(if is_ok { EqKind::Ok } else { EqKind::Err }))
                }
                Some(Payload::Option { .. }) => Some(plain(EqKind::Some)),
                Some(Payload::Closure { .. }) => Some(plain(EqKind::Closure)),
                None => {
                    internal(
                        ctx,
                        format!("stale handle in {who} (value {word:#x} is not live)"),
                        span,
                    );
                    None
                }
            }
        }
        _ => {
            internal(ctx, format!("{who} with unknown value tag {tag}"), span);
            None
        }
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

/// The `ops::equality` same-type rule, mirrored exactly.
fn eq_same_type(a: &EqOperand, b: &EqOperand) -> bool {
    match (a.kind, b.kind) {
        (EqKind::Struct, EqKind::Struct) => a.struct_name == b.struct_name,
        (EqKind::Closure, EqKind::Func) | (EqKind::Func, EqKind::Closure) => false,
        (EqKind::Ok | EqKind::Err, EqKind::Ok | EqKind::Err) => true,
        (EqKind::Some | EqKind::None, EqKind::Some | EqKind::None) => true,
        _ => a.kind == b.kind,
    }
}

/// The shared int/float conversion hint (unreachable in checked
/// programs, preserved for exactness).
fn eq_hint(a: &EqOperand, b: &EqOperand) -> Option<String> {
    match (a.kind, b.kind) {
        (EqKind::Int, EqKind::Float) | (EqKind::Float, EqKind::Int) => Some(
            "int and float do not mix implicitly; convert explicitly with to_int or to_float"
                .to_string(),
        ),
        _ => None,
    }
}

fn operand_of_rtval(ctx: &JitTaskCtx, v: &RtVal) -> EqOperand {
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
            let snap = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                payload_owned(&st.heap, word)
            };
            match snap {
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
                    struct_name: Some(name),
                },
                Some(Payload::Range { .. }) => EqOperand {
                    kind: EqKind::Range,
                    word,
                    struct_name: None,
                },
                Some(Payload::Result { is_ok, .. }) => EqOperand {
                    kind: if is_ok { EqKind::Ok } else { EqKind::Err },
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

/// The `ops::values_equal` rule, mirrored exactly (see the gate's
/// `jit_values_equal`, including the `(Chan, Chan)` quirk). Pure
/// apart from short shared borrows; comparison never collects, so
/// live handles stay live throughout.
fn jit_values_equal(ctx: &JitTaskCtx, a: &EqOperand, b: &EqOperand, depth: usize) -> bool {
    if depth > 100 {
        return match (a.kind, b.kind) {
            (EqKind::Array, EqKind::Array) | (EqKind::Struct, EqKind::Struct) => a.word == b.word,
            _ => false,
        };
    }
    let payloads = |x: u64, y: u64| {
        let st = ctx.shared.state.lock().expect("shared state lock");
        (payload_owned(&st.heap, x), payload_owned(&st.heap, y))
    };
    match (a.kind, b.kind) {
        (EqKind::Int, EqKind::Int) => a.word as i64 == b.word as i64,
        (EqKind::Float, EqKind::Float) => f64::from_bits(a.word) == f64::from_bits(b.word),
        (EqKind::Bool, EqKind::Bool) => (a.word != 0) == (b.word != 0),
        (EqKind::Unit, EqKind::Unit) => true,
        (EqKind::Str, EqKind::Str) => match payloads(a.word, b.word) {
            (Some(Payload::Str(x)), Some(Payload::Str(y))) => x == y,
            _ => false,
        },
        (EqKind::Range, EqKind::Range) => match payloads(a.word, b.word) {
            (Some(Payload::Range { lo: a0, hi: a1 }), Some(Payload::Range { lo: b0, hi: b1 })) => {
                a0 == b0 && a1 == b1
            }
            _ => false,
        },
        (EqKind::Closure, EqKind::Closure) => a.word == b.word,
        (EqKind::Func, EqKind::Func) => a.word == b.word,
        (EqKind::Chan, EqKind::Chan) => true,
        (EqKind::Ok, EqKind::Ok) | (EqKind::Err, EqKind::Err) => match payloads(a.word, b.word) {
            (Some(Payload::Result { inner: x, .. }), Some(Payload::Result { inner: y, .. })) => {
                let xo = operand_of_rtval(ctx, &x);
                let yo = operand_of_rtval(ctx, &y);
                jit_values_equal(ctx, &xo, &yo, depth + 1)
            }
            _ => false,
        },
        (EqKind::Ok, EqKind::Err) | (EqKind::Err, EqKind::Ok) => false,
        (EqKind::Some, EqKind::Some) => match payloads(a.word, b.word) {
            (Some(Payload::Option { inner: x }), Some(Payload::Option { inner: y })) => {
                let xo = operand_of_rtval(ctx, &x);
                let yo = operand_of_rtval(ctx, &y);
                jit_values_equal(ctx, &xo, &yo, depth + 1)
            }
            _ => false,
        },
        (EqKind::None, EqKind::None) => true,
        (EqKind::Some, EqKind::None) | (EqKind::None, EqKind::Some) => false,
        (EqKind::Array, EqKind::Array) => {
            if a.word == b.word {
                return true;
            }
            match payloads(a.word, b.word) {
                (Some(Payload::Array(x)), Some(Payload::Array(y))) => {
                    x.len() == y.len()
                        && x.iter().zip(y.iter()).all(|(e, f)| {
                            let eo = operand_of_rtval(ctx, e);
                            let fo = operand_of_rtval(ctx, f);
                            jit_values_equal(ctx, &eo, &fo, depth + 1)
                        })
                }
                _ => false,
            }
        }
        (EqKind::Struct, EqKind::Struct) => {
            if a.word == b.word {
                return true;
            }
            match payloads(a.word, b.word) {
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
                                let eo = operand_of_rtval(ctx, e);
                                let fo = operand_of_rtval(ctx, f);
                                jit_values_equal(ctx, &eo, &fo, depth + 1)
                            }
                        })
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// `==`/`!=` with the exact interpreter rule (`ops::equality`),
/// help included.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_values_eq(
    ctx: *mut JitTaskCtx,
    tag_a: u64,
    a: u64,
    tag_b: u64,
    b: u64,
    want_eq: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let oa = match eq_snapshot(ctx, "values_eq", tag_a, a, span) {
        Some(o) => o,
        None => return 0,
    };
    let ob = match eq_snapshot(ctx, "values_eq", tag_b, b, span) {
        Some(o) => o,
        None => return 0,
    };
    if !eq_same_type(&oa, &ob) {
        record(
            ctx,
            "E0203",
            format!(
                "cannot compare {} with {}",
                eq_name(oa.kind),
                eq_name(ob.kind)
            ),
            span,
            eq_hint(&oa, &ob),
        );
        return 0;
    }
    let eq = jit_values_equal(ctx, &oa, &ob, 0);
    u64::from(if want_eq != 0 { eq } else { !eq })
}

/// `<`/`<=`/`>`/`>=` with the exact interpreter rule
/// (`ops::ordering`), help included. `op` is 0/1/2/3 for
/// `Lt`/`LtEq`/`Gt`/`GtEq`.
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_values_order(
    ctx: *mut JitTaskCtx,
    op: u64,
    tag_a: u64,
    a: u64,
    tag_b: u64,
    b: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let oa = match eq_snapshot(ctx, "values_order", tag_a, a, span) {
        Some(o) => o,
        None => return 0,
    };
    let ob = match eq_snapshot(ctx, "values_order", tag_b, b, span) {
        Some(o) => o,
        None => return 0,
    };
    let ord = match (oa.kind, ob.kind) {
        (EqKind::Int, EqKind::Int) => (a as i64).cmp(&(b as i64)),
        (EqKind::Float, EqKind::Float) => match f64::from_bits(a).partial_cmp(&f64::from_bits(b)) {
            Some(o) => o,
            None => return 0,
        },
        (EqKind::Str, EqKind::Str) => {
            let pair = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                (payload_owned(&st.heap, a), payload_owned(&st.heap, b))
            };
            match pair {
                (Some(Payload::Str(x)), Some(Payload::Str(y))) => x.cmp(&y),
                _ => return 0,
            }
        }
        (EqKind::Chan, EqKind::Chan) => std::cmp::Ordering::Equal,
        _ => {
            record(
                ctx,
                "E0203",
                format!("cannot order {} and {}", eq_name(oa.kind), eq_name(ob.kind)),
                span,
                eq_hint(&oa, &ob),
            );
            return 0;
        }
    };
    let result = match op {
        0 => ord == std::cmp::Ordering::Less,
        1 => ord != std::cmp::Ordering::Greater,
        2 => ord == std::cmp::Ordering::Greater,
        3 => ord != std::cmp::Ordering::Less,
        _ => {
            internal(
                ctx,
                format!("values_order with unknown operator {op}"),
                span,
            );
            return 0;
        }
    };
    u64::from(result)
}

fn task_builtin_name(id: u64) -> Option<&'static str> {
    toto_compiler::builtins::BUILTIN_NAMES
        .get(id as usize)
        .copied()
}

fn builtin_arity(ctx: &mut JitTaskCtx, name: &str, got: usize, want: usize, span: Span) {
    record(
        ctx,
        "E0204",
        format!("wrong number of arguments: `{name}` takes {want}, got {got}"),
        span,
        None,
    );
}

/// Native `len` (see the gate's `builtin_len`).
fn builtin_len(ctx: &mut JitTaskCtx, decoded: &[RtVal], span: Span) -> u64 {
    if decoded.len() != 1 {
        builtin_arity(ctx, "len", decoded.len(), 1, span);
        return 0;
    }
    let out: Option<u64> = {
        let st = ctx.shared.state.lock().expect("shared state lock");
        match &decoded[0] {
            RtVal::Heap(r) => match st.heap.get(*r) {
                Some(Payload::Str(s)) => Some(s.chars().count() as u64),
                Some(Payload::Array(elems)) => Some(elems.len() as u64),
                _ => None,
            },
            _ => None,
        }
    };
    match out {
        Some(n) => n,
        None => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                decoded[0].type_name(&st.heap)
            };
            record(
                ctx,
                "E0210",
                format!("`len` needs a string or array, got {tn}"),
                span,
                None,
            );
            0
        }
    }
}

/// Native `push` (see the gate's `builtin_push`): one exclusive
/// guard for validate + mutate (atomic under M:N).
fn builtin_push(ctx: &mut JitTaskCtx, decoded: &[RtVal], span: Span) -> u64 {
    if decoded.len() != 2 {
        builtin_arity(ctx, "push", decoded.len(), 2, span);
        return 0;
    }
    enum Out {
        Len(u64),
        NotArray,
        Stale,
    }
    let out = match &decoded[0] {
        RtVal::Heap(r) => {
            let r = *r;
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            match st.heap.get_mut(r) {
                Some(Payload::Array(elems)) => {
                    elems.push(decoded[1].clone());
                    Out::Len(elems.len() as u64)
                }
                Some(_) => Out::NotArray,
                None => Out::Stale,
            }
        }
        _ => Out::NotArray,
    };
    match out {
        Out::Len(n) => n,
        Out::NotArray => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                decoded[0].type_name(&st.heap)
            };
            record(
                ctx,
                "E0210",
                format!("`push` needs an array, got {tn}"),
                span,
                None,
            );
            0
        }
        Out::Stale => {
            internal(ctx, String::from("stale handle in push"), span);
            0
        }
    }
}

/// Native `pop` (see the gate's `builtin_pop`).
fn builtin_pop(ctx: &mut JitTaskCtx, decoded: &[RtVal], span: Span) -> u64 {
    if decoded.len() != 1 {
        builtin_arity(ctx, "pop", decoded.len(), 1, span);
        return 0;
    }
    enum Out {
        Value(RtVal),
        Empty,
        NotArray,
        Stale,
    }
    let out = match &decoded[0] {
        RtVal::Heap(r) => {
            let r = *r;
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            match st.heap.get_mut(r) {
                Some(Payload::Array(elems)) => match elems.pop() {
                    Some(v) => Out::Value(v),
                    None => Out::Empty,
                },
                Some(_) => Out::NotArray,
                None => Out::Stale,
            }
        }
        _ => Out::NotArray,
    };
    match out {
        Out::Value(v) => match encode_val(&v) {
            Some(w) => w,
            None => {
                internal(ctx, String::from("pop value is not JIT-trafficable"), span);
                0
            }
        },
        Out::Empty => {
            record(
                ctx,
                "E0210",
                String::from("pop from empty array"),
                span,
                None,
            );
            0
        }
        Out::NotArray => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                decoded[0].type_name(&st.heap)
            };
            record(
                ctx,
                "E0210",
                format!("`pop` needs an array, got {tn}"),
                span,
                None,
            );
            0
        }
        Out::Stale => {
            internal(ctx, String::from("stale handle in pop"), span);
            0
        }
    }
}

/// Blocking-pool string argument (mirrors `exec::blocking_str`
/// exactly, including the internal-`E0204` shape: the checker
/// guarantees strings statically).
fn blocking_arg(ctx: &mut JitTaskCtx, v: &RtVal, name: &str, span: Span) -> Option<String> {
    let s = match v {
        RtVal::Heap(r) => {
            let st = ctx.shared.state.lock().expect("shared state lock");
            match st.heap.get(*r) {
                Some(Payload::Str(s)) => Some(s.clone()),
                _ => None,
            }
        }
        _ => None,
    };
    match s {
        Some(s) => Some(s),
        None => {
            let tn = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                v.type_name(&st.heap)
            };
            internal(
                ctx,
                format!("`{name}` argument must be a string, got {tn}"),
                span,
            );
            None
        }
    }
}

/// File-system builtins via the dedicated blocking pool (mirrors
/// `exec::builtin_blocking`: arity, fail-fast with op `blocking`,
/// safepoint, suspend, response mapping). Trace frames are appended
/// caller-side by `toto_check_failed`, except `Cancelled`, which sets
/// `skip_frame` (mirrors the executor popping its builtin frame).
fn builtin_blocking(
    ctx: &mut JitTaskCtx,
    name: &'static str,
    decoded: &[RtVal],
    span: Span,
) -> u64 {
    let op = match name {
        "read_file" => {
            if decoded.len() != 1 {
                builtin_arity(ctx, name, decoded.len(), 1, span);
                return 0;
            }
            match blocking_arg(ctx, &decoded[0], name, span) {
                Some(path) => BlockingOp::ReadFile { path },
                None => return 0,
            }
        }
        "write_file" => {
            if decoded.len() != 2 {
                builtin_arity(ctx, name, decoded.len(), 2, span);
                return 0;
            }
            let path = match blocking_arg(ctx, &decoded[0], name, span) {
                Some(p) => p,
                None => return 0,
            };
            let contents = match blocking_arg(ctx, &decoded[1], name, span) {
                Some(c) => c,
                None => return 0,
            };
            BlockingOp::WriteFile {
                path,
                contents: contents.into_bytes(),
            }
        }
        "file_exists" => {
            if decoded.len() != 1 {
                builtin_arity(ctx, name, decoded.len(), 1, span);
                return 0;
            }
            match blocking_arg(ctx, &decoded[0], name, span) {
                Some(path) => BlockingOp::FileExists { path },
                None => return 0,
            }
        }
        _ => {
            internal(ctx, String::from("non-blocking builtin"), span);
            return 0;
        }
    };
    if scope_failed(ctx) {
        record(
            ctx,
            "E0214",
            String::from("task cancelled at `blocking`: scope failed (released by cancellation)"),
            span,
            None,
        );
        return 0;
    }
    maybe_collect(ctx);
    let yielder = match ctx.yielder {
        Some(y) => y,
        None => {
            internal(
                ctx,
                String::from("blocking suspend with no yielder installed"),
                span,
            );
            return 0;
        }
    };
    // SAFETY: the yielder points into this task's own coroutine stack
    // (see the struct docs); suspending here parks the task with JIT
    // frames live, resuming with the pool completion.
    match unsafe { &*yielder }.suspend(ToSched::Blocking { op, span }) {
        ToTask::Value(v) => match encode_val(&v) {
            Some(w) => w,
            None => {
                internal(
                    ctx,
                    String::from("blocking result is not JIT-trafficable"),
                    span,
                );
                0
            }
        },
        ToTask::OpPanic { diag } => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        ToTask::Cancelled { op } => {
            ctx.skip_frame = true;
            record(
                ctx,
                "E0214",
                format!("task cancelled at `{op}`: scope failed (released by cancellation)"),
                span,
                None,
            );
            0
        }
        _ => {
            internal(ctx, String::from("blocking response"), span);
            0
        }
    }
}

/// Shared-implementation builtins (see the gate's `builtin_conv`).
fn builtin_conv(ctx: &mut JitTaskCtx, name: &'static str, decoded: Vec<RtVal>, span: Span) -> u64 {
    use std::collections::HashMap;
    let mut memo = HashMap::new();
    let mut args = Vec::with_capacity(decoded.len());
    for v in &decoded {
        let c = {
            let st = ctx.shared.state.lock().expect("shared state lock");
            toto_runtime::conv::to_value_memo(v, &st.heap, &mut memo)
        };
        match c {
            Ok(c) => args.push(c),
            Err(_) => {
                internal(
                    ctx,
                    format!("builtin `{name}` argument is not convertible"),
                    span,
                );
                return 0;
            }
        }
    }
    let fns = Arc::clone(&ctx.fns);
    let mut out_guard = ctx.shared.output.lock().expect("output lock");
    let out = toto_compiler::builtins::call_builtin(name, args, span, &fns, &mut out_guard);
    drop(out_guard);
    match out {
        Ok(v) => {
            let back = {
                let mut st = ctx.shared.state.lock().expect("shared state lock");
                toto_runtime::conv::from_value(&v, &mut st.heap)
            };
            match back {
                Ok(r) => match encode_val(&r) {
                    Some(w) => w,
                    None => {
                        internal(
                            ctx,
                            String::from("builtin result is not JIT-trafficable"),
                            span,
                        );
                        0
                    }
                },
                Err(_) => {
                    internal(ctx, String::from("function from builtin"), span);
                    0
                }
            }
        }
        Err((diag, _)) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
    }
}

/// Dispatch one builtin call (see the gate's `toto_builtin` for the
/// argument layout). File builtins park on the blocking pool;
/// everything else runs inline.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_builtin(
    ctx: *mut JitTaskCtx,
    name_id: u64,
    args_ptr: u64,
    nargs: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let name = match task_builtin_name(name_id) {
        Some(n) => n,
        None => {
            internal(ctx, format!("builtin id {name_id} out of range"), span);
            return 0;
        }
    };
    if matches!(name, "chan" | "close" | "closed") {
        internal(
            ctx,
            format!("builtin `{name}` reached generic dispatch"),
            span,
        );
        return 0;
    }
    let n = nargs as usize;
    let words = unsafe { std::slice::from_raw_parts(args_ptr as *const u64, n.saturating_mul(2)) };
    let mut decoded = Vec::with_capacity(n);
    for i in 0..n {
        match decode_inner(ctx, "builtin", words[2 * i], words[2 * i + 1], span) {
            Some(v) => decoded.push(v),
            None => return 0,
        }
    }
    match name {
        "len" => builtin_len(ctx, &decoded, span),
        "push" => builtin_push(ctx, &decoded, span),
        "pop" => builtin_pop(ctx, &decoded, span),
        "read_file" | "write_file" | "file_exists" => builtin_blocking(ctx, name, &decoded, span),
        _ => builtin_conv(ctx, name, decoded, span),
    }
}

/// Suspend with `req` (the caller holds no locks). Missing yielder
/// records internal `E0204` (the entry always installs one).
fn suspend(ctx: &mut JitTaskCtx, req: ToSched, span: Span) -> Option<ToTask> {
    // Copy out first (statement borrow ends here): matching on the
    // field directly would hold a context borrow across `internal`.
    let yielder = ctx.yielder;
    let yielder = match yielder {
        Some(y) => y,
        None => {
            internal(ctx, String::from("suspend with no yielder installed"), span);
            return None;
        }
    };
    // SAFETY: the yielder points into this task's own coroutine stack
    // (see the struct docs); suspending here parks the task with JIT
    // frames live, resuming with the scheduler response.
    Some(unsafe { &*yielder }.suspend(req))
}

fn cancelled(ctx: &mut JitTaskCtx, op: &'static str, span: Span) {
    record(
        ctx,
        "E0214",
        format!("task cancelled at `{op}`: scope failed (released by cancellation)"),
        span,
        None,
    );
}

/// Normalize a scheduler-delivered value to JIT shape, allocating
/// heap payloads for immediate-shaped values (`Range`/`Ok`/`Err`/
/// `Some` zero values). Callers root first (see `response_value`).
fn normalize_value(ctx: &mut JitTaskCtx, v: RtVal, span: Span) -> Option<RtVal> {
    match v {
        RtVal::Range(a, b) => {
            maybe_collect(ctx);
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            Some(RtVal::Heap(st.heap.alloc_range(a, b)))
        }
        RtVal::Ok(inner) => {
            let n = normalize_value(ctx, *inner, span)?;
            maybe_collect(ctx);
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            Some(RtVal::Heap(st.heap.alloc_result(true, n)))
        }
        RtVal::Err(inner) => {
            let n = normalize_value(ctx, *inner, span)?;
            maybe_collect(ctx);
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            Some(RtVal::Heap(st.heap.alloc_result(false, n)))
        }
        RtVal::Some(inner) => {
            let n = normalize_value(ctx, *inner, span)?;
            maybe_collect(ctx);
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            Some(RtVal::Heap(st.heap.alloc_some(n)))
        }
        RtVal::Closure(_) => {
            internal(
                ctx,
                String::from("interpreter closure in a scheduler response"),
                span,
            );
            None
        }
        other => Some(other),
    }
}

/// Convert a scheduler-delivered value to JIT words. Delivered
/// handles are pushed permanently to this task's shadow slot first:
/// conversion may collect, and the value is not yet in any local, so
/// without the push a collection could reclaim it mid-conversion.
/// (Conservative for the run's lifetime; precise response-root
/// scoping is a carry-forward.)
fn response_value(ctx: &mut JitTaskCtx, v: RtVal, span: Span) -> Option<u64> {
    {
        let mut refs = Vec::new();
        toto_runtime::value::heap_refs_of(&v, &mut refs);
        let mut st = ctx.shared.state.lock().expect("shared state lock");
        if let Some(shadow) = st.shadows.get_mut(ctx.task.0) {
            for r in refs {
                shadow.push(r);
            }
        }
    }
    let n = normalize_value(ctx, v, span)?;
    match encode_val(&n) {
        Some(w) => Some(w),
        None => {
            internal(
                ctx,
                String::from("response value is not JIT-trafficable"),
                span,
            );
            None
        }
    }
}

/// Explicit-check failure: record the Toto panic and return a dummy
/// word (the sentinel protocol — generated code checks
/// `toto_check_failed` after the call and unwinds explicitly).
/// `msg_ptr`/`msg_len` address JIT-owned static message bytes.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_panic(
    ctx: *mut JitTaskCtx,
    code: u64,
    start: u64,
    end: u64,
    msg_ptr: u64,
    msg_len: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let bytes = unsafe { std::slice::from_raw_parts(msg_ptr as *const u8, msg_len as usize) };
    let message = String::from_utf8_lossy(bytes).into_owned();
    record(
        ctx,
        code_str(code),
        message,
        Span::new(start as u32, end as u32),
        None,
    );
    0
}

/// Sentinel check after every call that can record: returns 0 while
/// healthy; on failure appends the callee frame (`name`, call-site
/// span — the caller knows both statically, exactly like the
/// executor pushing at call time) and returns 1, telling generated
/// code to return a dummy word immediately. Honors `skip_frame`
/// (blocking-`Cancelled` only).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_check_failed(
    ctx: *mut JitTaskCtx,
    name_ptr: u64,
    name_len: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    if !ctx.failed {
        return 0;
    }
    if ctx.skip_frame {
        ctx.skip_frame = false;
        return 1;
    }
    let bytes = unsafe { std::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let name = String::from_utf8_lossy(bytes).into_owned();
    ctx.trace.push(TraceFrame {
        name,
        span: Span::new(start as u32, end as u32),
    });
    1
}

/// Create a channel (reserved `toto_chan_make`): `elem_ty` indexes
/// the type table, `cap` is `Some` iff `has_cap` is nonzero.
/// Never blocks (`yield_now` semantics: no fail-fast, safepoint,
/// suspend). Failures mirror the executor exactly.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_chan_make(
    ctx: *mut JitTaskCtx,
    has_cap: u64,
    cap: u64,
    elem_ty: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let elem = ctx.chan_types.get(elem_ty as usize).cloned();
    let elem = match elem {
        Some(t) => t,
        None => {
            internal(
                ctx,
                format!("channel element type id {elem_ty} out of range"),
                span,
            );
            return 0;
        }
    };
    let cap = if has_cap != 0 { Some(cap as i64) } else { None };
    if cap.is_some_and(|n| n < 0) {
        record(
            ctx,
            "E0204",
            format!("negative channel capacity {}", cap.unwrap_or(0)),
            span,
            None,
        );
        return 0;
    }
    maybe_collect(ctx);
    match suspend(ctx, ToSched::ChanNew { cap, elem, span }, span) {
        Some(ToTask::Value(RtVal::Chan(c))) => c.0 as u64,
        Some(ToTask::Value(_)) => {
            internal(ctx, String::from("non-channel from ChanNew"), span);
            0
        }
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("channel creation response"), span);
            0
        }
    }
}

/// Spawn a `go` task (reserved `toto_spawn`): `body` is the
/// compiled-body id, `(caps_ptr, ncaps)` addresses `(tag, word)`
/// pairs. Never blocks. Capture handles are pushed permanently to
/// this task's shadow slot: the child may not run (and publish its
/// own roots) before the parent drops them, so the parent roots them
/// for the run's lifetime (conservative; precise spawn-root handoff
/// is a carry-forward).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_spawn(
    ctx: *mut JitTaskCtx,
    body: u64,
    caps_ptr: u64,
    ncaps: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let entry = ctx.bodies.get(&(body as u32)).copied();
    let entry = match entry {
        Some(e) => e,
        None => {
            internal(ctx, format!("unknown spawn body id {body}"), span);
            return 0;
        }
    };
    let n = ncaps as usize;
    let words = unsafe { std::slice::from_raw_parts(caps_ptr as *const u64, n.saturating_mul(2)) };
    let mut captures = Vec::with_capacity(n);
    for i in 0..n {
        let (tag, word) = (words[2 * i], words[2 * i + 1]);
        if tag == TAG_HANDLE {
            let live = {
                let st = ctx.shared.state.lock().expect("shared state lock");
                st.heap.get(decode(word)).is_some()
            };
            if !live {
                internal(
                    ctx,
                    format!("stale handle in spawn capture {i} (value {word:#x} is not live)"),
                    span,
                );
                return 0;
            }
            let mut st = ctx.shared.state.lock().expect("shared state lock");
            if let Some(shadow) = st.shadows.get_mut(ctx.task.0) {
                shadow.push(decode(word));
            }
        }
        captures.push(word);
    }
    let child = Box::new(JitTaskCtx {
        shared: Arc::clone(&ctx.shared),
        task: TaskId(usize::MAX),
        scope: ScopeId(usize::MAX),
        yielder: None,
        entry,
        captures,
        root_name: String::from("go"),
        root_span: span,
        is_main: false,
        failed: false,
        diag: None,
        skip_frame: false,
        trace: Vec::new(),
        fns: Arc::clone(&ctx.fns),
        entries: Arc::clone(&ctx.entries),
        bodies: Arc::clone(&ctx.bodies),
        func_names: Arc::clone(&ctx.func_names),
        chan_types: Arc::clone(&ctx.chan_types),
    });
    let child_u64 = Box::into_raw(child) as u64;
    let scope = ctx.scope;
    maybe_collect(ctx);
    match suspend(
        ctx,
        ToSched::SpawnNative {
            entry: jit_task_entry,
            ctx: child_u64,
            span,
            scope,
        },
        span,
    ) {
        Some(ToTask::Unit) => 0,
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("spawn response"), span);
            0
        }
    }
}

/// Yield op codes for `task_yield` (shared with codegen).
pub const YIELD_SEND: u64 = 0;
pub const YIELD_RECV: u64 = 1;
pub const YIELD_SELECT: u64 = 2;
pub const YIELD_CLOSE: u64 = 3;
pub const YIELD_CLOSED: u64 = 4;

fn yield_send(ctx: &mut JitTaskCtx, chan_w: u64, vtag: u64, vword: u64, span: Span) -> u64 {
    let v = match decode_inner(ctx, "send", vtag, vword, span) {
        Some(v) => v,
        None => return 0,
    };
    if scope_failed(ctx) {
        cancelled(ctx, "send", span);
        return 0;
    }
    // The value aliases a published local (same handle), so the
    // pre-suspend collection cannot reclaim it.
    maybe_collect(ctx);
    match suspend(
        ctx,
        ToSched::Send {
            chan: ChannelId(chan_w as usize),
            value: v,
            span,
        },
        span,
    ) {
        Some(ToTask::Unit) => 0,
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("send response"), span);
            0
        }
    }
}

fn yield_recv(ctx: &mut JitTaskCtx, chan_w: u64, span: Span) -> u64 {
    if scope_failed(ctx) {
        cancelled(ctx, "recv", span);
        return 0;
    }
    maybe_collect(ctx);
    match suspend(
        ctx,
        ToSched::Recv {
            chan: ChannelId(chan_w as usize),
            span,
        },
        span,
    ) {
        Some(ToTask::Value(v)) => response_value(ctx, v, span).unwrap_or(0),
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("receive response"), span);
            0
        }
    }
}

fn yield_select(
    ctx: &mut JitTaskCtx,
    cases_ptr: u64,
    ncases: u64,
    has_default: u64,
    out_ptr: u64,
    span: Span,
) -> u64 {
    // Cases are 6-word records in generated-code stack memory:
    // (kind, chan, vtag, vword, cstart, cend). Kind 0 receives, kind
    // 1 sends; the received value (when any) lands in `out_ptr`.
    let n = ncases as usize;
    let words = unsafe { std::slice::from_raw_parts(cases_ptr as *const u64, n.saturating_mul(6)) };
    let mut cases = Vec::with_capacity(n);
    for i in 0..n {
        let base = 6 * i;
        let (kind, chan_w, vtag, vword) = (
            words[base],
            words[base + 1],
            words[base + 2],
            words[base + 3],
        );
        let cspan = Span::new(words[base + 4] as u32, words[base + 5] as u32);
        match kind {
            0 => cases.push(SelReq::Recv {
                chan: ChannelId(chan_w as usize),
                span: cspan,
            }),
            1 => {
                let v = match decode_inner(ctx, "select", vtag, vword, cspan) {
                    Some(v) => v,
                    None => return 0,
                };
                cases.push(SelReq::Send {
                    chan: ChannelId(chan_w as usize),
                    value: v,
                    span: cspan,
                });
            }
            _ => {
                internal(ctx, format!("select case kind {kind} out of range"), span);
                return 0;
            }
        }
    }
    if scope_failed(ctx) {
        cancelled(ctx, "select", span);
        return 0;
    }
    maybe_collect(ctx);
    match suspend(
        ctx,
        ToSched::Select {
            cases,
            has_default: has_default != 0,
            span,
        },
        span,
    ) {
        Some(ToTask::Selected { index, value }) => {
            let out_w = match value {
                Some(v) => response_value(ctx, v, span).unwrap_or(0),
                None => 0,
            };
            unsafe {
                (out_ptr as *mut u64).write(out_w);
            }
            index as u64
        }
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("select response"), span);
            0
        }
    }
}

fn yield_close(ctx: &mut JitTaskCtx, chan_w: u64, span: Span) -> u64 {
    // `yield_now` semantics (no fail-fast): close completes even in
    // a failed scope.
    maybe_collect(ctx);
    match suspend(
        ctx,
        ToSched::Close {
            chan: ChannelId(chan_w as usize),
            span,
        },
        span,
    ) {
        Some(ToTask::Unit) => 0,
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("close response"), span);
            0
        }
    }
}

fn yield_closed(ctx: &mut JitTaskCtx, chan_w: u64, span: Span) -> u64 {
    maybe_collect(ctx);
    match suspend(
        ctx,
        ToSched::Closed {
            chan: ChannelId(chan_w as usize),
            span,
        },
        span,
    ) {
        Some(ToTask::Value(v)) => match encode_val(&v) {
            Some(w) => w,
            None => {
                internal(
                    ctx,
                    String::from("closed result is not JIT-trafficable"),
                    span,
                );
                0
            }
        },
        Some(ToTask::OpPanic { diag }) => {
            record(ctx, diag.code, diag.message, diag.span, diag.help);
            0
        }
        Some(ToTask::Cancelled { op }) => {
            cancelled(ctx, op, span);
            0
        }
        Some(_) | None => {
            internal(ctx, String::from("closed response"), span);
            0
        }
    }
}

/// Suspension-point dispatch (reserved `toto_yield`): `op` selects
/// send/recv/select/close/closed; the remaining words are op
/// operands (see each arm). May park the task: roots are already
/// published (a `Park` safepoint precedes every suspending op).
#[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
pub unsafe extern "C" fn task_yield(
    ctx: *mut JitTaskCtx,
    op: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    _a4: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    match op {
        YIELD_SEND => yield_send(ctx, a0, a1, a2, span),
        YIELD_RECV => yield_recv(ctx, a0, span),
        YIELD_SELECT => yield_select(ctx, a0, a1, a2, a3, span),
        YIELD_CLOSE => yield_close(ctx, a0, span),
        YIELD_CLOSED => yield_closed(ctx, a0, span),
        _ => {
            internal(ctx, format!("yield op {op} out of range"), span);
            0
        }
    }
}

/// Fetch one go-body capture word by static index.
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_capture(ctx: *mut JitTaskCtx, idx: u64, start: u64, end: u64) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let word = ctx.captures.get(idx as usize).copied();
    match word {
        Some(w) => w,
        None => {
            internal(ctx, format!("task capture index {idx} out of range"), span);
            0
        }
    }
}

/// Resolve a top-level function index to its compiled entry address
/// (indirect calls through `Func` values).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_func_entry(
    ctx: *mut JitTaskCtx,
    idx: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let name = ctx.func_names.get(idx as usize).cloned();
    let name = match name {
        Some(n) => n,
        None => {
            internal(ctx, format!("function index {idx} out of range"), span);
            return 0;
        }
    };
    let entry = ctx.entries.get(&name).copied();
    match entry {
        Some(e) => e as u64,
        None => {
            internal(
                ctx,
                format!("function `{name}` has no compiled entry"),
                span,
            );
            0
        }
    }
}

/// Resolve a compiled-body id to its entry address (indirect calls
/// through closure values).
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn task_body_entry(
    ctx: *mut JitTaskCtx,
    body: u64,
    start: u64,
    end: u64,
) -> u64 {
    let ctx = tctx(ctx);
    let span = Span::new(start as u32, end as u32);
    let entry = ctx.bodies.get(&(body as u32)).copied();
    match entry {
        Some(e) => e as u64,
        None => {
            internal(ctx, format!("unknown call body id {body}"), span);
            0
        }
    }
}

/// Shared compiled-program tables: built once per compile, cloned
/// cheaply (`Arc`) per spawn. Function declarations stay with the
/// runner (only it sees the AST).
pub struct JitSession {
    pub entries: Arc<HashMap<String, usize>>,
    pub bodies: Arc<HashMap<u32, usize>>,
    pub func_names: Arc<Vec<String>>,
    pub chan_types: Arc<Vec<Ty>>,
}

/// Task body for natively compiled code: installs the assignment,
/// pushes the root frame, and calls the compiled entry
/// (`(task_ctx_ptr) -> u64`). Frees the context box on completion.
/// The trace unwinds innermost-first and is reversed here to
/// outermost-first, like the executor's.
pub fn jit_task_entry(
    y: &Yielder<ToTask, ToSched>,
    ctx: u64,
    id: TaskId,
    scope: ScopeId,
) -> TaskResult {
    let ctxp = ctx as *mut JitTaskCtx;
    let entry = {
        let ctxr = tctx(ctxp);
        ctxr.task = id;
        ctxr.scope = scope;
        ctxr.yielder = Some(y as *const Yielder<ToTask, ToSched>);
        ctxr.trace.push(TraceFrame {
            name: ctxr.root_name.clone(),
            span: ctxr.root_span,
        });
        ctxr.entry
    };
    // SAFETY: `entry` is a finalized compiled function taking the
    // task-context pointer and returning a word; the context box is
    // live for the task's duration.
    let out = unsafe {
        let f: unsafe extern "C" fn(u64) -> u64 = std::mem::transmute(entry as *const u8);
        f(ctxp as u64)
    };
    let result = {
        let ctxr = tctx(ctxp);
        if ctxr.failed {
            let diag = ctxr.diag.clone().unwrap_or_else(|| {
                Diag::new(
                    "E0204",
                    String::from("internal error: failed without a recorded diag (unreachable)"),
                    Span::new(0, 0),
                    None,
                )
            });
            let mut trace = Vec::with_capacity(ctxr.trace.len());
            if let Some((root, rest)) = ctxr.trace.split_first() {
                trace.push(root.clone());
                trace.extend(rest.iter().rev().cloned());
            }
            TaskResult::Panic { diag, trace }
        } else if ctxr.is_main {
            TaskResult::Return(RtVal::Int(out as i64))
        } else {
            TaskResult::Return(RtVal::Unit)
        }
    };
    unsafe {
        drop(Box::from_raw(ctxp));
    }
    result
}

fn zero_stats() -> RunStats {
    RunStats {
        tasks_spawned: 0,
        switches: 0,
        steals: 0,
        high_water_max: 0,
        collections: 0,
        allocated: 0,
        reclaimed: 0,
    }
}

/// A live background run (Phase 4 dev): observe it (`live_tasks`),
/// cancel it (`cancel`), test completion (`is_finished`), and join it
/// (`join`, which composes the outcome like `run_jit`). A panicked
/// run thread surfaces as an internal-error outcome, never a panic
/// in the driver.
pub struct JitRun {
    sched: Arc<Scheduler>,
    join: std::thread::JoinHandle<toto_runtime::sched::SchedOutcome>,
    main_span: Span,
}

impl JitRun {
    pub fn live_tasks(&self) -> usize {
        self.sched.live_tasks()
    }

    pub fn cancel(&self) {
        self.sched.cancel_all();
    }

    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    pub fn join(self) -> toto_runtime::RtOutcome {
        match self.join.join() {
            Ok(outcome) => toto_runtime::compose(outcome, self.main_span),
            Err(_) => toto_runtime::RtOutcome {
                result: Err((
                    Diag::new(
                        "E0204",
                        String::from(
                            "internal error: run thread panicked (unreachable in checked programs)",
                        ),
                        self.main_span,
                        None,
                    ),
                    Vec::new(),
                )),
                stdout: Vec::new(),
                stats: zero_stats(),
            },
        }
    }
}

/// Spawn a checked program run in the background (Phase 4 dev):
/// returns the live handle immediately. `Err` carries an immediate
/// outcome when no run starts (missing main/entry — unreachable in
/// checked programs). The blocking `run_jit` is spawn-plus-join.
#[allow(clippy::result_large_err)]
pub fn spawn_jit_run(
    program: &Program,
    checked: &CheckedProgram,
    config: toto_runtime::RuntimeConfig,
    session: &JitSession,
) -> Result<JitRun, toto_runtime::RtOutcome> {
    let prog = toto_compiler::mir::lower(checked);
    let mut structs: HashMap<String, HStructDecl> = HashMap::new();
    for (name, decl) in &checked.structs {
        structs.insert(name.clone(), decl.clone());
    }
    let mut fns: Vec<FnDecl> = Vec::new();
    for item in &program.items {
        if let ItemKind::Fn(decl) = &item.kind {
            fns.push(decl.clone());
        }
    }
    let fn_blocks: Vec<Arc<[toto_compiler::mir::MBlock]>> = prog
        .fns
        .iter()
        .map(|f| Arc::from(f.blocks.as_slice()))
        .collect();
    if !prog.fn_index.contains_key("main") {
        return Err(toto_runtime::RtOutcome {
            result: Err((
                Diag::new(
                    "E0204",
                    "program has no `fn main() -> int`".to_string(),
                    Span::new(0, 0),
                    None,
                ),
                Vec::new(),
            )),
            stdout: Vec::new(),
            stats: zero_stats(),
        });
    }
    let main_span = prog.main_span;
    let main_entry = match session.entries.get("main").copied() {
        Some(e) => e,
        None => {
            return Err(toto_runtime::RtOutcome {
                result: Err((
                    Diag::new(
                        "E0204",
                        String::from(
                            "internal error: JIT main entry missing (unreachable in checked programs)",
                        ),
                        main_span,
                        None,
                    ),
                    Vec::new(),
                )),
                stdout: Vec::new(),
                stats: zero_stats(),
            });
        }
    };
    let code = Arc::new(ExecCode {
        prog,
        structs,
        fns: fns.clone(),
        fn_blocks,
    });
    let sched = Arc::new(Scheduler::new(
        code,
        config.gc_threshold_bytes,
        config.rng_seed,
        config.stack_bytes,
        config.collect_at_every_safepoint_for_test,
        config.workers,
        config.blocking_pool_threads,
    ));
    let entries = Arc::clone(&session.entries);
    let bodies = Arc::clone(&session.bodies);
    let func_names = Arc::clone(&session.func_names);
    let chan_types = Arc::clone(&session.chan_types);
    let fns = Arc::new(fns);
    let ctx = Box::new(JitTaskCtx {
        shared: Arc::clone(sched.shared()),
        task: TaskId(usize::MAX),
        scope: ScopeId(usize::MAX),
        yielder: None,
        entry: main_entry,
        captures: Vec::new(),
        root_name: String::from("main"),
        root_span: main_span,
        is_main: true,
        failed: false,
        diag: None,
        skip_frame: false,
        trace: Vec::new(),
        fns,
        entries,
        bodies,
        func_names,
        chan_types,
    });
    let ctx_u64 = Box::into_raw(ctx) as u64;
    sched.spawn_native_main(jit_task_entry, ctx_u64, main_span);
    let sched2 = Arc::clone(&sched);
    let join = std::thread::Builder::new()
        .name("toto-jit".to_string())
        .stack_size(256 << 20)
        .spawn(move || sched2.run())
        .expect("failed to spawn jit thread");
    Ok(JitRun {
        sched,
        join,
        main_span,
    })
}

/// Run a checked program with JIT-compiled code on scheduler tasks
/// (mirrors `run_checked`: same lowering, thread, scheduler, and
/// compose — only the task bodies differ).
pub fn run_jit(
    program: &Program,
    checked: &CheckedProgram,
    config: toto_runtime::RuntimeConfig,
    session: &JitSession,
) -> toto_runtime::RtOutcome {
    match spawn_jit_run(program, checked, config, session) {
        Ok(run) => run.join(),
        Err(outcome) => outcome,
    }
}
