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
        }
    }

    pub fn run_task(mut self, initial: Initial, call_span: Span) -> TaskResult {
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
        self.maybe_collect();
        match self.y.suspend(req) {
            ToTask::Cancelled { op } => Err(self.cancelled(op, span)),
            resp => Ok(resp),
        }
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
            _ => {
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
    fn maybe_collect(&self) {
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
                        (RtVal::Range(a, b), RtVal::Int(i)) => {
                            let len = b.saturating_sub(*a);
                            if *i >= 0 && *i < len {
                                Some(RtVal::Int(a + i))
                            } else {
                                None
                            }
                        }
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
        // memo so shared arguments stay shared).
        let owned: Vec<RtVal> = args
            .iter()
            .map(|a| self.frames[top].locals[*a].clone())
            .collect();
        let vs = self.to_interps(&owned, span)?;
        self.trace.push(TraceFrame {
            name: format!("{name} (builtin)"),
            span,
        });
        let out = {
            let mut output = self.shared.output.lock().expect("output lock");
            builtins::call_builtin(name, vs, span, &self.code.fns, &mut output)
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
