//! MIR interpreter (Phase 2, Stage 2): executes MIR directly.
//!
//! The tree-walking interpreter remains the semantics reference; this
//! executor must agree with it on every corpus program (differential
//! harness). All value semantics come from `crate::ops`, shared with the
//! tree-walker, so the only MIR-specific logic is control flow (blocks,
//! branches, calls) and environment handling.
//!
//! Panic codes, messages, spans, trace shape, exit-code handling, and the
//! 8,192-frame limit mirror the tree-walker exactly.

use std::cell::RefCell;
use std::rc::Rc;

use crate::diag::Diag;
use crate::hir::{CheckedProgram, Ty};
use crate::interp::{ClosureCode, ClosureVal, Interp, MAX_FRAMES, StructVal, TraceFrame, Value};
use crate::mir::{BlockId, MBinOp, MBlock, MInstr, MProgram, MTerm};
use crate::ops::{FnRetKind, Propagate};
use crate::span::Span;

type MResult<T> = Result<T, (Diag, Vec<TraceFrame>)>;

/// Control flow out of a block sequence.
enum Flow {
    Continue,
    Return(Value),
    /// Fell off the end of a body; the caller applies the fall-off rule.
    FallOff,
}

pub struct MirInterp<'a> {
    prog: &'a MProgram,
    /// Inner tree-walking interpreter, used ONLY for builtin dispatch
    /// (output capture and `<fn name>` display). It never runs user code.
    inner: Interp,
    /// Checked foreign signatures, consulted for `CallForeign`.
    externs: std::collections::HashMap<String, crate::hir::HExternFn>,
    /// Fired-callback slots awaiting drain (mirrors `Interp`).
    callbacks: std::collections::HashMap<u64, Value>,
    call_depth: usize,
    trace: Vec<TraceFrame>,
}

impl<'a> MirInterp<'a> {
    pub fn new(
        ast: &'a crate::ast::Program,
        prog: &'a MProgram,
        externs: std::collections::HashMap<String, crate::hir::HExternFn>,
    ) -> Self {
        Self {
            prog,
            inner: Interp::new(ast, externs.clone()),
            externs,
            callbacks: std::collections::HashMap::new(),
            call_depth: 0,
            trace: Vec::new(),
        }
    }

    pub fn output(&self) -> &[u8] {
        &self.inner.output
    }

    pub fn run(&mut self) -> Result<i32, (Diag, Vec<TraceFrame>)> {
        let main = match self.prog.fn_index.get("main") {
            Some(&i) => i,
            None => {
                let span = Span::new(0, 0);
                return Err((
                    Diag::new(
                        "E0204",
                        "program has no `fn main() -> int`".to_string(),
                        span,
                        None,
                    ),
                    Vec::new(),
                ));
            }
        };
        // The checker guarantees `main() -> int`; the shape below mirrors
        // the tree-walker (which reports E0204 on unchecked programs).
        let name_span = self.prog.main_span;
        let value = self.call_fn(main, Vec::new(), name_span)?;
        match value {
            Value::Int(n) if (0..=255).contains(&n) => Ok(n as i32),
            Value::Int(n) => Err((
                Diag::new(
                    "E0204",
                    format!("main returned {n}, which is not a valid exit code 0-255"),
                    name_span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
            other => Err((
                Diag::new(
                    "E0204",
                    format!("main must return an int, not {}", other.type_name()),
                    name_span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
        }
    }

    fn call_fn(&mut self, idx: usize, args: Vec<Value>, span: Span) -> MResult<Value> {
        let func = &self.prog.fns[idx];
        if args.len() != func.params.len() {
            return Err((
                Diag::new(
                    "E0204",
                    format!(
                        "wrong number of arguments: `{}` takes {}, got {}",
                        func.name,
                        func.params.len(),
                        args.len()
                    ),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            ));
        }
        self.call_depth += 1;
        if self.call_depth > MAX_FRAMES {
            self.call_depth -= 1;
            return Err((
                Diag::new(
                    "E0211",
                    "stack overflow: Toto call stack exceeded 8,192 frames (interpreter limit)"
                        .to_string(),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            ));
        }
        self.trace.push(TraceFrame {
            name: func.name.clone(),
            span,
        });
        let mut slots: Vec<Value> = vec![Value::Unit; func.locals.len()];
        for (i, arg) in args.into_iter().enumerate() {
            slots[i] = arg;
        }
        let ret_kind = fn_ret_kind(&func.ret);
        let result = self.exec_blocks(&func.blocks, func.entry, &mut slots, ret_kind);
        self.call_depth -= 1;
        match result {
            Ok(Flow::Continue) => {
                // Unreachable: every block ends in a terminator, and the
                // builder closes bodies with `Return` or `FallOff`.
                debug_assert!(false, "function `{}` fell through its blocks", func.name);
                self.trace.pop();
                Ok(Value::Unit)
            }
            Ok(Flow::FallOff) => {
                // The fall-off rule mirrors the tree-walker: the builder
                // emits `FallOff` only for bodies with no tail, and a
                // non-unit function that falls off is E0212.
                if func.ret == Ty::Unit {
                    self.trace.pop();
                    Ok(Value::Unit)
                } else {
                    Err((
                        Diag::new(
                            "E0212",
                            format!("function `{}` exited without returning a value", func.name),
                            func.body_span,
                            None,
                        ),
                        std::mem::take(&mut self.trace),
                    ))
                }
            }
            Ok(Flow::Return(v)) => {
                self.trace.pop();
                if func.ret == Ty::Unit {
                    Ok(Value::Unit)
                } else {
                    Ok(v)
                }
            }
            Err((diag, carried)) => {
                let mut trace = std::mem::take(&mut self.trace);
                trace.extend(carried);
                Err((diag, trace))
            }
        }
    }

    fn call_closure(
        &mut self,
        closure: &ClosureVal,
        args: Vec<Value>,
        span: Span,
        display: &str,
    ) -> MResult<Value> {
        let ClosureCode::Mir { params, ret, body } = &closure.code else {
            return Err((
                Diag::new(
                    "E0204",
                    "internal error: AST closure called by MIR interpreter".to_string(),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            ));
        };
        if args.len() != params.len() {
            return Err((
                Diag::new(
                    "E0204",
                    format!(
                        "wrong number of arguments: closure takes {}, got {}",
                        params.len(),
                        args.len()
                    ),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            ));
        }
        self.call_depth += 1;
        if self.call_depth > MAX_FRAMES {
            self.call_depth -= 1;
            return Err((
                Diag::new(
                    "E0211",
                    "stack overflow: Toto call stack exceeded 8,192 frames (interpreter limit)"
                        .to_string(),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            ));
        }
        self.trace.push(TraceFrame {
            name: display.to_string(),
            span,
        });
        let mut slots: Vec<Value> = vec![Value::Unit; body.locals.len()];
        for (i, arg) in args.into_iter().enumerate() {
            slots[i] = arg;
        }
        for (i, (_, v)) in closure.captured.iter().enumerate() {
            slots[params.len() + i] = v.clone();
        }
        let ret_kind = fn_ret_kind(ret);
        let result = self.exec_blocks(&body.blocks, body.entry, &mut slots, ret_kind);
        self.call_depth -= 1;
        match result {
            Ok(Flow::Continue) => {
                debug_assert!(false, "closure fell through its blocks");
                self.trace.pop();
                Ok(Value::Unit)
            }
            Ok(Flow::FallOff) => {
                if *ret == Ty::Unit {
                    self.trace.pop();
                    Ok(Value::Unit)
                } else {
                    Err((
                        Diag::new(
                            "E0212",
                            "closure exited without returning a value".to_string(),
                            body.body_span,
                            None,
                        ),
                        std::mem::take(&mut self.trace),
                    ))
                }
            }
            Ok(Flow::Return(v)) => {
                self.trace.pop();
                if *ret == Ty::Unit {
                    Ok(Value::Unit)
                } else {
                    Ok(v)
                }
            }
            Err((diag, carried)) => {
                let mut trace = std::mem::take(&mut self.trace);
                trace.extend(carried);
                Err((diag, trace))
            }
        }
    }

    fn exec_blocks(
        &mut self,
        blocks: &[MBlock],
        entry: BlockId,
        slots: &mut [Value],
        ret_kind: FnRetKind,
    ) -> Result<Flow, (Diag, Vec<TraceFrame>)> {
        let mut current = entry;
        loop {
            let Some(block) = blocks.iter().find(|b| b.id == current) else {
                return Err((
                    Diag::new(
                        "E0204",
                        "internal error: MIR jumps to a missing block".to_string(),
                        Span::new(0, 0),
                        None,
                    ),
                    Vec::new(),
                ));
            };
            for instr in &block.instrs {
                match self.exec_instr(instr, slots, ret_kind)? {
                    Flow::Continue => {}
                    flow @ (Flow::Return(_) | Flow::FallOff) => return Ok(flow),
                }
            }
            match &block.term {
                MTerm::Return(o) => {
                    let v = o.map(|l| slots[l].clone()).unwrap_or(Value::Unit);
                    return Ok(Flow::Return(v));
                }
                MTerm::FallOff { .. } => {
                    return Ok(Flow::FallOff);
                }
                MTerm::Jump(id) => current = *id,
                MTerm::Branch {
                    cond,
                    then_b,
                    else_b,
                    span,
                    while_cond,
                } => match &slots[*cond] {
                    Value::Bool(true) => current = *then_b,
                    Value::Bool(false) => current = *else_b,
                    other => {
                        let what = if *while_cond { "while" } else { "if" };
                        return Err((
                            Diag::new(
                                "E0207",
                                format!("{what} condition must be bool, got {}", other.type_name()),
                                *span,
                                None,
                            ),
                            Vec::new(),
                        ));
                    }
                },
            }
        }
    }

    fn exec_instr(
        &mut self,
        instr: &MInstr,
        slots: &mut [Value],
        ret_kind: FnRetKind,
    ) -> MResult<Flow> {
        match instr {
            MInstr::Const { dst, val } => {
                slots[*dst] = match val {
                    crate::mir::MConst::Int(v) => Value::Int(*v),
                    crate::mir::MConst::Float(v) => Value::Float(*v),
                    crate::mir::MConst::Str(s) => Value::Str(Rc::new(s.clone())),
                    crate::mir::MConst::Bool(b) => Value::Bool(*b),
                    crate::mir::MConst::Unit => Value::Unit,
                    crate::mir::MConst::None => Value::None,
                };
            }
            MInstr::Copy { dst, src } => {
                let v = slots[*src].clone();
                slots[*dst] = v;
            }
            MInstr::Bin {
                dst,
                op,
                l,
                r,
                span,
            } => {
                let lv = slots[*l].clone();
                let rv = slots[*r].clone();
                let v = match op {
                    MBinOp::Add => {
                        crate::ops::arith(lv, rv, *span, "add", |a, b| a.checked_add(b))?
                    }
                    MBinOp::Sub => {
                        crate::ops::arith(lv, rv, *span, "subtract", |a, b| a.checked_sub(b))?
                    }
                    MBinOp::Mul => {
                        crate::ops::arith(lv, rv, *span, "multiply", |a, b| a.checked_mul(b))?
                    }
                    MBinOp::Div => crate::ops::div_values(lv, rv, *span)?,
                    MBinOp::Mod => crate::ops::mod_values(lv, rv, *span)?,
                    MBinOp::Eq => crate::ops::equality(lv, rv, *span, true)?,
                    MBinOp::NotEq => crate::ops::equality(lv, rv, *span, false)?,
                    MBinOp::Lt | MBinOp::LtEq | MBinOp::Gt | MBinOp::GtEq => {
                        let aop = match op {
                            MBinOp::Lt => crate::ast::BinOp::Lt,
                            MBinOp::LtEq => crate::ast::BinOp::LtEq,
                            MBinOp::Gt => crate::ast::BinOp::Gt,
                            _ => crate::ast::BinOp::GtEq,
                        };
                        crate::ops::ordering(aop, lv, rv, *span)?
                    }
                };
                slots[*dst] = v;
            }
            MInstr::Neg { dst, operand, span } => {
                let v = slots[*operand].clone();
                slots[*dst] = crate::ops::neg_value(v, *span, *span)?;
            }
            MInstr::Not { dst, operand, span } => {
                let v = slots[*operand].clone();
                slots[*dst] = crate::ops::not_value(v, *span)?;
            }
            MInstr::DivCheck { value, span } => match &slots[*value] {
                Value::Int(0) => {
                    return Err((
                        Diag::new("E0202", "division by zero".to_string(), *span, None),
                        Vec::new(),
                    ));
                }
                Value::Int(_) => {}
                other => {
                    debug_assert!(false, "divcheck on non-int: {}", other.type_name());
                }
            },
            MInstr::Call {
                dst,
                func,
                args,
                span,
                display,
            } => {
                let fv = slots[*func].clone();
                let mut vs = Vec::with_capacity(args.len());
                for a in args {
                    vs.push(slots[*a].clone());
                }
                // `call_fn`/`call_closure` push exactly one frame each,
                // mirroring the tree-walker.
                let v = self.call_value(&fv, vs, *span, display)?;
                if let Some(d) = dst {
                    slots[*d] = v;
                }
            }
            MInstr::CallBuiltin {
                dst,
                name,
                args,
                span,
            } => {
                let mut vs = Vec::with_capacity(args.len());
                for a in args {
                    vs.push(slots[*a].clone());
                }
                self.trace.push(TraceFrame {
                    name: format!("{name} (builtin)"),
                    span: *span,
                });
                let result = self.inner.dispatch_builtin(name, vs, *span);
                match result {
                    Ok(v) => {
                        self.trace.pop();
                        if let Some(d) = dst {
                            slots[*d] = v;
                        }
                    }
                    Err((diag, carried)) => {
                        let mut trace = std::mem::take(&mut self.trace);
                        trace.extend(carried);
                        return Err((diag, trace));
                    }
                }
            }
            MInstr::CallForeign {
                dst,
                name,
                args,
                span,
            } => {
                let Some(sig) = self.externs.get(name).cloned() else {
                    // Unreachable: the checker resolves every foreign
                    // call against the same table.
                    return Err((
                        Diag::new(
                            "E0204",
                            format!("unknown foreign function `{name}`"),
                            *span,
                            None,
                        ),
                        std::mem::take(&mut self.trace),
                    ));
                };
                let mut vs = Vec::with_capacity(args.len());
                for a in args {
                    vs.push(slots[*a].clone());
                }
                self.trace.push(TraceFrame {
                    name: format!("{name} (foreign)"),
                    span: *span,
                });
                let (value, claimed) = match crate::ffi::call(name, &sig, &vs, *span) {
                    Ok((v, claimed)) => {
                        self.trace.pop();
                        (v, claimed)
                    }
                    Err((diag, carried)) => {
                        let mut trace = std::mem::take(&mut self.trace);
                        trace.extend(carried);
                        return Err((diag, trace));
                    }
                };
                for (arg_idx, slot) in claimed {
                    if let Some(v) = vs.get(arg_idx) {
                        self.callbacks.insert(slot, v.clone());
                    }
                }
                self.drain_callbacks(*span)?;
                if let Some(d) = dst {
                    slots[*d] = value;
                }
            }
            MInstr::FuncRef { dst, idx } => {
                slots[*dst] = Value::Func(*idx);
            }
            MInstr::StructLit {
                dst, name, fields, ..
            } => {
                let mut vs = Vec::with_capacity(fields.len());
                for (n, l) in fields {
                    vs.push((n.clone(), slots[*l].clone()));
                }
                slots[*dst] = Value::Struct(Rc::new(RefCell::new(StructVal {
                    name: name.clone(),
                    fields: vs,
                })));
            }
            MInstr::ArrayLit { dst, elems, .. } => {
                let vs: Vec<Value> = elems.iter().map(|e| slots[*e].clone()).collect();
                slots[*dst] = Value::Array(Rc::new(RefCell::new(vs)));
            }
            MInstr::Snapshot { dst, base, .. } => {
                let vs = match &slots[*base] {
                    Value::Array(cell) => cell.borrow().clone(),
                    other => {
                        debug_assert!(false, "snapshot of non-array: {}", other.type_name());
                        Vec::new()
                    }
                };
                slots[*dst] = Value::Array(Rc::new(RefCell::new(vs)));
            }
            MInstr::RangeLit { dst, lo, hi, .. } => {
                let (l, h) = match (&slots[*lo], &slots[*hi]) {
                    (Value::Int(a), Value::Int(b)) => (*a, *b),
                    _ => {
                        debug_assert!(false, "range of non-ints");
                        (0, 0)
                    }
                };
                slots[*dst] = Value::Range(l, h);
            }
            MInstr::SplitRange { lo, hi, range, .. } => {
                let (a, b) = match &slots[*range] {
                    Value::Range(a, b) => (*a, *b),
                    other => {
                        debug_assert!(false, "split of non-range: {}", other.type_name());
                        (0, 0)
                    }
                };
                slots[*lo] = Value::Int(a);
                slots[*hi] = Value::Int(b);
            }
            MInstr::WrapOk { dst, inner } => {
                let v = slots[*inner].clone();
                slots[*dst] = Value::Ok(Box::new(v));
            }
            MInstr::WrapErr { dst, inner } => {
                let v = slots[*inner].clone();
                slots[*dst] = Value::Err(Box::new(v));
            }
            MInstr::WrapSome { dst, inner } => {
                let v = slots[*inner].clone();
                slots[*dst] = Value::Some(Box::new(v));
            }
            MInstr::Propagate { dst, operand, span } => {
                let v = slots[*operand].clone();
                match crate::ops::propagate(v, ret_kind, *span) {
                    Ok(unwrapped) => slots[*dst] = unwrapped,
                    Err(Propagate::Return(e)) => return Ok(Flow::Return(e)),
                    Err(Propagate::Bug(span)) => {
                        return Err((
                            Diag::new(
                                "E0204",
                                "`?` outside a matching Result/Option function (unreachable in checked programs)".to_string(),
                                span,
                                None,
                            ),
                            Vec::new(),
                        ));
                    }
                }
            }
            MInstr::Field {
                dst,
                base,
                name,
                span,
            } => {
                let b = slots[*base].clone();
                slots[*dst] = crate::ops::field_read(b, name, *span)?;
            }
            MInstr::Index {
                dst,
                base,
                index,
                span,
                index_span,
            } => {
                let b = slots[*base].clone();
                let i = slots[*index].clone();
                slots[*dst] = crate::ops::index_read(b, i, *span, *index_span)?;
            }
            MInstr::StoreField {
                base,
                name,
                value,
                span,
            } => {
                let b = slots[*base].clone();
                let v = slots[*value].clone();
                crate::ops::field_write(b, name, v, *span)?;
            }
            MInstr::StoreIndex {
                base,
                index,
                value,
                span,
                index_span,
            } => {
                let b = slots[*base].clone();
                let i = slots[*index].clone();
                let v = slots[*value].clone();
                crate::ops::index_write(b, i, v, *span, *index_span)?;
            }
            MInstr::Len { dst, base, span } => {
                let v = match &slots[*base] {
                    Value::Array(cell) => Value::Int(cell.borrow().len() as i64),
                    other => {
                        return Err((
                            Diag::new(
                                "E0209",
                                format!("`len` needs an array, got {}", other.type_name()),
                                *span,
                                None,
                            ),
                            Vec::new(),
                        ));
                    }
                };
                slots[*dst] = v;
            }
            MInstr::SatIncr { dst, operand } => match &slots[*operand] {
                Value::Int(i) => slots[*dst] = Value::Int(i.saturating_add(1)),
                other => {
                    debug_assert!(false, "satincr of non-int: {}", other.type_name());
                }
            },
            MInstr::MakeClosure {
                dst,
                body,
                captures,
                ..
            } => {
                let mut env = Vec::with_capacity(captures.len());
                for (i, src) in captures.iter().enumerate() {
                    let (name, _) = &body.captures[i];
                    env.push((name.clone(), slots[*src].clone()));
                }
                let code = ClosureCode::Mir {
                    params: body.params.clone(),
                    ret: body.ret.clone(),
                    body: body.clone(),
                };
                slots[*dst] = Value::Closure(Rc::new(ClosureVal {
                    code,
                    captured: env,
                }));
            }
            MInstr::Safepoint { .. } => {
                // No-op in the MIR interpreter: single-threaded, no GC in
                // Phase 2. Phase 3 codegen uses these markers.
            }
            // Routing tripwire: the sequential MIR interpreter cannot
            // execute channels. Concurrent programs run on the runtime
            // executor; reaching one of these here is a routing bug.
            MInstr::ChanNew { span, .. }
            | MInstr::Send { span, .. }
            | MInstr::Recv { span, .. }
            | MInstr::Spawn { span, .. }
            | MInstr::Select { span, .. } => {
                return Err((
                    Diag::new(
                        "E0215",
                        "concurrent construct reached the sequential interpreter (internal routing error)".to_string(),
                        *span,
                        Some("concurrent programs run on the runtime executor".to_string()),
                    ),
                    Vec::new(),
                ));
            }
        }
        Ok(Flow::Continue)
    }

    /// Invoke every fired callback with its raw pointer argument.
    /// Returns are dropped (channels carry data outward); a panicking
    /// callback fails the foreign call that drains it.
    fn drain_callbacks(&mut self, span: Span) -> MResult<()> {
        for (slot, ptr) in crate::ffi::drain_queue() {
            // Unknown slots are stale entries from an earlier program
            // in the same process (see `Interp::run`); skip silently.
            if let Some(target) = self.callbacks.remove(&slot) {
                self.call_value(&target, vec![Value::Ptr(ptr)], span, "callback")?;
            }
            crate::ffi::release_slot(slot);
        }
        Ok(())
    }

    fn call_value(
        &mut self,
        target: &Value,
        args: Vec<Value>,
        span: Span,
        display: &str,
    ) -> MResult<Value> {
        match target {
            Value::Func(idx) => self.call_fn(*idx, args, span),
            Value::Closure(c) => {
                let c = c.clone();
                let name = display.to_string();
                self.call_closure(&c, args, span, &name)
            }
            other => Err((
                Diag::new(
                    "E0204",
                    format!("call of non-function value ({})", other.type_name()),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
        }
    }
}

fn fn_ret_kind(ret: &Ty) -> FnRetKind {
    match ret {
        Ty::Result(_, _) => FnRetKind::Result,
        Ty::Option(_) => FnRetKind::Option,
        _ => FnRetKind::Other,
    }
}

/// Outcome of running a checked program through the MIR interpreter.
pub type MirOutcome = (Result<i32, (Diag, Vec<TraceFrame>)>, Vec<u8>);

/// Lower a checked program to MIR, then run it through the MIR
/// interpreter: returns the exit code (or panic diagnostic with trace) and
/// the captured stdout. Runs on a dedicated large-stack thread like the
/// tree-walker, so deep recursion behaves the same in both.
pub fn run_mir(ast: &crate::ast::Program, checked: &CheckedProgram) -> MirOutcome {
    let prog = crate::mir::lower(checked);
    let ast = ast.clone();
    let externs = checked.externs.clone();
    std::thread::Builder::new()
        .name("toto-mir-interp".to_string())
        .stack_size(256 << 20)
        .spawn(move || {
            let mut interp = MirInterp::new(&ast, &prog, externs);
            let result = interp.run();
            let output = interp.output().to_vec();
            (result, output)
        })
        .expect("failed to spawn MIR interpreter thread")
        .join()
        .expect("MIR interpreter thread failed")
}
