//! MIR executor (Phase 3, Stage 1): runs checked MIR on coroutine
//! stacks under the scheduler, on the GC heap.
//!
//! ONE executor, two modes. The scheduler always drives it — even
//! single-task programs run as one coroutine — and `exec_fn_host`
//! runs a sequential function inline on the host thread (unit tests,
//! no scheduler or coroutines). Both modes share the stepping code.
//!
//! Value semantics are borrowed, not reimplemented: operands convert
//! to the compiler's `interp::Value` and run through the shared `ops`
//! and `builtins` modules (`conv.rs`), so the executor agrees with the
//! tree-walker by construction. Conversion is cycle-safe (memoized),
//! preserving sharing and `==` cycle-fallback behavior.
//!
//! Differences from the sequential interpreters, all per spec:
//! - no 8,192-frame Toto call limit (§5.9 is interpreter-only); deep
//!   Toto recursion grows heap frames until the 64 KiB guard page
//!   aborts (reported, not asserted; final validation under JIT is a
//!   Phase 4 gate item);
//! - `Safepoint` instructions publish roots and collect on threshold;
//! - channel ops yield to the scheduler and may park the task.

use std::collections::HashMap;
use std::sync::Arc;

use corosensei::Yielder;
use toto_compiler::builtins;
use toto_compiler::diag::Diag;
use toto_compiler::hir::Ty;
use toto_compiler::interp::TraceFrame;
use toto_compiler::mir::{BlockId, MBinOp, MBlock, MConst, MInstr, MSelectCase, MTerm};
use toto_compiler::ops::{self, FnRetKind};
use toto_compiler::span::Span;

use crate::channel::{ChannelId, TaskId};
use crate::conv;
use crate::heap::{Heap, Payload};
use crate::sched::{ExecCode, ScopeId, Shared, ToSched, ToTask};
use crate::value::{RtClosure, RtVal};

/// How a task finished.
#[derive(Debug)]
pub enum TaskResult {
    Return(RtVal),
    Panic { diag: Diag, trace: Vec<TraceFrame> },
}

/// A Toto-level failure inside the task: always becomes a task panic
/// (the scheduler then fails the scope).
enum ExecErr {
    Toto(Diag),
}

impl From<Diag> for ExecErr {
    fn from(diag: Diag) -> Self {
        ExecErr::Toto(diag)
    }
}

impl From<(Diag, Vec<TraceFrame>)> for ExecErr {
    fn from((diag, carried): (Diag, Vec<TraceFrame>)) -> Self {
        // `ops`/`builtins` leaf errors carry no trace of their own; the
        // executor's accumulated trace is attached at task completion.
        debug_assert!(carried.is_empty(), "unexpected carried trace");
        ExecErr::Toto(diag)
    }
}

struct Frame {
    name: String,
    blocks: Arc<[MBlock]>,
    by_id: HashMap<BlockId, usize>,
    locals: Vec<RtVal>,
    block: BlockId,
    ip: usize,
    ret_ty: Ty,
    body_span: Span,
    /// Local receiving the call result (`None` for the task top frame).
    return_dst: Option<usize>,
}

impl Frame {
    fn ret_kind(&self) -> FnRetKind {
        match &self.ret_ty {
            Ty::Result(..) => FnRetKind::Result,
            Ty::Option(..) => FnRetKind::Option,
            _ => FnRetKind::Other,
        }
    }

    fn ret_is_unit(&self) -> bool {
        self.ret_ty == Ty::Unit
    }
}

/// Native task entry (Phase 4 JIT): run compiled code in a fresh
/// task instead of the MIR executor. Receives the task yielder (the
/// same `ToSched`/`ToTask` protocol the executor speaks), an opaque
/// context word owned by the native runtime, and the assigned task
/// id and scope. Returns how the task finished. A plain `fn` pointer
/// so the coroutine body holding it stays `'static`.
pub type NativeEntry = fn(&Yielder<ToTask, ToSched>, u64, TaskId, ScopeId) -> TaskResult;

/// What to run in a fresh task.
pub enum Initial {
    /// `main` by function index (no arguments; the checker guarantees it).
    Main(usize),
    /// A `go` body with its capture values (blocks precomputed at
    /// spawn so frames never clone the graph).
    GoBody {
        body: Arc<toto_compiler::mir::MirBody>,
        blocks: Arc<[MBlock]>,
        captures: Vec<RtVal>,
    },
    /// Native code (see `NativeEntry`). Dispatched in the coroutine
    /// body before the executor exists; reaching `run_task` is a
    /// harness bug reported as an internal error, never a crash.
    Native { entry: NativeEntry, ctx: u64 },
}

pub struct Executor<'a> {
    y: &'a Yielder<ToTask, ToSched>,
    code: &'a ExecCode,
    shared: &'a Shared,
    scope: ScopeId,
    task: TaskId,
    frames: Vec<Frame>,
    trace: Vec<TraceFrame>,
    task_result: Option<RtVal>,
    /// Fired-callback slots awaiting drain, mapped to their Toto
    /// function (function index or closure value).
    callbacks: std::collections::HashMap<u64, RtVal>,
}

impl<'a> Executor<'a> {
    pub fn new(
        y: &'a Yielder<ToTask, ToSched>,
        code: &'a ExecCode,
        shared: &'a Shared,
        scope: ScopeId,
        task: TaskId,
    ) -> Self {
        Self {
            y,
            code,
            shared,
            scope,
            task,
            frames: Vec::new(),
            trace: Vec::new(),
            task_result: None,
            callbacks: std::collections::HashMap::new(),
        }
    }

    pub fn run_task(&mut self, initial: Initial, call_span: Span) -> TaskResult {
        match initial {
            Initial::Main(idx) => {
                let mfn = &self.code.prog.fns[idx];
                self.trace.push(TraceFrame {
                    name: mfn.name.clone(),
                    span: call_span,
                });
                self.push_frame(
                    mfn.name.clone(),
                    self.code.fn_blocks[idx].clone(),
                    mfn.locals.len(),
                    Vec::new(),
                    Vec::new(),
                    0,
                    mfn.ret.clone(),
                    mfn.body_span,
                    None,
                );
            }
            Initial::GoBody {
                body,
                blocks,
                captures,
            } => {
                self.trace.push(TraceFrame {
                    name: "go".to_string(),
                    span: call_span,
                });
                let n_params = body.params.len();
                self.push_frame(
                    "go".to_string(),
                    blocks,
                    body.locals.len(),
                    Vec::new(),
                    captures,
                    n_params,
                    Ty::Unit,
                    body.body_span,
                    None,
                );
            }
            // The coroutine body dispatches `Native` before the
            // executor exists; reaching here is a harness bug. A
            // total internal error, never a crash.
            Initial::Native { .. } => {
                return TaskResult::Panic {
                    diag: Diag::new(
                        "E0204",
                        "internal error: native task reached the MIR executor (unreachable in checked programs)".to_string(),
                        call_span,
                        None,
                    ),
                    trace: std::mem::take(&mut self.trace),
                };
            }
        }
        match self.loop_frames() {
            Ok(v) => TaskResult::Return(v),
            Err(ExecErr::Toto(diag)) => TaskResult::Panic {
                diag,
                trace: std::mem::take(&mut self.trace),
            },
        }
    }

    // Frame construction genuinely takes nine fields (identity, code,
    // layout, inputs, return contract); bundling them would obscure
    // the two call sites rather than clarify them.
    #[allow(clippy::too_many_arguments)]
    fn push_frame(
        &mut self,
        name: String,
        blocks: Arc<[MBlock]>,
        n_locals: usize,
        args: Vec<RtVal>,
        captures: Vec<RtVal>,
        n_params: usize,
        ret_ty: Ty,
        body_span: Span,
        return_dst: Option<usize>,
    ) {
        let mut by_id = HashMap::with_capacity(blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            by_id.insert(b.id, i);
        }
        let mut locals = vec![RtVal::Unit; n_locals];
        for (i, a) in args.into_iter().enumerate() {
            locals[i] = a;
        }
        for (i, c) in captures.into_iter().enumerate() {
            locals[n_params + i] = c;
        }
        let entry = blocks.first().map(|b| b.id).unwrap_or(0);
        self.frames.push(Frame {
            name,
            blocks,
            by_id,
            locals,
            block: entry,
            ip: 0,
            ret_ty,
            body_span,
            return_dst,
        });
    }

    fn err(&self, code: &'static str, message: String, span: Span) -> ExecErr {
        ExecErr::Toto(Diag::new(code, message, span, None))
    }

    fn internal(&self, what: &str, span: Span) -> ExecErr {
        self.err(
            "E0204",
            format!("internal error: {what} (unreachable in checked programs)"),
            span,
        )
    }

    /// Publish this task's live heap references to its shadow slot.
    /// Must precede every safepoint and every park request (the frozen
    /// publish-before-safepoint rule, ADR-008).
    fn publish_roots(&self) {
        let mut out = Vec::new();
        for f in &self.frames {
            for v in &f.locals {
                crate::value::heap_refs_of(v, &mut out);
            }
        }
        let mut st = self.shared.state.lock().expect("shared state lock");
        if let Some(shadow) = st.shadows.get_mut(self.task.0) {
            shadow.publish(out);
        }
    }

    fn scope_failed(&self) -> bool {
        self.shared
            .scopes
            .lock()
            .expect("scopes lock")
            .is_failed_subtree(self.scope)
    }

    /// Yield a request that never blocks (spawn, channel creation,
    /// close/closed). No fail-fast check: these complete immediately
    /// even in a failed scope. Suspension points are collection
    /// safepoints (frozen ADR-008: allocation + switch safepoints), so
    /// the threshold check runs here as well as at allocations.
    fn yield_now(&self, req: ToSched) -> ToTask {
        self.maybe_collect();
        self.y.suspend(req)
    }

    /// Yield a request that may park (send, receive, select). In a
    /// failed scope the op fails fast with E0214 instead of blocking.
    /// A suspension is a safepoint: collect on threshold (roots are
    /// published, channel/select waiters are gathered as roots).
    fn park(&self, req: ToSched, span: Span, op: &'static str) -> Result<ToTask, ExecErr> {
        if self.scope_failed() {
            return Err(self.cancelled(op, span));
        }
        self.check_deadline(span, op)?;
        self.maybe_collect();
        match self.y.suspend(req) {
            ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
            resp => Ok(resp),
        }
    }

    /// ADR-009 deadline check (Wave 2, Stage 3): when the task's
    /// slot carries an absolute monotonic-ms deadline that has
    /// passed, fail fast with E0214 — the same fail-fast path as
    /// cancellation (the unwind fails the scope through the normal
    /// panic path, which is exactly "fails the scope with
    /// deadline-exceeded"). Checked at every pre-park fail-fast
    /// site (send/recv/select, blocking/file/foreign calls,
    /// network and TLS waits); running code that never suspends
    /// keeps running (cooperative limit, documented like drain
    /// cancellation). Preemptive mid-wait wakeups are a recorded
    /// follow-up, not this wave.
    fn check_deadline(&self, span: Span, op: &'static str) -> Result<(), ExecErr> {
        // Hot frame stays thin: this sits inside the deepest
        // serve-path nest at every suspension, and coroutine stacks
        // are spec-fixed 64 KiB (ADR-002/ADR-008) — the Wave 1 ARM
        // guard-fault precedent. The lock + compare inline; the cold
        // expiry formatting is outlined below so `format!`
        // temporaries never bloat this frame.
        let expired = {
            let sched = self.shared.sched.lock().expect("sched lock");
            match sched.slots[self.task.0].ctx.deadline {
                Some(d) => toto_compiler::ctxlog::monotonic_ms() > d,
                None => false,
            }
        };
        if expired {
            return Err(self.deadline_expired(span, op));
        }
        Ok(())
    }

    /// Cold deadline-expiry constructor (`#[inline(never)]` keeps the
    /// `format!` temporaries out of `check_deadline`'s hot frame —
    /// see above). Reached only when a deadline actually expired,
    /// never on the common no-deadline path.
    #[inline(never)]
    fn deadline_expired(&self, span: Span, op: &'static str) -> ExecErr {
        self.err(
            "E0214",
            format!("task cancelled at `{op}`: deadline exceeded (fail-fast)"),
            span,
        )
    }

    fn cancelled(&self, op: &'static str, span: Span) -> ExecErr {
        self.err(
            "E0214",
            format!("task cancelled at `{op}`: scope failed (released by cancellation)"),
            span,
        )
    }

    /// Convert to tree-walking values sharing one memo (reads heap state
    /// under the shared lock; results are fully owned). Cross-operand
    /// sharing is preserved (`a == a` on cyclic `a` stays true).
    fn to_interps(
        &self,
        vs: &[RtVal],
        span: Span,
    ) -> Result<Vec<toto_compiler::interp::Value>, ExecErr> {
        use std::collections::HashMap;
        let st = self.shared.state.lock().expect("shared state lock");
        let mut memo = HashMap::new();
        let mut out = Vec::with_capacity(vs.len());
        for v in vs {
            out.push(
                conv::to_value_memo(v, &st.heap, &mut memo)
                    .map_err(|_| self.internal("function at convertible position", span))?,
            );
        }
        Ok(out)
    }

    /// Single-value conversion (fresh memo; see `to_vals`).
    fn to_interp(&self, v: &RtVal) -> Result<toto_compiler::interp::Value, ExecErr> {
        let span = Span::new(0, 0);
        Ok(self.to_interps(std::slice::from_ref(v), span)?.remove(0))
    }

    /// Convert a builtin/op result back (may allocate under the lock).
    fn convert_back(&self, v: &toto_compiler::interp::Value, span: Span) -> Result<RtVal, ExecErr> {
        let mut st = self.shared.state.lock().expect("shared state lock");
        conv::from_value(v, &mut st.heap).map_err(|_| self.internal("function from builtin", span))
    }

    fn loop_frames(&mut self) -> Result<RtVal, ExecErr> {
        loop {
            let top = self.frames.len() - 1;
            let frame = &mut self.frames[top];
            let block_idx = match frame.by_id.get(&frame.block) {
                Some(&i) => i,
                None => {
                    let span = Span::new(0, 0);
                    return Err(self.internal("MIR jumps to a missing block", span));
                }
            };
            let block = &frame.blocks[block_idx];
            if frame.ip < block.instrs.len() {
                let instr = &block.instrs[frame.ip];
                frame.ip += 1;
                // Borrow dance: execute with `frames` reborrowed per op.
                let op = instr.clone();
                self.exec_instr(op)?;
                continue;
            }
            let term = block.term.clone();
            match term {
                MTerm::Jump(id) => {
                    self.frames[top].block = id;
                    self.frames[top].ip = 0;
                }
                MTerm::Branch {
                    cond,
                    then_b,
                    else_b,
                    span,
                    while_cond,
                } => {
                    let c = self.frames[top].locals[cond].clone();
                    match c {
                        RtVal::Bool(true) => {
                            self.frames[top].block = then_b;
                            self.frames[top].ip = 0;
                        }
                        RtVal::Bool(false) => {
                            self.frames[top].block = else_b;
                            self.frames[top].ip = 0;
                        }
                        other => {
                            let heap = self.shared.state.lock().expect("shared state lock");
                            let what = if while_cond { "while" } else { "if" };
                            let tn = other.type_name(&heap.heap);
                            drop(heap);
                            return Err(self.err(
                                "E0207",
                                format!("{what} condition must be bool, got {tn}"),
                                span,
                            ));
                        }
                    }
                }
                MTerm::Return(o) => {
                    let v = o
                        .map(|l| self.frames[top].locals[l].clone())
                        .unwrap_or(RtVal::Unit);
                    if self.finish_frame(v)? {
                        return Ok(self.task_value());
                    }
                }
                MTerm::FallOff { .. } => {
                    let frame = self.frames.last().expect("frame underflow");
                    let name = frame.name.clone();
                    let span = frame.body_span;
                    let is_unit = frame.ret_is_unit();
                    if is_unit {
                        if self.finish_frame(RtVal::Unit)? {
                            return Ok(self.task_value());
                        }
                    } else {
                        self.trace.pop();
                        return Err(self.err(
                            "E0212",
                            format!("function `{name}` exited without returning a value"),
                            span,
                        ));
                    }
                }
            }
        }
    }

    /// Pop the top frame, delivering `v` to the caller. Returns true
    /// when the task itself finished; the task value is then read via
    /// `task_value`.
    fn finish_frame(&mut self, v: RtVal) -> Result<bool, ExecErr> {
        let frame = self.frames.pop().expect("frame underflow");
        self.trace.pop();
        let v = if frame.ret_is_unit() { RtVal::Unit } else { v };
        match (frame.return_dst, self.frames.last_mut()) {
            (Some(dst), Some(caller)) => {
                caller.locals[dst] = v;
                Ok(false)
            }
            // A discarded (`dst == None`) call — an FFI-drained
            // callback — finishing while caller frames remain is not
            // the task finishing: drop the value and continue the
            // caller. Only an empty frame stack ends the task.
            (None, Some(_)) => Ok(false),
            (_, None) => {
                self.task_result = Some(v);
                Ok(true)
            }
        }
    }

    fn task_value(&mut self) -> RtVal {
        self.task_result.take().expect("task value set")
    }

    fn call_value(
        &mut self,
        fv: RtVal,
        args: Vec<RtVal>,
        span: Span,
        display: &str,
        dst: Option<usize>,
    ) -> Result<(), ExecErr> {
        match fv {
            RtVal::Func(idx) => {
                let mfn = self
                    .code
                    .prog
                    .fns
                    .get(idx)
                    .ok_or_else(|| self.internal("call of missing function", span))?;
                let blocks = self
                    .code
                    .fn_blocks
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| self.internal("call of missing function", span))?;
                if args.len() != mfn.params.len() {
                    return Err(self.err(
                        "E0204",
                        format!(
                            "wrong number of arguments: `{}` takes {}, got {}",
                            mfn.name,
                            mfn.params.len(),
                            args.len()
                        ),
                        span,
                    ));
                }
                self.trace.push(TraceFrame {
                    name: mfn.name.clone(),
                    span,
                });
                let name = mfn.name.clone();
                let n_params = mfn.params.len();
                self.push_frame(
                    name,
                    blocks,
                    mfn.locals.len(),
                    args,
                    Vec::new(),
                    n_params,
                    mfn.ret.clone(),
                    mfn.body_span,
                    dst,
                );
                Ok(())
            }
            RtVal::Closure(c) => {
                let body = c.body.clone();
                if args.len() != body.params.len() {
                    return Err(self.err(
                        "E0204",
                        format!(
                            "wrong number of arguments: closure takes {}, got {}",
                            body.params.len(),
                            args.len()
                        ),
                        span,
                    ));
                }
                self.trace.push(TraceFrame {
                    name: display.to_string(),
                    span,
                });
                let n_params = body.params.len();
                self.push_frame(
                    display.to_string(),
                    c.blocks.clone(),
                    body.locals.len(),
                    args,
                    c.captured.clone(),
                    n_params,
                    body.ret.clone(),
                    body.body_span,
                    dst,
                );
                Ok(())
            }
            other => {
                let st = self.shared.state.lock().expect("shared state lock");
                let tn = other.type_name(&st.heap);
                drop(st);
                Err(self.err("E0204", format!("call of non-function value ({tn})"), span))
            }
        }
    }

    /// Publish roots and rendezvous for a stop-the-world collection
    /// when the allocation threshold is reached (allocation + switch
    /// safepoints, frozen ADR-008). With one worker the rendezvous
    /// completes inline, exactly like the old direct collect; with M:N
    /// every worker joins at its next opportunity while this task's
    /// roots stay published.
    /// Profiling sample point (Wave 4 Stage 2): called at every
    /// safepoint via `maybe_collect`. Fast path is two cheap
    /// checks (enabled flag, 1-in-1024 tick) with no locks; only
    /// an actual sample or a requested dump touches shared state.
    fn maybe_sample(&self) {
        let prof = &self.shared.profiler;
        if !prof.enabled() {
            return;
        }
        // Requested dump first: the dumping worker writes the
        // file, everyone else just keeps sampling.
        if prof.take_dump_request() {
            crate::sched::Scheduler::profile_dump(self.shared);
        }
        if !crate::profile::sample_tick() {
            return;
        }
        let quantum = 1_000_000 / prof.config.hz.max(1);
        let now = prof.micros_since_start();
        if now < crate::profile::last_sample().saturating_add(quantum) {
            return;
        }
        crate::profile::stamp_sample(now);
        let Some(worker) = crate::profile::current_worker() else {
            return;
        };
        if worker >= prof.rings.len() {
            return;
        }
        let Some(first) = self.trace.first().map(|f| f.name.clone()) else {
            return;
        };
        let mut stack = String::new();
        for (k, f) in self.trace.iter().enumerate() {
            if k > 0 {
                stack.push(';');
            }
            stack.push_str(&f.name);
        }
        {
            let mut names = prof.names.lock().expect("profile names lock");
            names.entry(self.task.0 as u32).or_insert(first.clone());
        }
        let s = crate::profile::Sample {
            task: self.task.0 as u32,
            micros: now,
            first,
            stack,
        };
        let mut ring = prof.rings[worker].lock().expect("profile ring lock");
        ring.push(s);
    }

    fn maybe_collect(&self) {
        self.maybe_sample();
        self.publish_roots();
        let need = self.shared.force_collect_for_test
            || {
                let st = self.shared.state.lock().expect("shared state lock");
                st.heap.bytes_since_gc() >= self.shared.threshold
            }
            || { self.shared.gc.lock().expect("gc lock").requested };
        if need {
            self.shared.rendezvous_collect();
        }
    }

    /// Allocate on the GC heap behind a safepoint (uniform: every
    /// executor allocation publishes first, then collects on threshold).
    ///
    /// Lock-order contract: `f` runs with the heap-state lock held,
    /// so it must NEVER take the sched lock (nor any lock above
    /// state) — the drain nests sched -> state, and the reverse
    /// deadlocks M:N. Take error slots and other sched-owned values
    /// before calling (see the `*_last_error` arms).
    fn alloc_with<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        self.maybe_collect();
        f(&mut self.shared.state.lock().expect("shared state lock").heap)
    }

    fn expect_chan(&self, v: &RtVal, span: Span) -> Result<ChannelId, ExecErr> {
        match v {
            RtVal::Chan(c) => Ok(*c),
            _ => Err(self.internal("channel operand of non-channel value", span)),
        }
    }

    /// Write a mutated container back to the heap (the `ops` call ran on
    /// a converted clone). Only arrays/structs reach here in checked
    /// programs; anything else is an internal error.
    fn write_back(
        &self,
        base: &RtVal,
        mutated: &toto_compiler::interp::Value,
        span: Span,
    ) -> Result<(), ExecErr> {
        use toto_compiler::interp::Value as V;
        let r = match base {
            RtVal::Heap(r) => *r,
            _ => return Err(self.internal("store into non-heap value", span)),
        };
        // Shape first (short immutable borrow), then convert (needs
        // `&mut`), then install — never holding both borrows at once.
        enum Shape {
            Array,
            Struct { name: String },
            Other,
        }
        let shape = {
            let st = self.shared.state.lock().expect("shared state lock");
            match st.heap.get(r) {
                Some(Payload::Array(_)) => Shape::Array,
                Some(Payload::Struct { name, .. }) => Shape::Struct { name: name.clone() },
                _ => Shape::Other,
            }
        };
        use std::collections::HashMap;
        let mut st = self.shared.state.lock().expect("shared state lock");
        let mut memo = HashMap::new();
        let mut cvt =
            |e: &toto_compiler::interp::Value, heap: &mut Heap| -> Result<RtVal, ExecErr> {
                conv::from_value_memo(e, heap, &mut memo).map_err(|_| {
                    ExecErr::Toto(Diag::new(
                        "E0204",
                        "internal error: function from builtin (unreachable in checked programs)"
                            .to_string(),
                        span,
                        None,
                    ))
                })
            };
        let payload = match (shape, mutated) {
            (Shape::Array, V::Array(cell)) => {
                let borrowed = cell.borrow();
                let mut out = Vec::with_capacity(borrowed.len());
                for e in borrowed.iter() {
                    out.push(cvt(e, &mut st.heap)?);
                }
                Payload::Array(out)
            }
            (Shape::Struct { name }, V::Struct(cell)) => {
                let borrowed = cell.borrow();
                let mut out = Vec::with_capacity(borrowed.fields.len());
                for (n, e) in borrowed.fields.iter() {
                    out.push((n.clone(), cvt(e, &mut st.heap)?));
                }
                Payload::Struct { name, fields: out }
            }
            _ => return Err(self.internal("store shape mismatch", span)),
        };
        match st.heap.get_mut(r) {
            Some(slot) => *slot = payload,
            None => return Err(self.internal("stale heap reference", span)),
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn exec_instr(&mut self, op: MInstr) -> Result<(), ExecErr> {
        use toto_compiler::ast::BinOp;
        let top = self.frames.len() - 1;
        match op {
            MInstr::Const { dst, val } => {
                let v = match val {
                    MConst::Int(n) => RtVal::Int(n),
                    MConst::Float(f) => RtVal::Float(f),
                    MConst::Str(s) => {
                        let r = self.alloc_with(|h| h.alloc_str(&s));
                        RtVal::Heap(r)
                    }
                    MConst::Bool(b) => RtVal::Bool(b),
                    MConst::Unit => RtVal::Unit,
                    MConst::None => RtVal::None,
                };
                self.frames[top].locals[dst] = v;
            }
            MInstr::Copy { dst, src } => {
                let v = self.frames[top].locals[src].clone();
                self.frames[top].locals[dst] = v;
            }
            MInstr::Bin {
                dst,
                op,
                l,
                r,
                span,
            } => {
                let both = [
                    self.frames[top].locals[l].clone(),
                    self.frames[top].locals[r].clone(),
                ];
                // Closure identity is pointer identity in the
                // interpreter (`Rc::ptr_eq`): converting the same
                // closure twice would mint two `Rc`s and break
                // `f == f`. Short-circuit natively on `Arc::ptr_eq`.
                let closed_over_identity = match (&both[0], &both[1], op) {
                    (RtVal::Closure(a), RtVal::Closure(b), MBinOp::Eq) => {
                        Some(std::sync::Arc::ptr_eq(a, b))
                    }
                    (RtVal::Closure(a), RtVal::Closure(b), MBinOp::NotEq) => {
                        Some(!std::sync::Arc::ptr_eq(a, b))
                    }
                    _ => None,
                };
                let v = match closed_over_identity {
                    Some(eq) => toto_compiler::interp::Value::Bool(eq),
                    None => {
                        let vs = self.to_interps(&both, span)?;
                        let lv = vs[0].clone();
                        let rv = vs[1].clone();
                        match op {
                            MBinOp::Add => {
                                ops::arith(lv, rv, span, "add", |a, b| a.checked_add(b))?
                            }
                            MBinOp::Sub => {
                                ops::arith(lv, rv, span, "subtract", |a, b| a.checked_sub(b))?
                            }
                            MBinOp::Mul => {
                                ops::arith(lv, rv, span, "multiply", |a, b| a.checked_mul(b))?
                            }
                            MBinOp::Div => ops::div_values(lv, rv, span)?,
                            MBinOp::Mod => ops::mod_values(lv, rv, span)?,
                            MBinOp::Eq => ops::equality(lv, rv, span, true)?,
                            MBinOp::NotEq => ops::equality(lv, rv, span, false)?,
                            MBinOp::Lt | MBinOp::LtEq | MBinOp::Gt | MBinOp::GtEq => {
                                let aop = match op {
                                    MBinOp::Lt => BinOp::Lt,
                                    MBinOp::LtEq => BinOp::LtEq,
                                    MBinOp::Gt => BinOp::Gt,
                                    _ => BinOp::GtEq,
                                };
                                ops::ordering(aop, lv, rv, span)?
                            }
                        }
                    }
                };
                let r = self.convert_back(&v, span)?;
                self.frames[top].locals[dst] = r;
            }
            MInstr::Neg { dst, operand, span } => {
                let v = self.to_interp(&self.frames[top].locals[operand].clone())?;
                let out = ops::neg_value(v, span, span)?;
                let r = self.convert_back(&out, span)?;
                self.frames[top].locals[dst] = r;
            }
            MInstr::Not { dst, operand, span } => {
                let v = self.to_interp(&self.frames[top].locals[operand].clone())?;
                let out = ops::not_value(v, span)?;
                let r = self.convert_back(&out, span)?;
                self.frames[top].locals[dst] = r;
            }
            MInstr::DivCheck { value, span } => match &self.frames[top].locals[value] {
                RtVal::Int(0) => {
                    return Err(self.err("E0202", "division by zero".to_string(), span));
                }
                RtVal::Int(_) => {}
                other => {
                    let heap = self.shared.state.lock().expect("shared state lock");
                    let tn = other.type_name(&heap.heap);
                    drop(heap);
                    debug_assert!(false, "divcheck on non-int: {tn}");
                }
            },
            MInstr::Call {
                dst,
                func,
                args,
                span,
                display,
            } => {
                // Calls push a frame and continue in it; the callee's
                // `Return` delivers to `dst` via `finish_frame`.
                let fv = self.frames[top].locals[func].clone();
                let mut vs = Vec::with_capacity(args.len());
                for a in &args {
                    vs.push(self.frames[top].locals[*a].clone());
                }
                self.call_value(fv, vs, span, &display, dst)?;
            }
            MInstr::CallBuiltin {
                dst,
                name,
                args,
                span,
            } => {
                self.exec_builtin(dst, &name, &args, span)?;
            }
            MInstr::CallForeign {
                dst,
                ref name,
                ref args,
                span,
            } => {
                self.exec_foreign(dst, name, args, span)?;
            }
            MInstr::FuncRef { dst, idx } => {
                self.frames[top].locals[dst] = RtVal::Func(idx);
            }
            MInstr::StructLit {
                dst, name, fields, ..
            } => {
                let mut vs = Vec::with_capacity(fields.len());
                for (n, l) in &fields {
                    vs.push((n.clone(), self.frames[top].locals[*l].clone()));
                }
                let r = self.alloc_with(|h| h.alloc_struct(&name, vs));
                self.frames[top].locals[dst] = RtVal::Heap(r);
            }
            MInstr::ArrayLit { dst, elems, .. } => {
                let mut vs = Vec::with_capacity(elems.len());
                for e in &elems {
                    vs.push(self.frames[top].locals[*e].clone());
                }
                let r = self.alloc_with(|h| h.alloc_array(vs));
                self.frames[top].locals[dst] = RtVal::Heap(r);
            }
            MInstr::Snapshot { dst, base, .. } => {
                let vs = {
                    let st = self.shared.state.lock().expect("shared state lock");
                    match &self.frames[top].locals[base] {
                        RtVal::Heap(r) => match st.heap.get(*r) {
                            Some(Payload::Array(elems)) => elems.clone(),
                            _ => {
                                debug_assert!(false, "snapshot of non-array");
                                Vec::new()
                            }
                        },
                        _ => {
                            debug_assert!(false, "snapshot of non-heap");
                            Vec::new()
                        }
                    }
                };
                let r = self.alloc_with(|h| h.alloc_array(vs));
                self.frames[top].locals[dst] = RtVal::Heap(r);
            }
            MInstr::RangeLit { dst, lo, hi, .. } => {
                let (l, h) = match (&self.frames[top].locals[lo], &self.frames[top].locals[hi]) {
                    (RtVal::Int(a), RtVal::Int(b)) => (*a, *b),
                    _ => {
                        debug_assert!(false, "range of non-ints");
                        (0, 0)
                    }
                };
                self.frames[top].locals[dst] = RtVal::Range(l, h);
            }
            MInstr::SplitRange { lo, hi, range, .. } => {
                let (a, b) = match &self.frames[top].locals[range] {
                    RtVal::Range(a, b) => (*a, *b),
                    _ => {
                        debug_assert!(false, "split of non-range");
                        (0, 0)
                    }
                };
                self.frames[top].locals[lo] = RtVal::Int(a);
                self.frames[top].locals[hi] = RtVal::Int(b);
            }
            MInstr::WrapOk { dst, inner } => {
                let v = self.frames[top].locals[inner].clone();
                self.frames[top].locals[dst] = RtVal::Ok(Box::new(v));
            }
            MInstr::WrapErr { dst, inner } => {
                let v = self.frames[top].locals[inner].clone();
                self.frames[top].locals[dst] = RtVal::Err(Box::new(v));
            }
            MInstr::WrapSome { dst, inner } => {
                let v = self.frames[top].locals[inner].clone();
                self.frames[top].locals[dst] = RtVal::Some(Box::new(v));
            }
            MInstr::Propagate { dst, operand, span } => {
                let ret_kind = self.frames[top].ret_kind();
                let owned = [self.frames[top].locals[operand].clone()];
                let vs = self.to_interps(&owned, span)?;
                let v = vs[0].clone();
                match ops::propagate(v, ret_kind, span) {
                    Ok(unwrapped) => {
                        let r = self.convert_back(&unwrapped, span)?;
                        self.frames[top].locals[dst] = r;
                    }
                    Err(ops::Propagate::Return(e)) => {
                        let r = self.convert_back(&e, span)?;
                        if self.finish_frame(r)? {
                            // Propagated out of the task top frame: the
                            // task value is set; the loop observes it.
                            return Ok(());
                        }
                    }
                    Err(ops::Propagate::Bug(bspan)) => {
                        return Err(self.err(
                            "E0204",
                            "`?` outside a matching Result/Option function (unreachable in checked programs)".to_string(),
                            bspan,
                        ));
                    }
                }
            }
            // Field/index reads ALIAS (share the heap object), exactly
            // like the tree-walking interpreter: the result is the same
            // `GcRef`, not a copy. Only the error paths convert (so
            // messages stay identical by construction).
            MInstr::Field {
                dst,
                base,
                name,
                span,
            } => {
                let aliased = {
                    let st = self.shared.state.lock().expect("shared state lock");
                    match &self.frames[top].locals[base] {
                        RtVal::Heap(r) => match st.heap.get(*r) {
                            Some(Payload::Struct { fields, .. }) => fields
                                .iter()
                                .find(|(n, _)| n == &name)
                                .map(|(_, v)| v.clone()),
                            _ => None,
                        },
                        _ => None,
                    }
                };
                match aliased {
                    Some(v) => self.frames[top].locals[dst] = v,
                    None => {
                        let b = self.to_interp(&self.frames[top].locals[base].clone())?;
                        let out = ops::field_read(b, &name, span)?;
                        let r = self.convert_back(&out, span)?;
                        self.frames[top].locals[dst] = r;
                    }
                }
            }
            MInstr::Index {
                dst,
                base,
                index,
                span,
                index_span,
            } => {
                let aliased = {
                    let st = self.shared.state.lock().expect("shared state lock");
                    match (
                        &self.frames[top].locals[base],
                        &self.frames[top].locals[index],
                    ) {
                        (RtVal::Heap(r), RtVal::Int(i)) => match st.heap.get(*r) {
                            Some(Payload::Array(elems)) => (*i)
                                .try_into()
                                .ok()
                                .and_then(|k: usize| elems.get(k).cloned()),
                            _ => None,
                        },
                        // No `Range` fast path: indexing a non-array is
                        // `E0209` (spec §5.8), raised via the fallback
                        // below like every other executor. The checker
                        // rejects `Range` bases statically (`E0310`),
                        // so this arm is unreachable in checked code.
                        _ => None,
                    }
                };
                match aliased {
                    Some(v) => self.frames[top].locals[dst] = v,
                    None => {
                        let pair = [
                            self.frames[top].locals[base].clone(),
                            self.frames[top].locals[index].clone(),
                        ];
                        let vs = self.to_interps(&pair, span)?;
                        let out = ops::index_read(vs[0].clone(), vs[1].clone(), span, index_span)?;
                        let r = self.convert_back(&out, span)?;
                        self.frames[top].locals[dst] = r;
                    }
                }
            }
            MInstr::StoreField {
                base,
                name,
                value,
                span,
            } => {
                let b = self.frames[top].locals[base].clone();
                let triple = [b.clone(), self.frames[top].locals[value].clone()];
                let vs = self.to_interps(&triple, span)?;
                let bv = vs[0].clone();
                let vv = vs[1].clone();
                // `field_write` mutates through the shared `Rc`: the
                // converted clone carries the mutation for write-back.
                ops::field_write(bv.clone(), &name, vv, span)?;
                self.write_back(&b, &bv, span)?;
            }
            MInstr::StoreIndex {
                base,
                index,
                value,
                span,
                index_span,
            } => {
                let b = self.frames[top].locals[base].clone();
                let triple = [
                    b.clone(),
                    self.frames[top].locals[index].clone(),
                    self.frames[top].locals[value].clone(),
                ];
                let vs = self.to_interps(&triple, span)?;
                let bv = vs[0].clone();
                let iv = vs[1].clone();
                let vv = vs[2].clone();
                ops::index_write(bv.clone(), iv, vv, span, index_span)?;
                self.write_back(&b, &bv, span)?;
            }
            MInstr::Len { dst, base, span } => {
                let st = self.shared.state.lock().expect("shared state lock");
                let v = match &self.frames[top].locals[base] {
                    RtVal::Heap(r) => match st.heap.get(*r) {
                        Some(Payload::Array(elems)) => RtVal::Int(elems.len() as i64),
                        _ => {
                            let tn = self.frames[top].locals[base].type_name(&st.heap);
                            drop(st);
                            return Err(self.err(
                                "E0209",
                                format!("`len` needs an array, got {tn}"),
                                span,
                            ));
                        }
                    },
                    other => {
                        let tn = other.type_name(&st.heap);
                        drop(st);
                        return Err(self.err(
                            "E0209",
                            format!("`len` needs an array, got {tn}"),
                            span,
                        ));
                    }
                };
                drop(st);
                self.frames[top].locals[dst] = v;
            }
            MInstr::SatIncr { dst, operand } => match &self.frames[top].locals[operand] {
                RtVal::Int(i) => {
                    self.frames[top].locals[dst] = RtVal::Int(i.saturating_add(1));
                }
                other => {
                    let st = self.shared.state.lock().expect("shared state lock");
                    let tn = other.type_name(&st.heap);
                    drop(st);
                    debug_assert!(false, "satincr of non-int: {tn}");
                }
            },
            MInstr::MakeClosure {
                dst,
                body,
                captures,
                ..
            } => {
                let mut env = Vec::with_capacity(captures.len());
                for src in captures {
                    env.push(self.frames[top].locals[src].clone());
                }
                self.frames[top].locals[dst] = RtVal::Closure(Arc::new(RtClosure {
                    body: body.clone(),
                    blocks: Arc::from(body.blocks.as_slice()),
                    captured: env,
                }));
            }
            MInstr::Safepoint { .. } => {
                self.maybe_collect();
            }
            MInstr::StateGet { dst, slot, span } => {
                // Declared-state read (spec amendment, Phase 6): the
                // cell vector lives in shared state (one per run, all
                // tasks see the same cells). Initialization is
                // ordinary MIR in `main`, so an unset slot is a
                // compiler bug, never a user error.
                let v = match self
                    .shared
                    .state
                    .lock()
                    .expect("shared state lock")
                    .state_vars
                    .get(slot as usize)
                    .cloned()
                {
                    Some(v) => v,
                    None => {
                        return Err(self.internal("state slot never initialized", span));
                    }
                };
                self.frames[top].locals[dst] = v;
            }
            MInstr::StateSet { slot, src, span } => {
                let _ = span;
                let v = self.frames[top].locals[src].clone();
                let mut st = self.shared.state.lock().expect("shared state lock");
                if st.state_vars.len() <= slot as usize {
                    st.state_vars.resize(slot as usize + 1, RtVal::Unit);
                }
                st.state_vars[slot as usize] = v;
            }
            MInstr::ChanNew {
                dst,
                elem,
                cap,
                span,
            } => {
                let cap_n = match cap {
                    Some(l) => match &self.frames[top].locals[l] {
                        RtVal::Int(n) => Some(*n),
                        _ => return Err(self.internal("non-int channel capacity", span)),
                    },
                    None => None,
                };
                if cap_n.is_some_and(|n| n < 0) {
                    return Err(self.err(
                        "E0204",
                        format!("negative channel capacity {}", cap_n.unwrap_or(0)),
                        span,
                    ));
                }
                match self.yield_now(ToSched::ChanNew {
                    cap: cap_n,
                    elem,
                    span,
                }) {
                    ToTask::Value(v) => self.frames[top].locals[dst] = v,
                    ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
                    ToTask::Cancelled { op } => return Err(self.cancelled(op, span)),
                    other => {
                        let _ = other;
                        return Err(self.internal("channew response", span));
                    }
                }
            }
            MInstr::Send { chan, value, span } => {
                let c = self.expect_chan(&self.frames[top].locals[chan].clone(), span)?;
                let v = self.frames[top].locals[value].clone();
                match self.park(
                    ToSched::Send {
                        chan: c,
                        value: v,
                        span,
                    },
                    span,
                    "send",
                )? {
                    ToTask::Unit => {}
                    ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
                    other => {
                        let _ = other;
                        return Err(self.internal("send response", span));
                    }
                }
            }
            MInstr::Recv { dst, chan, span } => {
                let c = self.expect_chan(&self.frames[top].locals[chan].clone(), span)?;
                match self.park(ToSched::Recv { chan: c, span }, span, "recv")? {
                    ToTask::Value(v) => self.frames[top].locals[dst] = v,
                    ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
                    other => {
                        let _ = other;
                        return Err(self.internal("recv response", span));
                    }
                }
            }
            MInstr::Spawn {
                body,
                captures,
                span,
            } => {
                let mut vs = Vec::with_capacity(captures.len());
                for src in captures {
                    vs.push(self.frames[top].locals[src].clone());
                }
                match self.yield_now(ToSched::Spawn {
                    body: body.clone(),
                    blocks: Arc::from(body.blocks.as_slice()),
                    captures: vs,
                    span,
                    scope: self.scope,
                }) {
                    ToTask::Unit => {}
                    ToTask::Cancelled { op } => return Err(self.cancelled(op, span)),
                    ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
                    other => {
                        let _ = other;
                        return Err(self.internal("spawn response", span));
                    }
                }
            }
            MInstr::Select {
                dst,
                cases,
                has_default,
                span,
            } => {
                self.exec_select(dst, cases, has_default, span)?;
            }
        }
        Ok(())
    }

    fn exec_select(
        &mut self,
        dst: usize,
        cases: Vec<MSelectCase>,
        has_default: bool,
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::SelReq;
        let top = self.frames.len() - 1;
        let mut reqs = Vec::with_capacity(cases.len());
        for c in &cases {
            match c {
                MSelectCase::Recv { chan, span: cs, .. } => {
                    let ch = self.expect_chan(&self.frames[top].locals[*chan].clone(), *cs)?;
                    reqs.push(SelReq::Recv {
                        chan: ch,
                        span: *cs,
                    });
                }
                MSelectCase::Send {
                    chan,
                    value,
                    span: cs,
                } => {
                    let ch = self.expect_chan(&self.frames[top].locals[*chan].clone(), *cs)?;
                    let v = self.frames[top].locals[*value].clone();
                    reqs.push(SelReq::Send {
                        chan: ch,
                        value: v,
                        span: *cs,
                    });
                }
            }
        }
        match self.park(
            ToSched::Select {
                cases: reqs,
                has_default,
                span,
            },
            span,
            "select",
        )? {
            ToTask::Selected { index, value } => {
                if index < cases.len()
                    && let (Some(v), MSelectCase::Recv { bind: Some(b), .. }) =
                        (value, &cases[index])
                {
                    self.frames[top].locals[*b] = v;
                }
                self.frames[top].locals[dst] = RtVal::Int(index as i64);
            }
            ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
            other => {
                let _ = other;
                return Err(self.internal("select response", span));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn exec_builtin(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        let top = self.frames.len() - 1;
        // Channel builtins yield to the scheduler (channel state lives
        // there); everything else runs inline on converted values.
        match name {
            "close" => {
                let c = match args {
                    [l] => self.expect_chan(&self.frames[top].locals[*l].clone(), span)?,
                    _ => return Err(self.internal("close arity", span)),
                };
                match self.yield_now(ToSched::Close { chan: c, span }) {
                    ToTask::Unit => {
                        if let Some(d) = dst {
                            self.frames[top].locals[d] = RtVal::Unit;
                        }
                    }
                    ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
                    ToTask::Cancelled { op } => return Err(self.cancelled(op, span)),
                    other => {
                        let _ = other;
                        return Err(self.internal("close response", span));
                    }
                }
                return Ok(());
            }
            "closed" => {
                let c = match args {
                    [l] => self.expect_chan(&self.frames[top].locals[*l].clone(), span)?,
                    _ => return Err(self.internal("closed arity", span)),
                };
                match self.yield_now(ToSched::Closed { chan: c, span }) {
                    ToTask::Value(v) => {
                        if let Some(d) = dst {
                            self.frames[top].locals[d] = v;
                        }
                    }
                    ToTask::OpPanic { diag } => return Err(ExecErr::Toto(diag)),
                    ToTask::Cancelled { op } => return Err(self.cancelled(op, span)),
                    other => {
                        let _ = other;
                        return Err(self.internal("closed response", span));
                    }
                }
                return Ok(());
            }
            "chan" => {
                // Unreachable: the parser only ever emits `ChanNew`.
                return Err(self.internal("bare `chan` builtin call", span));
            }
            "len" => return self.builtin_len(dst, args, span),
            "push" => return self.builtin_push(dst, args, span),
            "pop" => return self.builtin_pop(dst, args, span),
            "read_file" | "write_file" | "file_exists" => {
                return self.builtin_blocking(dst, name, args, span);
            }
            _ => {}
        }
        // Shared builtins via conversion (parity by construction; one
        // memo so shared arguments stay shared). `owned` is scoped so
        // its slot is dead during the builtin call below.
        let vs = {
            let owned: Vec<RtVal> = args
                .iter()
                .map(|a| self.frames[top].locals[*a].clone())
                .collect();
            self.to_interps(&owned, span)?
        };
        self.trace.push(TraceFrame {
            name: format!("{name} (builtin)"),
            span,
        });
        // `print`/`println` bypass `call_builtin`'s 40-arm match
        // (~24 KiB of debug frame): same canonical implementation via
        // `call_print`, same trace frame and conversions, a fraction
        // of the stack. The 64 KiB task budget leaves no room for the
        // full match on the hottest builtin.
        let is_print = name == "print" || name == "println";
        let out = {
            let mut output = self.shared.output.lock().expect("output lock");
            if is_print {
                builtins::call_print(vs, name == "println", span, &self.code.fns, &mut output)
            } else {
                builtins::call_builtin(name, vs, span, &self.code.fns, &mut output)
            }
        };
        match out {
            Ok(v) => {
                self.trace.pop();
                let r = self.convert_back(&v, span)?;
                if let Some(d) = dst {
                    self.frames[top].locals[d] = r;
                }
            }
            // Like the MIR interpreter: the builtin frame stays in the
            // trace on failure.
            Err((diag, _carried)) => return Err(ExecErr::Toto(diag)),
        }
        Ok(())
    }

    /// Read a string argument straight off the heap (no interpreter
    /// conversion: blocking requests carry owned plain data so no
    /// `Rc`-bearing `Value` is alive at the park — see the `Send`
    /// audit in `sched.rs`).
    fn blocking_str(&self, local: usize, name: &str, span: Span) -> Result<String, ExecErr> {
        let top = self.frames.len() - 1;
        let st = self.shared.state.lock().expect("shared state lock");
        match &self.frames[top].locals[local] {
            RtVal::Heap(r) => match st.heap.get(*r) {
                Some(Payload::Str(s)) => Ok(s.clone()),
                _ => {
                    let tn = self.frames[top].locals[local].type_name(&st.heap);
                    Err(self.internal(
                        &format!("`{name}` argument must be a string, got {tn}"),
                        span,
                    ))
                }
            },
            other => {
                let tn = other.type_name(&st.heap);
                Err(self.internal(
                    &format!("`{name}` argument must be a string, got {tn}"),
                    span,
                ))
            }
        }
    }

    /// File-system builtins via the dedicated blocking pool (Stage 2
    /// policy): arguments convert to owned plain data, the job runs on
    /// a pool thread, and the task parks until the completion arrives.
    /// Results mirror the inline builtins exactly (same values, same
    /// E0210 messages — the pool formats those).
    fn builtin_blocking(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::BlockingOp;
        let op = match name {
            "read_file" => {
                self.arity(name, args.len(), 1, span)?;
                BlockingOp::ReadFile {
                    path: self.blocking_str(args[0], name, span)?,
                }
            }
            "write_file" => {
                self.arity(name, args.len(), 2, span)?;
                BlockingOp::WriteFile {
                    path: self.blocking_str(args[0], name, span)?,
                    contents: self.blocking_str(args[1], name, span)?.into_bytes(),
                }
            }
            "file_exists" => {
                self.arity(name, args.len(), 1, span)?;
                BlockingOp::FileExists {
                    path: self.blocking_str(args[0], name, span)?,
                }
            }
            _ => return Err(self.internal("non-blocking builtin", span)),
        };
        // Fail-fast in a failed scope, like `park`.
        if self.scope_failed() {
            return Err(self.cancelled("blocking", span));
        }
        self.check_deadline(span, "blocking")?;
        self.maybe_collect();
        // Push the builtin frame BEFORE parking so a pool-reported
        // failure carries the same trace as the inline builtins (the
        // frame stays on panic, like `exec_builtin`). The frame holds
        // no heap references, so parking with it pushed is root-safe.
        self.trace.push(TraceFrame {
            name: format!("{name} (builtin)"),
            span,
        });
        match self.y.suspend(crate::sched::ToSched::Blocking { op, span }) {
            ToTask::Value(v) => {
                self.trace.pop();
                if let Some(d) = dst {
                    let top = self.frames.len() - 1;
                    self.frames[top].locals[d] = v;
                }
                Ok(())
            }
            ToTask::OpPanic { diag } => Err(ExecErr::Toto(diag)),
            ToTask::Cancelled { op } => {
                self.trace.pop();
                Err(self.cancelled(op, span))
            }
            other => {
                let _ = other;
                Err(self.internal("blocking response", span))
            }
        }
    }

    fn arity(&self, name: &str, got: usize, want: usize, span: Span) -> Result<(), ExecErr> {
        if got == want {
            Ok(())
        } else {
            Err(self.err(
                "E0204",
                format!("wrong number of arguments: `{name}` takes {want}, got {got}"),
                span,
            ))
        }
    }

    /// Foreign calls via the dedicated blocking pool (Phase 5, Stage 3;
    /// same policy as file builtins): arguments convert to owned plain
    /// data, the pool runs the call, and callbacks drain on this worker
    /// after the completion. Callback slots are claimed here (mapped to
    /// their Toto function) and released at drain, on call failure, or
    /// at task teardown — the pool never releases (single owner).
    fn exec_foreign(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        // Network foreign calls bypass the generic pool path: they
        // need the M:N poller (never the pool, never inline blocking).
        // Unix-only surface; checked code on other platforms cannot
        // name these (E0301), so the cfg'd-out fallthrough below is
        // unreachable there.
        #[cfg(unix)]
        if Self::NET_FNS.contains(&name) {
            return self.exec_net(dst, name, args, span);
        }
        // Crypto foreign calls run inline on the worker: they are
        // one-shot OpenSSL calls (microseconds, never blocking, never
        // parking) — the poller and the pool are both uninvolved.
        // Unix-only surface; checked code on other platforms cannot
        // name these (E0301), so the cfg'd-out fallthrough below is
        // unreachable there.
        #[cfg(unix)]
        if Self::CRYPTO_FNS.contains(&name) {
            return self.exec_crypto(dst, name, args, span);
        }
        // TLS foreign calls run on the blocking pool: handshakes and
        // record I/O block on the network, so they must never run
        // inline on a worker (unlike one-shot crypto) and never on
        // the poller (an SSL step is not a readiness wait).
        // Unix-only surface; checked code on other platforms cannot
        // name these (E0301).
        #[cfg(unix)]
        if Self::TLS_FNS.contains(&name) {
            return self.exec_tls(dst, name, args, span);
        }
        // Gzip foreign calls run inline on the worker: one-shot
        // pure-Rust compression, never blocking, never parking.
        // Unix-gated like the other Wave 1 surfaces (uniformity;
        // the backend itself is portable, so lifting is cfg-only).
        #[cfg(unix)]
        if Self::GZIP_FNS.contains(&name) {
            return self.exec_gzip(dst, name, args, span);
        }
        // Wave 2 Stage 3 context + clock + logging calls run inline
        // on the worker: slot reads/writes, the process clock, and
        // line formatting never block and never park. Unix-gated
        // like the other Wave 2 surfaces (uniformity; the backends
        // are portable Rust, so lifting is cfg-only).
        #[cfg(unix)]
        if Self::CTX_FNS.contains(&name) {
            return self.exec_ctx(dst, name, args, span);
        }
        #[cfg(unix)]
        if Self::CLOCK_FNS.contains(&name) {
            return self.exec_clock(dst, name, args, span);
        }
        #[cfg(unix)]
        if Self::LOG_FNS.contains(&name) {
            return self.exec_log(dst, name, args, span);
        }
        // Wave 3 Stage 1 database calls run on the blocking pool: a
        // checkout or a query blocks, so they must never run inline
        // on a worker (unlike one-shot crypto/gzip) and never on
        // the poller (a pool wait is not a readiness wait).
        // Unix-only surface; checked code on other platforms cannot
        // name these (E0301).
        #[cfg(unix)]
        if Self::DB_FNS.contains(&name) {
            return self.exec_db(dst, name, args, span);
        }
        // Wave 3 Stage 3 directory calls run on the blocking pool:
        // a connect/bind/search blocks, so they must never run
        // inline on a worker (unlike one-shot crypto/gzip) and
        // never on the poller (a pool wait is not a readiness
        // wait). Linux-only surface; checked code on other
        // platforms cannot name these (E0301).
        #[cfg(target_os = "linux")]
        if Self::LDAP_FNS.contains(&name) {
            return self.exec_ldap(dst, name, args, span);
        }
        // Wave 3 Stage 4 environment lookups run inline on the
        // worker: `getenv` is a pure process-table read (never
        // blocks, never parks) — same discipline as one-shot
        // crypto/gzip. Unix-only surface.
        #[cfg(unix)]
        if Self::ENV_FNS.contains(&name) {
            return self.exec_env(dst, name, args, span);
        }
        // Generic pool tail lives in its own frame (see below): this
        // dispatcher stays thin so every inline foreign call on the
        // serve path (net/crypto/gzip/tls/ctx/clock/log) pays only the
        // dispatch checks. Coroutine stacks are spec-fixed 64 KiB
        // (ADR-002/ADR-008) — the Wave 1 ARM guard-fault precedent:
        // the Stage 3 dispatch growth pushed serve high-water from
        // ~60.5 KiB to ~61.2 KiB on x86_64, overflowing the guard
        // under ARM64's larger frames (macOS SIGBUS). Shrink the
        // frames, never the budget.
        self.exec_foreign_pool(dst, name, args, span)
    }

    /// Generic blocking-pool foreign tail of `exec_foreign`
    /// (`#[inline(never)]` keeps these temporaries out of the hot
    /// dispatcher frame — see above). Arguments convert to owned
    /// plain data, the pool runs the call, and callbacks drain on
    /// this worker after the completion. Behavior is exactly the
    /// former inline tail; only the frame moved.
    #[inline(never)]
    fn exec_foreign_pool(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::{BlockingOp, ForeignArg};
        use toto_compiler::ffi;
        let Some(sig) = self.code.externs.get(name).cloned() else {
            return Err(self.internal("unknown foreign function", span));
        };
        let top = self.frames.len() - 1;
        let mut fargs = Vec::with_capacity(args.len());
        // `(argument index, slot)`: the pool pairs by argument index,
        // so positional pairing would misroute (e.g. `pthread_create`
        // takes its callback at index 2, not 0).
        let mut slots: Vec<(usize, u64)> = Vec::new();
        for (i, (local, (_, pty))) in args.iter().zip(sig.params.iter()).enumerate() {
            let v = self.frames[top].locals[*local].clone();
            if matches!(pty, toto_compiler::hir::Ty::Fn(..)) {
                match v {
                    RtVal::Func(_) | RtVal::Closure(_) => match ffi::claim_slot() {
                        Ok(s) => {
                            self.callbacks.insert(s, v);
                            slots.push((i, s));
                            fargs.push(ForeignArg::FnPtr(s));
                        }
                        Err(ffi::PoolExhausted) => {
                            for (_, s) in &slots {
                                self.callbacks.remove(s);
                                ffi::release_slot(*s);
                            }
                            return Err(self.err(
                                "E0217",
                                "all 8 callback slots are live; drain running callbacks (join threads) before registering more".to_string(),
                                span,
                            ));
                        }
                    },
                    other => {
                        let st = self.shared.state.lock().expect("shared state lock");
                        let tn = other.type_name(&st.heap);
                        return Err(self.internal(
                            &format!("foreign callback must be a function, got {tn}"),
                            span,
                        ));
                    }
                }
            } else {
                fargs.push(self.foreign_arg(&v, name, span)?);
            }
        }
        if self.scope_failed() {
            for (_, s) in &slots {
                self.callbacks.remove(s);
                ffi::release_slot(*s);
            }
            return Err(self.cancelled("blocking", span));
        }
        if let Err(e) = self.check_deadline(span, "blocking") {
            for (_, s) in &slots {
                self.callbacks.remove(s);
                ffi::release_slot(*s);
            }
            return Err(e);
        }
        self.maybe_collect();
        // Push the foreign frame BEFORE parking so a pool-reported
        // failure carries the same trace as inline calls (the frame
        // stays on panic). The frame holds no heap references, so
        // parking with it pushed is root-safe.
        self.trace.push(TraceFrame {
            name: format!("{name} (foreign)"),
            span,
        });
        // Cleanup copy: `slots` moves into the job, but failure
        // paths below need the ids to release the pool slots.
        let cleanup: Vec<u64> = slots.iter().map(|(_, s)| *s).collect();
        let op = BlockingOp::Foreign {
            name: name.to_string(),
            sig,
            args: fargs,
            slots,
        };
        match self.y.suspend(crate::sched::ToSched::Blocking { op, span }) {
            ToTask::Value(v) => {
                self.trace.pop();
                if let Some(d) = dst {
                    let top = self.frames.len() - 1;
                    self.frames[top].locals[d] = v;
                }
                self.drain_callbacks(span)?;
                Ok(())
            }
            ToTask::OpPanic { diag } => {
                for s in &cleanup {
                    self.callbacks.remove(s);
                    ffi::release_slot(*s);
                }
                Err(ExecErr::Toto(diag))
            }
            ToTask::Cancelled { op } => {
                for s in &cleanup {
                    self.callbacks.remove(s);
                    ffi::release_slot(*s);
                }
                self.trace.pop();
                Err(self.cancelled(op, span))
            }
            other => {
                let _ = other;
                Err(self.internal("blocking response", span))
            }
        }
    }

    /// Network foreign calls (Wave 1, unix-only): native executor
    /// handling with M:N poller integration. Routed here — never
    /// through the generic pool `Foreign` path (a pool thread must
    /// never block on a network wait) — by name from `exec_foreign`.
    ///
    /// Shape per call: fast nonblocking syscall inline on the worker;
    /// `WouldBlock` parks on the poller (`ToSched::Net`) exactly once
    /// and the resume value is the finished result (the poller ran the
    /// syscall). DNS is the exception: it rides
    /// the blocking pool (`BlockingOp::DnsResolve`), the only
    /// genuinely blocking network call. The per-task `net_error`
    /// slot carries failures (set on every completion, read and
    /// cleared by `toto_rt_net_last_error`).
    #[cfg(unix)]
    const NET_FNS: &[&str] = &[
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
    ];

    /// Park the current task on a readiness wait. The resume value
    /// IS the finished result: the poller executed the nonblocking
    /// syscall and the drain converted it (setting the error slot).
    /// Map it with `net_scalar_resume` / `net_array_resume`.
    /// Cancellations and unexpected responses propagate as errors.
    #[cfg(unix)]
    fn net_wait(&mut self, op: crate::sched::NetOp, span: Span) -> Result<RtVal, ExecErr> {
        if self.scope_failed() {
            return Err(self.cancelled("net", span));
        }
        self.check_deadline(span, "net")?;
        self.maybe_collect();
        self.trace.push(TraceFrame {
            name: "net wait".to_string(),
            span,
        });
        let out = match self.y.suspend(crate::sched::ToSched::Net { op, span }) {
            ToTask::Value(v) => Ok(v),
            ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
            other => {
                let _ = other;
                Err(self.internal("net response", span))
            }
        };
        self.trace.pop();
        out
    }

    /// Assign `dst` (if any). No trace handling: callers manage their
    /// own frames around parks (see `net_wait`).
    #[cfg(unix)]
    fn net_assign(&mut self, dst: Option<usize>, v: RtVal) {
        if let Some(d) = dst {
            let top = self.frames.len() - 1;
            self.frames[top].locals[d] = v;
        }
    }

    /// Read an int arg (checked code guarantees the static type; a
    /// mismatch is a bug, not a user error).
    #[cfg(unix)]
    fn net_int(&self, args: &[usize], i: usize, name: &str, span: Span) -> Result<i64, ExecErr> {
        let top = self.frames.len() - 1;
        match self.frames[top].locals[args[i]] {
            RtVal::Int(n) => Ok(n),
            _ => Err(self.internal(&format!("`{name}` argument must be an int"), span)),
        }
    }

    /// Read a validated port (1-65535; the Toto wrapper pre-checks,
    /// this is the backstop for hand-built MIR).
    #[cfg(unix)]
    fn net_port(&self, args: &[usize], i: usize, name: &str, span: Span) -> Result<u16, ExecErr> {
        let n = self.net_int(args, i, name, span)?;
        if !(1..=65535).contains(&n) {
            return Err(self.err("E0308", format!("invalid port {n} (want 1-65535)"), span));
        }
        Ok(n as u16)
    }

    /// Read an fd arg (checked code guarantees `int`; negative fds
    /// fail loudly in the syscall, never silently).
    #[cfg(unix)]
    fn net_fd(&self, args: &[usize], i: usize, name: &str, span: Span) -> Result<i32, ExecErr> {
        Ok(self.net_int(args, i, name, span)? as i32)
    }

    /// Read a byte count (must be >= 1; the wrapper pre-checks, this
    /// is the backstop for hand-built MIR).
    #[cfg(unix)]
    fn net_count(
        &self,
        args: &[usize],
        i: usize,
        name: &str,
        span: Span,
    ) -> Result<usize, ExecErr> {
        let n = self.net_int(args, i, name, span)?;
        if n < 1 {
            return Err(self.err("E0308", format!("invalid max {n} (want >= 1)"), span));
        }
        Ok(n as usize)
    }

    /// Read a string arg (checked code guarantees the static type).
    #[cfg(unix)]
    fn net_str(&self, args: &[usize], i: usize, name: &str, span: Span) -> Result<String, ExecErr> {
        let top = self.frames.len() - 1;
        match &self.frames[top].locals[args[i]] {
            RtVal::Heap(r) => {
                let st = self.shared.state.lock().expect("shared state lock");
                match st.heap.get(*r) {
                    Some(crate::heap::Payload::Str(s)) => Ok(s.clone()),
                    _ => Err(self.internal(&format!("`{name}` argument must be a string"), span)),
                }
            }
            _ => Err(self.internal(&format!("`{name}` argument must be a string"), span)),
        }
    }

    /// Read a byte-array arg, validating 0-255 per element (E0308 —
    /// a value error the checker cannot see statically).
    #[cfg(unix)]
    fn net_bytes(&self, args: &[usize], i: usize, span: Span) -> Result<Vec<u8>, ExecErr> {
        use crate::heap::Payload;
        let top = self.frames.len() - 1;
        match &self.frames[top].locals[args[i]] {
            RtVal::Heap(r) => {
                let st = self.shared.state.lock().expect("shared state lock");
                match st.heap.get(*r) {
                    Some(Payload::Array(elems)) => {
                        let mut out = Vec::with_capacity(elems.len());
                        for e in elems {
                            match e {
                                RtVal::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                                other => {
                                    let tn = other.type_name(&st.heap);
                                    drop(st);
                                    return Err(self.err(
                                        "E0308",
                                        format!(
                                            "cannot pass {tn} as socket bytes (elements must be ints 0-255)"
                                        ),
                                        span,
                                    ));
                                }
                            }
                        }
                        Ok(out)
                    }
                    _ => Err(self.internal("socket data argument must be an array", span)),
                }
            }
            _ => Err(self.internal("socket data argument must be an array", span)),
        }
    }

    /// Set (or clear) this task's network error slot.
    #[cfg(unix)]
    fn net_set_error(&self, msg: Option<String>) {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0].net_error = msg;
    }

    /// Take (read + clear) this task's network error slot.
    #[cfg(unix)]
    fn net_take_error(&self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0]
            .net_error
            .take()
            .unwrap_or_default()
    }

    /// Allocate a byte array on the GC heap (worker side, where the
    /// heap is available; never on the poller thread).
    #[cfg(unix)]
    fn net_alloc_bytes(&self, bytes: Vec<u8>) -> RtVal {
        RtVal::Heap(self.alloc_with(|h| {
            h.alloc_array(bytes.into_iter().map(|b| RtVal::Int(b as i64)).collect())
        }))
    }

    /// Resume mapping for scalar (int-returning) net ops: an Int
    /// resume is the result; a Unit resume means the drain set
    /// `net_error` (the Toto wrapper reads it via `net_last_error`).
    #[cfg(unix)]
    fn net_scalar_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Int(n) => self.net_assign(dst, RtVal::Int(n)),
            _ => self.net_assign(dst, RtVal::Int(-1)),
        }
    }

    /// Resume mapping for array-returning net ops: an array resume is
    /// the payload (possibly empty); a Unit resume means the drain
    /// set `net_error`. Empty + clean slot is orderly EOF.
    #[cfg(unix)]
    fn net_array_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Heap(_) => self.net_assign(dst, v),
            _ => self.net_assign(dst, self.net_alloc_bytes(Vec::new())),
        }
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn exec_net(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::NetOp;
        use toto_compiler::netsys as nn;
        // Every arm assigns `dst` and returns Ok, or returns an
        // executor error (E0308 on bad values, Cancelled on scope
        // failure, internal on impossible states). The error slot is
        // set on every path that produces a value.
        match name {
            "toto_rt_net_resolve" => {
                self.arity(name, args.len(), 1, span)?;
                let host = self.net_str(args, 0, name, span)?;
                // DNS cannot be nonblocking: park on the pool.
                if self.scope_failed() {
                    return Err(self.cancelled("net", span));
                }
                self.check_deadline(span, "net")?;
                self.maybe_collect();
                self.trace.push(TraceFrame {
                    name: "net resolve (pool)".to_string(),
                    span,
                });
                let v = match self.y.suspend(crate::sched::ToSched::Blocking {
                    op: crate::sched::BlockingOp::DnsResolve { host },
                    span,
                }) {
                    // Drain converted + set the slot; the array (empty
                    // on failure) is the whole result.
                    ToTask::Value(v) => v,
                    ToTask::Cancelled { op } => {
                        self.trace.pop();
                        return Err(self.cancelled(op, span));
                    }
                    other => {
                        let _ = other;
                        self.trace.pop();
                        return Err(self.internal("dns response", span));
                    }
                };
                self.trace.pop();
                self.net_array_resume(dst, v);
                Ok(())
            }
            "toto_rt_net_connect_ip" => {
                self.arity(name, args.len(), 2, span)?;
                let ip = self.net_str(args, 0, name, span)?;
                let port = self.net_port(args, 1, name, span)?;
                let fd = match nn::socket_tcp() {
                    Ok(fd) => fd,
                    Err(e) => {
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        return Ok(());
                    }
                };
                if let Err(e) = nn::set_nonblocking(fd) {
                    nn::close(fd).ok();
                    self.net_set_error(Some(e));
                    self.net_assign(dst, RtVal::Int(-1));
                    return Ok(());
                }
                match nn::connect_start(fd, &ip, port) {
                    nn::ConnectOutcome::Connected(fd) => {
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(fd as i64));
                        Ok(())
                    }
                    nn::ConnectOutcome::Failed(e) => {
                        nn::close(fd).ok();
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                    nn::ConnectOutcome::InProgress(fd) => {
                        // The poller waits for writability and runs
                        // SO_ERROR itself; the resume carries the
                        // verdict (NetInt on success, Unit+slot on
                        // failure).
                        let v = self.net_wait(NetOp::Connect { fd }, span)?;
                        self.net_scalar_resume(dst, v);
                        Ok(())
                    }
                }
            }
            "toto_rt_net_listen" => {
                self.arity(name, args.len(), 1, span)?;
                let port = self.net_port(args, 0, name, span)?;
                match nn::listen_loopback(port) {
                    Ok(fd) => {
                        if let Err(e) = nn::set_nonblocking(fd) {
                            nn::close(fd).ok();
                            self.net_set_error(Some(e));
                            self.net_assign(dst, RtVal::Int(-1));
                            return Ok(());
                        }
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(fd as i64));
                        Ok(())
                    }
                    Err(e) => {
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                }
            }
            "toto_rt_net_accept" => {
                self.arity(name, args.len(), 1, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                match nn::accept_nonblocking(fd) {
                    nn::AcceptOutcome::Accepted(c) => {
                        // Accepted fds must be nonblocking before any
                        // park can wait on them; a failure here is a
                        // loud error, never a blocking socket.
                        if let Err(e) = nn::set_nonblocking(c) {
                            nn::close(c).ok();
                            self.net_set_error(Some(e));
                            self.net_assign(dst, RtVal::Int(-1));
                            return Ok(());
                        }
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(c as i64));
                        Ok(())
                    }
                    nn::AcceptOutcome::WouldBlock => {
                        let v = self.net_wait(NetOp::Accept { fd }, span)?;
                        self.net_scalar_resume(dst, v);
                        Ok(())
                    }
                    nn::AcceptOutcome::Failed(e) => {
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                }
            }
            "toto_rt_net_send" => {
                self.arity(name, args.len(), 2, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let buf = self.net_bytes(args, 1, span)?;
                // Fast path first (usually succeeds on loopback); one
                // park if the buffer is full (the poller completes the
                // send — no exec-side retry needed).
                match nn::send_nonblocking(fd, &buf) {
                    nn::SendOutcome::Wrote(n) => {
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(n as i64));
                        Ok(())
                    }
                    nn::SendOutcome::WouldBlock => {
                        let op = NetOp::Send { fd, bytes: buf };
                        let v = self.net_wait(op, span)?;
                        self.net_scalar_resume(dst, v);
                        Ok(())
                    }
                    nn::SendOutcome::Closed => {
                        self.net_set_error(Some("connection closed by peer".to_string()));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                    nn::SendOutcome::Failed(e) => {
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                }
            }
            "toto_rt_net_recv" => {
                self.arity(name, args.len(), 2, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let max = self.net_count(args, 1, name, span)?;
                // Fast path first; one park on WouldBlock.
                match nn::recv_nonblocking(fd, max) {
                    nn::RecvOutcome::Got(bytes) => {
                        self.net_set_error(None);
                        let v = self.net_alloc_bytes(bytes);
                        self.net_assign(dst, v);
                        Ok(())
                    }
                    // Orderly shutdown reads as an empty array with a
                    // clean slot — NOT an error (spec §17.1).
                    nn::RecvOutcome::Closed => {
                        self.net_set_error(None);
                        let v = self.net_alloc_bytes(Vec::new());
                        self.net_assign(dst, v);
                        Ok(())
                    }
                    nn::RecvOutcome::WouldBlock => {
                        let v = self.net_wait(NetOp::Recv { fd, max }, span)?;
                        self.net_array_resume(dst, v);
                        Ok(())
                    }
                    nn::RecvOutcome::Failed(e) => {
                        self.net_set_error(Some(e));
                        let v = self.net_alloc_bytes(Vec::new());
                        self.net_assign(dst, v);
                        Ok(())
                    }
                }
            }
            "toto_rt_net_close" => {
                self.arity(name, args.len(), 1, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                // Unregister first so parked waiters fail loudly
                // instead of operating on a recycled fd number.
                crate::sched::Scheduler::net_unregister_fd(self.shared, fd, "socket closed");
                nn::forget_peer(fd);
                // A TLS session names this fd: free it first so a plain
                // `net_close` of a TLS connection never leaks the `SSL*`
                // nor leaves a stale entry for a recycled fd number.
                toto_compiler::tlslib::free_session(fd);
                match nn::close(fd) {
                    Ok(()) => {
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(0));
                        Ok(())
                    }
                    Err(e) => {
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                }
            }
            "toto_rt_net_udp_bind" => {
                self.arity(name, args.len(), 1, span)?;
                let port = self.net_port(args, 0, name, span)?;
                match nn::bind_udp_loopback(port) {
                    Ok(fd) => {
                        if let Err(e) = nn::set_nonblocking(fd) {
                            nn::close(fd).ok();
                            self.net_set_error(Some(e));
                            self.net_assign(dst, RtVal::Int(-1));
                            return Ok(());
                        }
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(fd as i64));
                        Ok(())
                    }
                    Err(e) => {
                        self.net_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                }
            }
            "toto_rt_net_udp_send_to" => {
                self.arity(name, args.len(), 4, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let buf = self.net_bytes(args, 1, span)?;
                let ip = self.net_str(args, 2, name, span)?;
                let port = self.net_port(args, 3, name, span)?;
                match nn::udp_send_to_nb(fd, &buf, &ip, port) {
                    nn::SendOutcome::Wrote(n) => {
                        self.net_set_error(None);
                        self.net_assign(dst, RtVal::Int(n as i64));
                        Ok(())
                    }
                    nn::SendOutcome::WouldBlock => {
                        let op = NetOp::UdpSend {
                            fd,
                            bytes: buf,
                            ip,
                            port,
                        };
                        let v = self.net_wait(op, span)?;
                        self.net_scalar_resume(dst, v);
                        Ok(())
                    }
                    nn::SendOutcome::Closed | nn::SendOutcome::Failed(_) => {
                        self.net_set_error(Some(nn::last_error()));
                        self.net_assign(dst, RtVal::Int(-1));
                        Ok(())
                    }
                }
            }
            "toto_rt_net_udp_recv_from" => {
                self.arity(name, args.len(), 2, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let max = self.net_count(args, 1, name, span)?;
                match nn::udp_recv_from_nb(fd, max) {
                    nn::UdpRecvOutcome::Got(bytes, host, port) => {
                        // Fast path mirrors the poller: record the peer
                        // for `net_udp_peer_*` and return the bytes. The
                        // Toto wrapper assembles the `UdpPacket` struct
                        // itself (foreign signature returns `Array<int>`).
                        nn::record_peer(fd, host, port);
                        self.net_set_error(None);
                        let v = self.net_alloc_bytes(bytes);
                        self.net_assign(dst, v);
                        Ok(())
                    }
                    // Empty datagrams are real data (UDP has no EOF).
                    nn::UdpRecvOutcome::WouldBlock => {
                        let v = self.net_wait(NetOp::UdpRecv { fd, max }, span)?;
                        self.net_array_resume(dst, v);
                        Ok(())
                    }
                    nn::UdpRecvOutcome::Failed(e) => {
                        self.net_set_error(Some(e));
                        let v = self.net_alloc_bytes(Vec::new());
                        self.net_assign(dst, v);
                        Ok(())
                    }
                }
            }
            "toto_rt_net_udp_peer_host" => {
                self.arity(name, args.len(), 1, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let v = match nn::lookup_peer(fd) {
                    Some((host, _)) => self.alloc_with(|h| h.alloc_str(&host)),
                    None => self.alloc_with(|h| h.alloc_str("")),
                };
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            "toto_rt_net_udp_peer_port" => {
                self.arity(name, args.len(), 1, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let v = match nn::lookup_peer(fd) {
                    Some((_, port)) => RtVal::Int(port as i64),
                    None => RtVal::Int(0),
                };
                self.net_assign(dst, v);
                Ok(())
            }
            "toto_rt_net_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                // Take the error BEFORE allocating: `alloc_with`
                // holds the heap-state lock, and the drain nests
                // sched -> state, so taking the sched lock inside the
                // closure inverts the order and deadlocks M:N (Wave 1
                // fix-forward: ABBA between this arm and
                // `drain_completions_locked`). Same shape as the
                // crypto/gzip last-error arms.
                let e = self.net_take_error();
                let v = self.alloc_with(|h| h.alloc_str(&e));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            _ => Err(self.internal("unknown net function", span)),
        }
    }

    /// Crypto foreign functions (Wave 1, unix-only): one-shot
    /// OpenSSL calls dispatched by name from `exec_foreign`.
    ///
    /// Shape: parse args with the shared `net_*` readers (byte
    /// arrays validated 0–255, `E0308` on bad values), call the
    /// `toto_compiler::cryptolib` bridge inline on the worker, assign
    /// `dst`, and set the per-task `crypto_error` slot on every path
    /// that produces a value (read and cleared by
    /// `toto_rt_crypto_last_error`). Array results carry the payload
    /// (empty on failure); `verify`-style results are 1/0 (slot clean)
    /// or -1 + slot on malformed input. Never parks, never blocks.
    #[cfg(unix)]
    const CRYPTO_FNS: &[&str] = &[
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
        "toto_rt_crypto_pbkdf2",
        "toto_rt_crypto_pbkdf2_verify",
        "toto_rt_crypto_rsa_verify",
        "toto_rt_crypto_x509_verify",
        "toto_rt_crypto_last_error",
        "toto_rt_crypto_version",
    ];

    /// Set (or clear) this task's crypto error slot.
    #[cfg(unix)]
    fn crypto_set_error(&mut self, msg: Option<String>) {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0].crypto_error = msg;
    }

    /// Take (read + clear) this task's crypto error slot.
    #[cfg(unix)]
    fn crypto_take_error(&mut self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0]
            .crypto_error
            .take()
            .unwrap_or_default()
    }

    /// TLS foreign functions (Wave 2, Stage 1, unix-only): blocking
    /// libssl steps dispatched by name from `exec_foreign`.
    ///
    /// Shape: parse args with the shared `net_*` readers (fds, byte
    /// arrays validated 0–255, `E0308` on bad values), park the task
    /// on the dedicated blocking pool (`BlockingOp::Tls`), and map
    /// the resume with `tls_scalar_resume` / `tls_array_resume`. The
    /// drain sets the per-task `tls_error` slot on every completion
    /// (read and cleared by `toto_rt_tls_last_error`). Scalar results
    /// are the fd/count or -1; array results carry the payload (empty
    /// on failure, or orderly shutdown when the slot stays clean).
    #[cfg(unix)]
    const TLS_FNS: &[&str] = &[
        "toto_rt_tls_connect",
        "toto_rt_tls_accept",
        "toto_rt_tls_send",
        "toto_rt_tls_recv",
        "toto_rt_tls_close",
        "toto_rt_tls_last_error",
    ];

    /// Take (read + clear) this task's TLS error slot.
    #[cfg(unix)]
    fn tls_take_error(&mut self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0]
            .tls_error
            .take()
            .unwrap_or_default()
    }

    /// Resume mapping for scalar TLS ops: an Int resume is the
    /// result; a Unit resume means the drain set `tls_error` (assign
    /// -1 so the Toto wrapper reports the slot text).
    #[cfg(unix)]
    fn tls_scalar_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Int(n) => self.net_assign(dst, RtVal::Int(n)),
            _ => self.net_assign(dst, RtVal::Int(-1)),
        }
    }

    /// Resume mapping for array-returning TLS ops: a Heap resume is
    /// the payload (possibly empty); anything else means the drain
    /// set `tls_error`. Empty + clean slot is orderly shutdown.
    #[cfg(unix)]
    fn tls_array_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Heap(_) => self.net_assign(dst, v),
            _ => self.net_assign(dst, self.net_alloc_bytes(Vec::new())),
        }
    }

    /// Park the current task on one blocking TLS step. The resume
    /// value IS the finished result: the pool ran the libssl call
    /// and the drain converted it (setting the error slot).
    /// Cancellations and unexpected responses propagate as errors.
    #[cfg(unix)]
    fn tls_wait(&mut self, op: crate::sched::TlsOp, span: Span) -> Result<RtVal, ExecErr> {
        if self.scope_failed() {
            return Err(self.cancelled("tls", span));
        }
        self.check_deadline(span, "tls")?;
        self.maybe_collect();
        self.trace.push(TraceFrame {
            name: "tls (pool)".to_string(),
            span,
        });
        let out = match self.y.suspend(crate::sched::ToSched::Blocking {
            op: crate::sched::BlockingOp::Tls { op },
            span,
        }) {
            ToTask::Value(v) => Ok(v),
            ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
            other => {
                let _ = other;
                Err(self.internal("tls response", span))
            }
        };
        self.trace.pop();
        out
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn exec_tls(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::TlsOp;
        // Every arm assigns `dst` and returns Ok, or returns an
        // executor error (E0308 on bad values, Cancelled on scope
        // failure, internal on impossible states). The error slot is
        // set by the drain on every pool completion.
        match name {
            "toto_rt_tls_connect" => {
                self.arity(name, args.len(), 2, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let ca_pem = self.net_str(args, 1, name, span)?;
                let v = self.tls_wait(TlsOp::Connect { fd, ca_pem }, span)?;
                self.tls_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_tls_accept" => {
                self.arity(name, args.len(), 3, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let cert_pem = self.net_str(args, 1, name, span)?;
                let key_pem = self.net_str(args, 2, name, span)?;
                let v = self.tls_wait(
                    TlsOp::Accept {
                        fd,
                        cert_pem,
                        key_pem,
                    },
                    span,
                )?;
                self.tls_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_tls_send" => {
                self.arity(name, args.len(), 2, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let bytes = self.net_bytes(args, 1, span)?;
                let v = self.tls_wait(TlsOp::Send { fd, bytes }, span)?;
                self.tls_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_tls_recv" => {
                self.arity(name, args.len(), 2, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                let max = self.net_count(args, 1, name, span)?;
                let v = self.tls_wait(TlsOp::Recv { fd, max }, span)?;
                self.tls_array_resume(dst, v);
                Ok(())
            }
            "toto_rt_tls_close" => {
                self.arity(name, args.len(), 1, span)?;
                let fd = self.net_fd(args, 0, name, span)?;
                // Unregister first so parked waiters fail loudly
                // instead of operating on a recycled fd number.
                crate::sched::Scheduler::net_unregister_fd(self.shared, fd, "socket closed");
                toto_compiler::netsys::forget_peer(fd);
                let v = self.tls_wait(TlsOp::Close { fd }, span)?;
                self.tls_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_tls_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                let e = self.tls_take_error();
                let v = self.alloc_with(|h| h.alloc_str(&e));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            _ => Err(self.internal("tls dispatch", span)),
        }
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn exec_crypto(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use toto_compiler::cryptolib as cc;
        // Map a bridge `Result<Vec<u8>, String>` to an array assign +
        // slot update (empty array + slot on failure).
        macro_rules! array_call {
            ($call:expr) => {{
                match $call {
                    Ok(bytes) => {
                        self.crypto_set_error(None);
                        let v = self.net_alloc_bytes(bytes);
                        self.net_assign(dst, v);
                    }
                    Err(e) => {
                        self.crypto_set_error(Some(e));
                        let v = self.net_alloc_bytes(Vec::new());
                        self.net_assign(dst, v);
                    }
                }
            }};
        }
        // Map a bridge `Result<bool, String>` to a 1/0/-1 assign.
        macro_rules! verify_call {
            ($call:expr) => {{
                match $call {
                    Ok(true) => {
                        self.crypto_set_error(None);
                        self.net_assign(dst, RtVal::Int(1));
                    }
                    Ok(false) => {
                        self.crypto_set_error(None);
                        self.net_assign(dst, RtVal::Int(0));
                    }
                    Err(e) => {
                        self.crypto_set_error(Some(e));
                        self.net_assign(dst, RtVal::Int(-1));
                    }
                }
            }};
        }
        match name {
            "toto_rt_crypto_sha256" => {
                self.arity(name, args.len(), 1, span)?;
                let buf = self.net_bytes(args, 0, span)?;
                array_call!(cc::sha256(&buf));
                Ok(())
            }
            "toto_rt_crypto_sha512" => {
                self.arity(name, args.len(), 1, span)?;
                let buf = self.net_bytes(args, 0, span)?;
                array_call!(cc::sha512(&buf));
                Ok(())
            }
            "toto_rt_crypto_hmac256" => {
                self.arity(name, args.len(), 2, span)?;
                let key = self.net_bytes(args, 0, span)?;
                let data = self.net_bytes(args, 1, span)?;
                array_call!(cc::hmac256(&key, &data));
                Ok(())
            }
            "toto_rt_crypto_hmac512" => {
                self.arity(name, args.len(), 2, span)?;
                let key = self.net_bytes(args, 0, span)?;
                let data = self.net_bytes(args, 1, span)?;
                array_call!(cc::hmac512(&key, &data));
                Ok(())
            }
            "toto_rt_crypto_aead_encrypt" => {
                self.arity(name, args.len(), 4, span)?;
                let key = self.net_bytes(args, 0, span)?;
                let nonce = self.net_bytes(args, 1, span)?;
                let pt = self.net_bytes(args, 2, span)?;
                let aad = self.net_bytes(args, 3, span)?;
                array_call!(cc::aead_encrypt(&key, &nonce, &pt, &aad));
                Ok(())
            }
            "toto_rt_crypto_aead_decrypt" => {
                self.arity(name, args.len(), 4, span)?;
                let key = self.net_bytes(args, 0, span)?;
                let nonce = self.net_bytes(args, 1, span)?;
                let cttag = self.net_bytes(args, 2, span)?;
                let aad = self.net_bytes(args, 3, span)?;
                array_call!(cc::aead_decrypt(&key, &nonce, &cttag, &aad));
                Ok(())
            }
            "toto_rt_crypto_sign_keygen" => {
                self.arity(name, args.len(), 0, span)?;
                array_call!(cc::sign_keygen());
                Ok(())
            }
            "toto_rt_crypto_sign_sign" => {
                self.arity(name, args.len(), 2, span)?;
                let secret = self.net_bytes(args, 0, span)?;
                let msg = self.net_bytes(args, 1, span)?;
                array_call!(cc::sign_sign(&secret, &msg));
                Ok(())
            }
            "toto_rt_crypto_sign_verify" => {
                self.arity(name, args.len(), 3, span)?;
                let public = self.net_bytes(args, 0, span)?;
                let msg = self.net_bytes(args, 1, span)?;
                let sig = self.net_bytes(args, 2, span)?;
                verify_call!(cc::sign_verify(&public, &msg, &sig));
                Ok(())
            }
            "toto_rt_crypto_random" => {
                self.arity(name, args.len(), 1, span)?;
                let n = self.net_int(args, 0, name, span)?;
                if !(1..=65536).contains(&n) {
                    self.crypto_set_error(Some(format!("random: need 1-65536 bytes, got {n}")));
                    let v = self.net_alloc_bytes(Vec::new());
                    self.net_assign(dst, v);
                    return Ok(());
                }
                array_call!(cc::random_bytes(n as usize));
                Ok(())
            }
            "toto_rt_crypto_pbkdf2" => {
                self.arity(name, args.len(), 3, span)?;
                let password = self.net_str(args, 0, name, span)?;
                let salt = self.net_bytes(args, 1, span)?;
                let iterations = self.net_int(args, 2, name, span)?;
                array_call!(cc::pbkdf2_sha256(&password, &salt, iterations));
                Ok(())
            }
            "toto_rt_crypto_pbkdf2_verify" => {
                self.arity(name, args.len(), 2, span)?;
                let expected = self.net_bytes(args, 0, span)?;
                let got = self.net_bytes(args, 1, span)?;
                verify_call!(cc::pbkdf2_verify(&expected, &got));
                Ok(())
            }
            "toto_rt_crypto_rsa_verify" => {
                self.arity(name, args.len(), 4, span)?;
                let n = self.net_bytes(args, 0, span)?;
                let e = self.net_bytes(args, 1, span)?;
                let digest = self.net_bytes(args, 2, span)?;
                let sig = self.net_bytes(args, 3, span)?;
                verify_call!(cc::rsa_verify(&n, &e, &digest, &sig));
                Ok(())
            }
            "toto_rt_crypto_x509_verify" => {
                self.arity(name, args.len(), 2, span)?;
                let leaf = self.net_str(args, 0, name, span)?;
                let ca = self.net_str(args, 1, name, span)?;
                verify_call!(cc::x509_verify(&leaf, &ca));
                Ok(())
            }
            "toto_rt_crypto_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                let e = self.crypto_take_error();
                let v = self.alloc_with(|h| h.alloc_str(&e));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            "toto_rt_crypto_version" => {
                self.arity(name, args.len(), 0, span)?;
                let v = self.alloc_with(|h| h.alloc_str(&cc::openssl_version()));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            _ => Err(self.internal("unknown crypto function", span)),
        }
    }

    /// Gzip foreign functions (Wave 1, unix-only): one-shot
    /// pure-Rust compression dispatched by name from `exec_foreign`.
    /// Same shape as crypto (inline on the worker, per-task
    /// `gzip_error` slot, array results); never parks, never blocks.
    #[cfg(unix)]
    const GZIP_FNS: &[&str] = &[
        "toto_rt_gzip_compress",
        "toto_rt_gzip_decompress",
        "toto_rt_gzip_last_error",
    ];

    /// Set (or clear) this task's gzip error slot.
    #[cfg(unix)]
    fn gzip_set_error(&mut self, msg: Option<String>) {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0].gzip_error = msg;
    }

    /// Take (read + clear) this task's gzip error slot.
    #[cfg(unix)]
    fn gzip_take_error(&mut self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0]
            .gzip_error
            .take()
            .unwrap_or_default()
    }

    #[cfg(unix)]
    fn exec_gzip(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use toto_compiler::compress as gz;
        match name {
            "toto_rt_gzip_compress" => {
                self.arity(name, args.len(), 1, span)?;
                let buf = self.net_bytes(args, 0, span)?;
                match gz::gzip_compress(&buf) {
                    Ok(bytes) => {
                        self.gzip_set_error(None);
                        let v = self.net_alloc_bytes(bytes);
                        self.net_assign(dst, v);
                    }
                    Err(e) => {
                        self.gzip_set_error(Some(e));
                        let v = self.net_alloc_bytes(Vec::new());
                        self.net_assign(dst, v);
                    }
                }
                Ok(())
            }
            "toto_rt_gzip_decompress" => {
                self.arity(name, args.len(), 1, span)?;
                let buf = self.net_bytes(args, 0, span)?;
                match gz::gzip_decompress(&buf) {
                    Ok(bytes) => {
                        self.gzip_set_error(None);
                        let v = self.net_alloc_bytes(bytes);
                        self.net_assign(dst, v);
                    }
                    Err(e) => {
                        self.gzip_set_error(Some(e));
                        let v = self.net_alloc_bytes(Vec::new());
                        self.net_assign(dst, v);
                    }
                }
                Ok(())
            }
            "toto_rt_gzip_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                let e = self.gzip_take_error();
                let v = self.alloc_with(|h| h.alloc_str(&e));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            _ => Err(self.internal("unknown gzip function", span)),
        }
    }

    /// Context slot functions (Wave 2, Stage 3, unix-only): inline
    /// per-task slot access dispatched by name from `exec_foreign`.
    ///
    /// Shape: parse args with the shared `net_*` readers, touch only
    /// this task's slot (plain data, one short `sched` section —
    /// never nested inside the heap lock), assign `dst`. Reads stay
    /// available after expiry; nothing here parks, so no deadline
    /// check (enforcement is at the suspension points, not the
    /// slot). `cancel` yields the `CtxCancel` scheduler request,
    /// which fails the named scope through the standard sweep.
    #[cfg(unix)]
    const CTX_FNS: &[&str] = &[
        "toto_rt_ctx_begin",
        "toto_rt_ctx_add",
        "toto_rt_ctx_count",
        "toto_rt_ctx_key",
        "toto_rt_ctx_val",
        "toto_rt_ctx_deadline",
        "toto_rt_ctx_set_deadline",
        "toto_rt_ctx_with_cancel",
        "toto_rt_ctx_cancel",
        "toto_rt_ctx_detach_next",
        "toto_rt_ctx_last_error",
    ];

    /// Clock functions (Wave 2, Stage 3, unix-only): the shared
    /// monotonic process clock. Observability only — never gated.
    #[cfg(unix)]
    const CLOCK_FNS: &[&str] = &["toto_rt_clock_monotonic_ms"];

    /// Structured-logging functions (Wave 2, Stage 3, unix-only):
    /// inline emission through the process log gate. `emit` returns
    /// the formatted line for the stdout sink only (the Toto wrapper
    /// prints it through `println`, keeping one output path); other
    /// sinks and filtered calls return "" (the error slot tells
    /// misuse apart from filtering).
    #[cfg(unix)]
    const LOG_FNS: &[&str] = &[
        "toto_rt_log_emit",
        "toto_rt_log_set_level",
        "toto_rt_log_set_sink",
        "toto_rt_log_drain",
        "toto_rt_log_last_error",
    ];

    /// Set (or clear) this task's context error slot.
    #[cfg(unix)]
    fn ctx_set_error(&mut self, msg: Option<String>) {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0].ctx_error = msg;
    }

    /// Take (read + clear) this task's context error slot.
    #[cfg(unix)]
    fn ctx_take_error(&mut self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0]
            .ctx_error
            .take()
            .unwrap_or_default()
    }

    /// Assign a fresh heap string to `dst` (single home for the
    /// read-back arms below: take the plain data first — the
    /// `sched` lock — then allocate — the heap-state lock — same
    /// order as the `last_error` arms, never nested).
    #[cfg(unix)]
    fn ctx_assign_str(&mut self, dst: Option<usize>, s: &str) {
        let v = self.alloc_with(|h| h.alloc_str(s));
        self.net_assign(dst, RtVal::Heap(v));
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn exec_ctx(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        match name {
            "toto_rt_ctx_begin" => {
                self.arity(name, args.len(), 0, span)?;
                {
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    sched.slots[self.task.0].ctx.clear();
                }
                self.ctx_set_error(None);
                self.net_assign(dst, RtVal::Int(0));
                Ok(())
            }
            "toto_rt_ctx_add" => {
                self.arity(name, args.len(), 2, span)?;
                let key = self.net_str(args, 0, name, span)?;
                let val = self.net_str(args, 1, name, span)?;
                let n = {
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    let slot = &mut sched.slots[self.task.0].ctx;
                    slot.add(key, val);
                    slot.pairs.len() as i64
                };
                self.ctx_set_error(None);
                self.net_assign(dst, RtVal::Int(n));
                Ok(())
            }
            "toto_rt_ctx_count" => {
                self.arity(name, args.len(), 0, span)?;
                let n = {
                    let sched = self.shared.sched.lock().expect("sched lock");
                    sched.slots[self.task.0].ctx.pairs.len() as i64
                };
                self.net_assign(dst, RtVal::Int(n));
                Ok(())
            }
            "toto_rt_ctx_key" => {
                self.arity(name, args.len(), 1, span)?;
                let i = self.net_int(args, 0, name, span)?;
                let got = {
                    let sched = self.shared.sched.lock().expect("sched lock");
                    let pairs = &sched.slots[self.task.0].ctx.pairs;
                    (i >= 0)
                        .then(|| pairs.get(i as usize))
                        .flatten()
                        .map(|(k, _)| k.clone())
                };
                match got {
                    Some(k) => {
                        self.ctx_set_error(None);
                        self.ctx_assign_str(dst, &k);
                    }
                    None => {
                        let n = {
                            let sched = self.shared.sched.lock().expect("sched lock");
                            sched.slots[self.task.0].ctx.pairs.len()
                        };
                        self.ctx_set_error(Some(format!(
                            "context index out of range (need 0..{n}, got {i})"
                        )));
                        self.ctx_assign_str(dst, "");
                    }
                }
                Ok(())
            }
            "toto_rt_ctx_val" => {
                self.arity(name, args.len(), 1, span)?;
                let i = self.net_int(args, 0, name, span)?;
                let got = {
                    let sched = self.shared.sched.lock().expect("sched lock");
                    let pairs = &sched.slots[self.task.0].ctx.pairs;
                    (i >= 0)
                        .then(|| pairs.get(i as usize))
                        .flatten()
                        .map(|(_, v)| v.clone())
                };
                match got {
                    Some(v) => {
                        self.ctx_set_error(None);
                        self.ctx_assign_str(dst, &v);
                    }
                    None => {
                        let n = {
                            let sched = self.shared.sched.lock().expect("sched lock");
                            sched.slots[self.task.0].ctx.pairs.len()
                        };
                        self.ctx_set_error(Some(format!(
                            "context index out of range (need 0..{n}, got {i})"
                        )));
                        self.ctx_assign_str(dst, "");
                    }
                }
                Ok(())
            }
            "toto_rt_ctx_deadline" => {
                self.arity(name, args.len(), 0, span)?;
                let d = {
                    let sched = self.shared.sched.lock().expect("sched lock");
                    sched.slots[self.task.0].ctx.deadline.unwrap_or(-1)
                };
                self.net_assign(dst, RtVal::Int(d));
                Ok(())
            }
            "toto_rt_ctx_set_deadline" => {
                self.arity(name, args.len(), 1, span)?;
                let d = self.net_int(args, 0, name, span)?;
                {
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    // The none-sentinel is EXACTLY -1 (see the
                    // interpreter arm): negative absolute times are
                    // legitimate past deadlines.
                    sched.slots[self.task.0].ctx.deadline = if d == -1 { None } else { Some(d) };
                }
                self.ctx_set_error(None);
                self.net_assign(dst, RtVal::Int(0));
                Ok(())
            }
            "toto_rt_ctx_with_cancel" => {
                self.arity(name, args.len(), 0, span)?;
                // Snapshot design (ADR-009 consequences): the handle
                // names the caller's CURRENT scope. No scope is
                // created and no task moves: `cancel` fails exactly
                // the scope this task was in at `with_cancel` time.
                self.ctx_set_error(None);
                self.net_assign(dst, RtVal::Int(self.scope.0 as i64));
                Ok(())
            }
            "toto_rt_ctx_cancel" => {
                self.arity(name, args.len(), 1, span)?;
                let h = self.net_int(args, 0, name, span)?;
                let known = h >= 0
                    && (h as usize)
                        < self
                            .shared
                            .scopes
                            .lock()
                            .expect("scopes lock")
                            .scope_count();
                if !known {
                    // Loud but non-fatal (the net/tls `-1` + slot
                    // discipline): the wrapper turns this into an
                    // `Err` value, so validation stays an ordinary
                    // error, not a scope failure.
                    self.ctx_set_error(Some(format!("invalid cancel handle {h} (no such scope)")));
                    self.net_assign(dst, RtVal::Int(-1));
                    return Ok(());
                }
                match self.yield_now(crate::sched::ToSched::CtxCancel {
                    scope: crate::sched::ScopeId(h as usize),
                    handle: h,
                    span,
                    trace: self.trace.clone(),
                }) {
                    ToTask::Unit => {
                        self.ctx_set_error(None);
                        self.net_assign(dst, RtVal::Int(0));
                        Ok(())
                    }
                    ToTask::OpPanic { diag } => Err(ExecErr::Toto(diag)),
                    ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
                    other => {
                        let _ = other;
                        Err(self.internal("cancel response", span))
                    }
                }
            }
            "toto_rt_ctx_detach_next" => {
                self.arity(name, args.len(), 0, span)?;
                {
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    sched.slots[self.task.0].ctx.detach_once = true;
                }
                self.ctx_set_error(None);
                self.net_assign(dst, RtVal::Int(0));
                Ok(())
            }
            "toto_rt_ctx_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                let e = self.ctx_take_error();
                self.ctx_assign_str(dst, &e);
                Ok(())
            }
            _ => Err(self.internal("unknown context function", span)),
        }
    }

    #[cfg(unix)]
    fn exec_clock(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        match name {
            "toto_rt_clock_monotonic_ms" => {
                self.arity(name, args.len(), 0, span)?;
                self.net_assign(dst, RtVal::Int(toto_compiler::ctxlog::monotonic_ms()));
                Ok(())
            }
            _ => Err(self.internal("unknown clock function", span)),
        }
    }

    #[cfg(unix)]
    fn exec_log(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        match name {
            "toto_rt_log_emit" => {
                self.arity(name, args.len(), 2, span)?;
                let level = self.net_int(args, 0, name, span)? as i32;
                let msg = self.net_str(args, 1, name, span)?;
                // Take the pairs BEFORE allocating: `ctx_assign_str`
                // holds the heap-state lock, and the drain nests
                // sched -> state, so taking the sched lock inside the
                // closure inverts the order (Wave 1 fix-forward
                // ABBA rule, same as the `last_error` arms).
                let pairs = {
                    let sched = self.shared.sched.lock().expect("sched lock");
                    sched.slots[self.task.0].ctx.pairs.clone()
                };
                let (_rc, line) = toto_compiler::ctxlog::log_emit(level, &msg, &pairs);
                self.ctx_assign_str(dst, &line.unwrap_or_default());
                Ok(())
            }
            "toto_rt_log_set_level" => {
                self.arity(name, args.len(), 1, span)?;
                let level = self.net_int(args, 0, name, span)? as i32;
                let rc = toto_compiler::ctxlog::log_set_level(level);
                self.net_assign(dst, RtVal::Int(rc));
                Ok(())
            }
            "toto_rt_log_set_sink" => {
                self.arity(name, args.len(), 1, span)?;
                let sink = self.net_int(args, 0, name, span)? as i32;
                let rc = toto_compiler::ctxlog::log_set_sink(sink);
                self.net_assign(dst, RtVal::Int(rc));
                Ok(())
            }
            "toto_rt_log_drain" => {
                self.arity(name, args.len(), 0, span)?;
                let drained = toto_compiler::ctxlog::log_drain();
                self.ctx_assign_str(dst, &drained);
                Ok(())
            }
            "toto_rt_log_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                self.ctx_assign_str(dst, &toto_compiler::ctxlog::take_log_error());
                Ok(())
            }
            _ => Err(self.internal("unknown log function", span)),
        }
    }

    /// Database foreign functions (Wave 3, Stage 1, unix-only):
    /// pooled client-library steps dispatched by name from
    /// `exec_foreign`. The pool (checkout, connect, query, fetch)
    /// runs on the dedicated blocking pool; the worker never
    /// blocks. Same error discipline as net/TLS (scalar ops
    /// return the count or -1; the query op returns the framed
    /// rows string or "" with the slot set; the message lands in
    /// the call-duration slot read by `toto_rt_db_last_error`).
    /// Params cross as the framing string and rows cross back as
    /// the framed string on both engines (checked code cannot name
    /// structs in foreign signatures, so no checker change and no
    /// new types) — one Toto wrapper, one pure-Toto decoder.
    #[cfg(unix)]
    const DB_FNS: &[&str] = &[
        "toto_rt_db_exec",
        "toto_rt_db_query",
        "toto_rt_db_pool_config",
        "toto_rt_db_close_pool",
        "toto_rt_db_last_error",
    ];

    /// Take (read + clear) this task's DB error slot.
    #[cfg(unix)]
    fn db_take_error(&mut self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0].db_error.take().unwrap_or_default()
    }

    /// Resume mapping for scalar DB ops: an Int resume is the
    /// result; a Unit resume means the drain set `db_error` (assign
    /// -1 so the Toto wrapper reports the slot text).
    #[cfg(unix)]
    fn db_scalar_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Int(n) => self.net_assign(dst, RtVal::Int(n)),
            _ => self.net_assign(dst, RtVal::Int(-1)),
        }
    }

    /// Park the current task on one blocking DB step. The resume
    /// value IS the finished result: the pool ran the
    /// checkout/connect/query and the drain converted it (setting
    /// the error slot). Cancellations and unexpected responses
    /// propagate as errors.
    #[cfg(unix)]
    fn db_wait(&mut self, op: crate::sched::DbOp, span: Span) -> Result<RtVal, ExecErr> {
        if self.scope_failed() {
            return Err(self.cancelled("db", span));
        }
        self.check_deadline(span, "db")?;
        self.maybe_collect();
        self.trace.push(TraceFrame {
            name: "db (pool)".to_string(),
            span,
        });
        let out = match self.y.suspend(crate::sched::ToSched::Blocking {
            op: crate::sched::BlockingOp::Db { op },
            span,
        }) {
            ToTask::Value(v) => Ok(v),
            ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
            other => {
                let _ = other;
                Err(self.internal("db response", span))
            }
        };
        self.trace.pop();
        out
    }

    /// Read the params framing string into plain cells (E0308 on
    /// bad framing — pure Toto builds it, so this fires only on
    /// hand-built strings).
    #[cfg(unix)]
    fn db_params(
        &self,
        args: &[usize],
        i: usize,
        span: Span,
    ) -> Result<Vec<toto_compiler::dblib::DbCell>, ExecErr> {
        let framing = self.net_str(args, i, "db params framing", span)?;
        toto_compiler::dblib::decode_params(&framing).map_err(|e| {
            self.err(
                "E0308",
                format!("bad db params framing ({e}; build it with db_encode_params)"),
                span,
            )
        })
    }

    /// Read a framed rows string (the drain's `DbRows` conversion).
    /// Empty (with a clean slot) is a rowless result; empty with a
    /// set slot is a failure the wrapper reports.
    #[cfg(unix)]
    fn db_rows_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Heap(_) => self.net_assign(dst, v),
            _ => self.net_assign(dst, RtVal::Heap(self.alloc_with(|h| h.alloc_str("")))),
        }
    }

    /// Read a non-negative int arg (pool max / timeout ms).
    #[cfg(unix)]
    fn db_nonneg(&self, args: &[usize], i: usize, name: &str, span: Span) -> Result<i64, ExecErr> {
        let n = self.net_int(args, i, name, span)?;
        if n < 0 {
            return Err(self.err(
                "E0308",
                format!("db pool config needs non-negative numbers, got {n}"),
                span,
            ));
        }
        Ok(n)
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn exec_db(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::DbOp;
        // Every arm assigns `dst` and returns Ok, or returns an
        // executor error (E0308 on bad values, Cancelled on scope
        // failure, internal on impossible states). The error slot is
        // set by the drain on every pool completion.
        match name {
            "toto_rt_db_exec" => {
                self.arity(name, args.len(), 3, span)?;
                let dsn = self.net_str(args, 0, name, span)?;
                let sql = self.net_str(args, 1, name, span)?;
                let params = self.db_params(args, 2, span)?;
                let v = self.db_wait(DbOp::Exec { dsn, sql, params }, span)?;
                self.db_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_db_query" => {
                self.arity(name, args.len(), 3, span)?;
                let dsn = self.net_str(args, 0, name, span)?;
                let sql = self.net_str(args, 1, name, span)?;
                let params = self.db_params(args, 2, span)?;
                let v = self.db_wait(DbOp::Query { dsn, sql, params }, span)?;
                self.db_rows_resume(dst, v);
                Ok(())
            }
            "toto_rt_db_pool_config" => {
                self.arity(name, args.len(), 3, span)?;
                let dsn = self.net_str(args, 0, name, span)?;
                let max = self.db_nonneg(args, 1, name, span)? as u32;
                let timeout_ms = self.db_nonneg(args, 2, name, span)? as u64;
                let v = self.db_wait(
                    DbOp::PoolConfig {
                        dsn,
                        max,
                        timeout_ms,
                    },
                    span,
                )?;
                self.db_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_db_close_pool" => {
                self.arity(name, args.len(), 1, span)?;
                let dsn = self.net_str(args, 0, name, span)?;
                let v = self.db_wait(DbOp::ClosePool { dsn }, span)?;
                self.db_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_db_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                let e = self.db_take_error();
                let v = self.alloc_with(|h| h.alloc_str(&e));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            _ => Err(self.internal("unknown database function", span)),
        }
    }

    /// Environment lookups (Wave 3, Stage 4, unix-only): `getenv`
    /// is a pure process-table read — inline on the worker, never
    /// parked. Names cross as strings; `*_has` answers presence
    /// (empty-but-set reads as "").
    #[cfg(unix)]
    const ENV_FNS: &[&str] = &["toto_rt_env_get", "toto_rt_env_has"];

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn exec_env(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        let read = |this: &Self, i: usize| -> Result<Option<String>, ExecErr> {
            let s = this.net_str(args, i, name, span)?;
            toto_compiler::netsys::env_get(&s).map_err(|e| this.internal(&e, span))
        };
        match name {
            "toto_rt_env_get" => {
                self.arity(name, args.len(), 1, span)?;
                let v = read(self, 0)?.unwrap_or_default();
                let r = self.alloc_with(|h| h.alloc_str(&v));
                self.net_assign(dst, RtVal::Heap(r));
                Ok(())
            }
            "toto_rt_env_has" => {
                self.arity(name, args.len(), 1, span)?;
                let present = read(self, 0)?.is_some();
                self.net_assign(dst, RtVal::Int(if present { 1 } else { 0 }));
                Ok(())
            }
            _ => Err(self.internal("unknown env function", span)),
        }
    }

    /// LDAP foreign functions (Wave 3, Stage 3, Linux-only):
    /// directory steps dispatched by name from `exec_foreign`.
    /// Connect, bind, and operate run on the dedicated blocking
    /// pool (an LDAP round-trip blocks) — never inline on a
    /// worker, never on the poller. Same error discipline as db
    /// (scalar ops return the count or -1; the search op returns
    /// the JSON string or "" with the slot set; the message lands
    /// in the call-duration slot read by
    /// `toto_rt_ldap_last_error`). No handle cache in v1 — each
    /// op is self-contained.
    #[cfg(target_os = "linux")]
    const LDAP_FNS: &[&str] = &[
        "toto_rt_ldap_bind",
        "toto_rt_ldap_search",
        "toto_rt_ldap_last_error",
    ];

    /// Take (read + clear) this task's LDAP error slot.
    #[cfg(target_os = "linux")]
    fn ldap_take_error(&mut self) -> String {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        sched.slots[self.task.0]
            .ldap_error
            .take()
            .unwrap_or_default()
    }

    /// Resume mapping for scalar LDAP ops: an Int resume is the
    /// result; a Unit resume means the drain set `ldap_error`
    /// (assign -1 so the Toto wrapper reports the slot text).
    #[cfg(target_os = "linux")]
    fn ldap_scalar_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Int(n) => self.net_assign(dst, RtVal::Int(n)),
            _ => self.net_assign(dst, RtVal::Int(-1)),
        }
    }

    /// Resume mapping for the search op: a Heap (string) resume is
    /// the JSON result (possibly `"[]"`); anything else means the
    /// drain set `ldap_error` (assign "" so the wrapper reports
    /// the slot text).
    #[cfg(target_os = "linux")]
    fn ldap_str_resume(&mut self, dst: Option<usize>, v: RtVal) {
        match v {
            RtVal::Heap(_) => self.net_assign(dst, v),
            _ => self.net_assign(dst, RtVal::Heap(self.alloc_with(|h| h.alloc_str("")))),
        }
    }

    /// Park the current task on one blocking LDAP step. The resume
    /// value IS the finished result: the pool ran the
    /// connect/bind/operate and the drain converted it (setting
    /// the error slot). Cancellations and unexpected responses
    /// propagate as errors.
    #[cfg(target_os = "linux")]
    fn ldap_wait(&mut self, op: crate::sched::LdapOp, span: Span) -> Result<RtVal, ExecErr> {
        if self.scope_failed() {
            return Err(self.cancelled("ldap", span));
        }
        self.check_deadline(span, "ldap")?;
        self.maybe_collect();
        self.trace.push(TraceFrame {
            name: "ldap (pool)".to_string(),
            span,
        });
        let out = match self.y.suspend(crate::sched::ToSched::Blocking {
            op: crate::sched::BlockingOp::Ldap { op },
            span,
        }) {
            ToTask::Value(v) => Ok(v),
            ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
            other => {
                let _ = other;
                Err(self.internal("ldap response", span))
            }
        };
        self.trace.pop();
        out
    }

    /// Read a port arg (1-65535, E0308 outside).
    #[cfg(target_os = "linux")]
    fn ldap_port(&self, args: &[usize], i: usize, name: &str, span: Span) -> Result<u16, ExecErr> {
        let n = self.net_int(args, i, name, span)?;
        if !(1..=65535).contains(&n) {
            return Err(self.err("E0308", format!("ldap: bad port {n} (want 1-65535)"), span));
        }
        Ok(n as u16)
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_lines)]
    fn exec_ldap(
        &mut self,
        dst: Option<usize>,
        name: &str,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        use crate::sched::LdapOp;
        // Every arm assigns `dst` and returns Ok, or returns an
        // executor error (E0308 on bad values, Cancelled on scope
        // failure, internal on impossible states). The error slot is
        // set by the drain on every pool completion.
        match name {
            "toto_rt_ldap_bind" => {
                self.arity(name, args.len(), 4, span)?;
                let host = self.net_str(args, 0, name, span)?;
                let port = self.ldap_port(args, 1, name, span)?;
                let bind_dn = self.net_str(args, 2, name, span)?;
                let password = self.net_str(args, 3, name, span)?;
                let v = self.ldap_wait(
                    LdapOp::Bind {
                        host,
                        port,
                        bind_dn,
                        password,
                    },
                    span,
                )?;
                self.ldap_scalar_resume(dst, v);
                Ok(())
            }
            "toto_rt_ldap_search" => {
                self.arity(name, args.len(), 7, span)?;
                let host = self.net_str(args, 0, name, span)?;
                let port = self.ldap_port(args, 1, name, span)?;
                let bind_dn = self.net_str(args, 2, name, span)?;
                let password = self.net_str(args, 3, name, span)?;
                let base = self.net_str(args, 4, name, span)?;
                let filter = self.net_str(args, 5, name, span)?;
                let attrs = self.net_str(args, 6, name, span)?;
                let v = self.ldap_wait(
                    LdapOp::Search {
                        host,
                        port,
                        bind_dn,
                        password,
                        base,
                        filter,
                        attrs,
                    },
                    span,
                )?;
                self.ldap_str_resume(dst, v);
                Ok(())
            }
            "toto_rt_ldap_last_error" => {
                self.arity(name, args.len(), 0, span)?;
                let e = self.ldap_take_error();
                let v = self.alloc_with(|h| h.alloc_str(&e));
                self.net_assign(dst, RtVal::Heap(v));
                Ok(())
            }
            _ => Err(self.internal("unknown ldap function", span)),
        }
    }

    /// One runtime value to owned plain pool data. Callback-typed
    /// positions never reach here (claimed in `exec_foreign`).
    fn foreign_arg(
        &self,
        v: &RtVal,
        name: &str,
        span: Span,
    ) -> Result<crate::sched::ForeignArg, ExecErr> {
        use crate::sched::ForeignArg;
        match v {
            RtVal::Int(n) => Ok(ForeignArg::Int(*n)),
            RtVal::Float(f) => Ok(ForeignArg::Float(*f)),
            RtVal::Heap(r) => {
                let st = self.shared.state.lock().expect("shared state lock");
                match st.heap.get(*r) {
                    Some(crate::heap::Payload::Str(s)) => {
                        Ok(ForeignArg::Str(s.clone().into_bytes()))
                    }
                    _ => {
                        let tn = v.type_name(&st.heap);
                        Err(self.internal(&format!("cannot pass {tn} to foreign `{name}`"), span))
                    }
                }
            }
            RtVal::None => Ok(ForeignArg::Null),
            RtVal::Ptr(p) => Ok(ForeignArg::Ptr(*p)),
            other => {
                let st = self.shared.state.lock().expect("shared state lock");
                let tn = other.type_name(&st.heap);
                Err(self.internal(&format!("cannot pass {tn} to foreign `{name}`"), span))
            }
        }
    }

    /// Invoke every fired callback with its raw pointer argument.
    /// Returns are dropped (channels carry data outward); a panicking
    /// callback fails the foreign call that drains it.
    fn drain_callbacks(&mut self, span: Span) -> Result<(), ExecErr> {
        use toto_compiler::ffi;
        for (slot, ptr) in ffi::drain_queue() {
            // Unknown slots are stale entries from an earlier program
            // in the same process (see `run_checked` hygiene); skip.
            if let Some(target) = self.callbacks.remove(&slot) {
                self.call_value(target, vec![RtVal::Ptr(ptr)], span, "callback", None)?;
            }
            ffi::release_slot(slot);
        }
        Ok(())
    }

    /// Native `len` (array-only in MIR; strings go through the builtin).
    fn builtin_len(
        &mut self,
        dst: Option<usize>,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        self.arity("len", args.len(), 1, span)?;
        let top = self.frames.len() - 1;
        let st = self.shared.state.lock().expect("shared state lock");
        // Mirror the tree-walking `len` exactly (spec: chars for
        // strings): string counts Unicode scalar values, array counts
        // elements, anything else is E0210 with the shared message.
        let v = match &self.frames[top].locals[args[0]] {
            RtVal::Heap(r) => match st.heap.get(*r) {
                Some(Payload::Str(s)) => RtVal::Int(s.chars().count() as i64),
                Some(Payload::Array(elems)) => RtVal::Int(elems.len() as i64),
                _ => {
                    let tn = self.frames[top].locals[args[0]].type_name(&st.heap);
                    drop(st);
                    return Err(self.err(
                        "E0210",
                        format!("`len` needs a string or array, got {tn}"),
                        span,
                    ));
                }
            },
            other => {
                let tn = other.type_name(&st.heap);
                drop(st);
                return Err(self.err(
                    "E0210",
                    format!("`len` needs a string or array, got {tn}"),
                    span,
                ));
            }
        };
        drop(st);
        if let Some(d) = dst {
            self.frames[top].locals[d] = v;
        }
        Ok(())
    }

    /// Native `push` (mutates the heap array in place — no conversion,
    /// no write-back; messages mirror the shared builtin exactly).
    fn builtin_push(
        &mut self,
        dst: Option<usize>,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        self.arity("push", args.len(), 2, span)?;
        let top = self.frames.len() - 1;
        let v = self.frames[top].locals[args[1]].clone();
        let mut st = self.shared.state.lock().expect("shared state lock");
        let n = match &self.frames[top].locals[args[0]] {
            RtVal::Heap(r) => match st.heap.get_mut(*r) {
                Some(Payload::Array(elems)) => {
                    elems.push(v);
                    elems.len() as i64
                }
                _ => {
                    let tn = self.frames[top].locals[args[0]].type_name(&st.heap);
                    drop(st);
                    return Err(self.err(
                        "E0210",
                        format!("`push` needs an array, got {tn}"),
                        span,
                    ));
                }
            },
            other => {
                let tn = other.type_name(&st.heap);
                drop(st);
                return Err(self.err("E0210", format!("`push` needs an array, got {tn}"), span));
            }
        };
        drop(st);
        if let Some(d) = dst {
            self.frames[top].locals[d] = RtVal::Int(n);
        }
        Ok(())
    }

    /// Native `pop` (messages mirror the shared builtin exactly).
    fn builtin_pop(
        &mut self,
        dst: Option<usize>,
        args: &[usize],
        span: Span,
    ) -> Result<(), ExecErr> {
        self.arity("pop", args.len(), 1, span)?;
        let top = self.frames.len() - 1;
        let mut st = self.shared.state.lock().expect("shared state lock");
        let v = match &self.frames[top].locals[args[0]] {
            RtVal::Heap(r) => match st.heap.get_mut(*r) {
                Some(Payload::Array(elems)) => match elems.pop() {
                    Some(v) => v,
                    None => {
                        drop(st);
                        return Err(self.err("E0210", "pop from empty array".to_string(), span));
                    }
                },
                _ => {
                    let tn = self.frames[top].locals[args[0]].type_name(&st.heap);
                    drop(st);
                    return Err(self.err("E0210", format!("`pop` needs an array, got {tn}"), span));
                }
            },
            other => {
                let tn = other.type_name(&st.heap);
                drop(st);
                return Err(self.err("E0210", format!("`pop` needs an array, got {tn}"), span));
            }
        };
        drop(st);
        if let Some(d) = dst {
            self.frames[top].locals[d] = v;
        }
        Ok(())
    }
}

impl<'a> Drop for Executor<'a> {
    /// Task teardown (normal end, panic, or scope cancellation):
    /// release any callback slots still mapped here. Fired entries for
    /// them become unknown-slot skips at the next drain; unfired
    /// threads can no longer be observed, so the slots must not leak.
    fn drop(&mut self) {
        for (slot, _) in std::mem::take(&mut self.callbacks) {
            toto_compiler::ffi::release_slot(slot);
        }
    }
}
