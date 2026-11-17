//! MIR: explicit control-flow-graph IR (Phase 2, Stage 2).
//!
//! HIR preserves language-level shapes; MIR lowers them to basic blocks
//! with explicit operations so Phase 3 codegen can translate mechanically:
//!
//! - Checked arithmetic is explicit ops (`Bin` with `MBinOp::Add/Sub/Mul`
//!   on ints traps `E0201` on overflow, exactly like the interpreter).
//! - Integer division/modulo is split into an explicit `DivCheck`
//!   (`E0202` on zero) followed by a total `Bin` — the trap decision
//!   (explicit checks, ADR-008) is visible in the IR.
//! - `&&`/`||`, `if`, `while`, `for`, and `?` are explicit branches.
//! - Allocation points carry `Safepoint` markers (`Entry`, `Loop`, `Call`,
//!   `Alloc`) so Phase 3 codegen can auto-generate GC root publication
//!   (Spike 3 finding). The MIR interpreter treats them as no-ops.
//! - Closures are closure-converted: each literal becomes a nested
//!   `MirBody` with an explicit capture list; `MakeClosure` bundles the
//!   code with the captured values.
//!
//! Function indices match source order, the same order the tree-walking
//! interpreter numbers its table — valid because the checker rejects
//! duplicate definitions (`E0311`), the one case where the interpreter's
//! last-wins table could disagree.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::hir::{
    CheckedProgram, HAssignTarget, HBlock, HExpr, HExprKind, HFn, HStmtKind, Ty, VarRef,
};
use crate::span::Span;

pub type BlockId = usize;
pub type Local = usize;

/// A whole MIR program: top-level functions plus struct declarations.
#[derive(Clone, Debug, PartialEq)]
pub struct MProgram {
    pub fns: Vec<MFn>,
    pub fn_index: HashMap<String, usize>,
    /// Name span of `main` for exit-code diagnostics (tree-walker parity).
    pub main_span: Span,
    /// Checked foreign signatures, kept out of the text dump (which
    /// prints functions only): the JIT/AOT backend needs them to
    /// marshal `CallForeign` arguments and refuse callback params.
    pub externs: HashMap<String, crate::hir::HExternFn>,
    /// Lowered migration bodies, in `CheckedProgram.migrates` order.
    /// Never called during single-generation execution; the reload
    /// protocol invokes them (spec amendment, Phase 6).
    pub migrates: Vec<MFn>,
}

/// One function body: locals plus a block graph.
#[derive(Clone, Debug, PartialEq)]
pub struct MFn {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    /// All locals by id; parameters are locals `0..params.len()`.
    pub locals: Vec<(String, Ty)>,
    pub blocks: Vec<MBlock>,
    pub entry: BlockId,
    /// Source span of the function body (fall-off `E0212` anchor).
    pub body_span: Span,
}

/// A closure body: parameters, explicit captures, then locals and blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct MirBody {
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    pub captures: Vec<(String, Ty)>,
    /// Locals by id: params first, then capture slots, then temporaries.
    pub locals: Vec<(String, Ty)>,
    pub blocks: Vec<MBlock>,
    pub entry: BlockId,
    pub body_span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MBlock {
    pub id: BlockId,
    pub instrs: Vec<MInstr>,
    pub term: MTerm,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MInstr {
    Const {
        dst: Local,
        val: MConst,
    },
    Copy {
        dst: Local,
        src: Local,
    },
    Bin {
        dst: Local,
        op: MBinOp,
        l: Local,
        r: Local,
        span: Span,
    },
    Neg {
        dst: Local,
        operand: Local,
        span: Span,
    },
    Not {
        dst: Local,
        operand: Local,
        span: Span,
    },
    /// Explicit zero check for integer `/` and `%` (trap decision).
    DivCheck {
        value: Local,
        span: Span,
    },
    Call {
        dst: Option<Local>,
        func: Local,
        args: Vec<Local>,
        span: Span,
        /// Source-level callee name for traces (`<value>` if computed).
        display: String,
    },
    CallBuiltin {
        dst: Option<Local>,
        name: String,
        args: Vec<Local>,
        span: Span,
    },
    /// Direct call to a declared foreign function (executed by the
    /// interpreter inline and by the runtime executor on the blocking
    /// pool; the JIT refuses these programs cleanly).
    CallForeign {
        dst: Option<Local>,
        name: String,
        args: Vec<Local>,
        span: Span,
    },
    /// Materialize a top-level function as a value.
    FuncRef {
        dst: Local,
        idx: usize,
    },
    StructLit {
        dst: Local,
        name: String,
        fields: Vec<(String, Local)>,
        span: Span,
    },
    ArrayLit {
        dst: Local,
        elems: Vec<Local>,
        span: Span,
    },
    /// Clone an array's elements into a fresh array (for-loop snapshot).
    Snapshot {
        dst: Local,
        base: Local,
        span: Span,
    },
    RangeLit {
        dst: Local,
        lo: Local,
        hi: Local,
        span: Span,
    },
    /// Split a range value into bound locals (non-literal range iteration).
    SplitRange {
        lo: Local,
        hi: Local,
        range: Local,
        span: Span,
    },
    WrapOk {
        dst: Local,
        inner: Local,
    },
    WrapErr {
        dst: Local,
        inner: Local,
    },
    WrapSome {
        dst: Local,
        inner: Local,
    },
    /// `?`: unwrap `Ok`/`Some`, or early-return `Err`/`None`.
    Propagate {
        dst: Local,
        operand: Local,
        span: Span,
    },
    Field {
        dst: Local,
        base: Local,
        name: String,
        span: Span,
    },
    Index {
        dst: Local,
        base: Local,
        index: Local,
        span: Span,
        index_span: Span,
    },
    StoreField {
        base: Local,
        name: String,
        value: Local,
        span: Span,
    },
    StoreIndex {
        base: Local,
        index: Local,
        value: Local,
        span: Span,
        index_span: Span,
    },
    Len {
        dst: Local,
        base: Local,
        span: Span,
    },
    /// `i.saturating_add(1)` for range loops (mirrors the interpreter).
    SatIncr {
        dst: Local,
        operand: Local,
    },
    MakeClosure {
        dst: Local,
        body: Arc<MirBody>,
        /// Enclosing locals feeding the body's capture slots, in order.
        captures: Vec<Local>,
        span: Span,
    },
    /// Create a channel. `elem` is the element type `T`; `cap` is `None`
    /// for unbuffered (`chan<T>()`) or a local holding the capacity
    /// (`chan<T>(n)`).
    ChanNew {
        dst: Local,
        elem: Ty,
        cap: Option<Local>,
        span: Span,
    },
    /// Rendezvous send. May park the task; always a safepoint.
    Send {
        chan: Local,
        value: Local,
        span: Span,
    },
    /// Blocking receive. May park the task; always a safepoint.
    Recv {
        dst: Local,
        chan: Local,
        span: Span,
    },
    /// Spawn a task running `body` (a parameterless unit body, lowered
    /// like a closure) with `captures` as its capture values.
    Spawn {
        body: Arc<MirBody>,
        /// Enclosing locals feeding the body's capture slots, in order.
        captures: Vec<Local>,
        span: Span,
    },
    /// Multi-way rendezvous. The executor evaluates readiness, performs
    /// exactly one transfer, and writes the chosen case index into
    /// `dst` (`cases.len()` for `default`). Recv cases write the
    /// received value into `bind` when present. Always a safepoint.
    Select {
        dst: Local,
        cases: Vec<MSelectCase>,
        has_default: bool,
        span: Span,
    },
    /// No runtime effect; marks GC safepoints for Phase 3 codegen.
    Safepoint {
        kind: SpKind,
    },
    /// Read declared-state slot `slot` (spec amendment, Phase 6).
    /// Slots are declaration order across the program's one state
    /// block; every executor owns a same-ordered cell vector.
    StateGet {
        dst: Local,
        slot: u32,
        span: Span,
    },
    /// Write declared-state slot `slot`. Revisions are atomic at
    /// the op level; cross-op atomicity is the reload protocol's job.
    StateSet {
        slot: u32,
        src: Local,
        span: Span,
    },
}

/// One `select` guard, lowered: channel/value locals plus the bind slot
/// for receive-and-bind cases.
#[derive(Clone, Debug, PartialEq)]
pub enum MSelectCase {
    Recv {
        chan: Local,
        bind: Option<Local>,
        span: Span,
    },
    Send {
        chan: Local,
        value: Local,
        span: Span,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum MTerm {
    Return(Option<Local>),
    /// Fell off the end: `E0212` unless the function returns `unit`.
    FallOff {
        span: Span,
    },
    Jump(BlockId),
    Branch {
        cond: Local,
        then_b: BlockId,
        else_b: BlockId,
        span: Span,
        /// `while` conditions panic with the `while` message (`E0207`).
        while_cond: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpKind {
    Entry,
    Loop,
    Call,
    Alloc,
    /// A suspension point: send, receive, select, or spawn. Live
    /// references are published to the shadow stack before parking.
    Park,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MConst {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Unit,
    None,
}

/// Lower checked HIR to MIR. The input passed the checker, so name
/// resolution, types, and arities hold; the builder still never panics —
/// any violated invariant becomes a well-formed `FallOff` in dead code.
pub fn lower(checked: &CheckedProgram) -> MProgram {
    lower_with_init(checked, true)
}

/// Lower without the state-init prepend in `main` (Phase 6,
/// Stage 3): the preserving-reload path seeds every slot from
/// carried values instead of running initializers. Everything else
/// (safepoints, checks, migrates) lowers identically.
pub fn lower_without_init(checked: &CheckedProgram) -> MProgram {
    lower_with_init(checked, false)
}

fn lower_with_init(checked: &CheckedProgram, with_init: bool) -> MProgram {
    let mut fns = Vec::new();
    let mut fn_index = HashMap::new();
    let mut main_span = Span::new(0, 0);
    let mut saw_main = false;
    for (idx, sig) in checked.fns.iter().enumerate() {
        fn_index.insert(sig.name.clone(), idx);
        let hfn = checked
            .program
            .items
            .iter()
            .find_map(|item| match &item.kind {
                crate::hir::HItemKind::Fn(f) if f.name == sig.name => Some(f),
                _ => None,
            })
            .expect("checked function has an HIR body");
        if sig.name == "main" {
            main_span = hfn.span;
            saw_main = true;
        }
        fns.push(lower_fn_inner(checked, hfn, with_init));
    }
    // State without `main` lowers nothing: `main` is mandatory
    // (E0312), so a checked program with state always has it.
    debug_assert!(checked.state.is_empty() || saw_main);
    let mut migrates = Vec::new();
    for m in &checked.migrates {
        let sty = Ty::Struct(m.target.clone());
        migrates.push(lower_fn_inner(
            checked,
            &crate::hir::HFn {
                name: format!("migrate {}", m.target),
                params: vec![(m.param.clone(), sty.clone(), false)],
                ret: sty,
                body: m.body.clone(),
                span: m.span,
            },
            with_init,
        ));
    }
    MProgram {
        fns,
        fn_index,
        main_span,
        externs: checked.externs.clone(),
        migrates,
    }
}

/// Lower one function with optional state-init prepend (Phase 6,
/// Stage 3): the preserving-reload compile uses `false` for `main`,
/// since carried values seed every slot and re-running initializers
/// would clobber them (and repeat IO side effects).
fn lower_fn_inner(checked: &CheckedProgram, hfn: &HFn, with_init: bool) -> MFn {
    let mut b = FnBuilder::new(checked);
    for (name, ty, _) in &hfn.params {
        let l = b.fresh(format!("{name} (param)"), ty.clone());
        b.scopes
            .last_mut()
            .expect("scope underflow")
            .insert(name.clone(), l);
    }
    // Declared-state initialization (spec amendment, Phase 6): the
    // entry block of `main` evaluates initializers in declaration
    // order before any user code. Channel bindings construct their
    // (always unbuffered) channel. Earlier bindings are visible to
    // later initializers through ordinary `StateGet` lowering. The
    // entry safepoint stays first: init code allocates and carries
    // the same per-op guards as ordinary code.
    b.entry_safepoint();
    if with_init && hfn.name == "main" {
        b.lower_state_init();
    }
    b.lower_stmts(&hfn.body.stmts, &hfn.ret);
    if !b.sealed {
        match &hfn.body.tail {
            Some(t) => {
                let v = b.lower_expr(t, &hfn.ret);
                b.close(MTerm::Return(Some(v)));
            }
            None => {
                if hfn.ret == Ty::Unit {
                    let u = b.materialize_unit();
                    b.close(MTerm::Return(Some(u)));
                } else {
                    b.close(MTerm::FallOff {
                        span: hfn.body.span,
                    });
                }
            }
        }
    }
    b.finish_fn(hfn)
}

struct FnBuilder<'a> {
    checked: &'a CheckedProgram,
    locals: Vec<(String, Ty)>,
    blocks: Vec<MBlock>,
    next_id: BlockId,
    cur_id: BlockId,
    cur: Vec<MInstr>,
    /// True once the current block has its terminator.
    sealed: bool,
    scopes: Vec<HashMap<String, Local>>,
    loop_stack: Vec<(BlockId, BlockId)>,
    /// Declared-state name → slot (declaration order). Slots are the
    /// cross-executor contract; every lowering and executor agrees.
    state_slots: HashMap<String, u32>,
}

impl<'a> FnBuilder<'a> {
    fn new(checked: &'a CheckedProgram) -> Self {
        let mut state_slots = HashMap::new();
        for (slot, s) in checked.state.iter().enumerate() {
            state_slots.insert(s.name.clone(), slot as u32);
        }
        Self {
            checked,
            locals: Vec::new(),
            blocks: Vec::new(),
            next_id: 1,
            cur_id: 0,
            cur: Vec::new(),
            sealed: false,
            scopes: vec![HashMap::new()],
            loop_stack: Vec::new(),
            state_slots,
        }
    }

    /// Lower declared-state initializers into the current (entry)
    /// block, in declaration order. Channel bindings construct their
    /// channel; data bindings evaluate their initializer. Earlier
    /// bindings resolve through ordinary `StateGet` lowering.
    fn lower_state_init(&mut self) {
        let count = self.checked.state.len();
        for slot in 0..count {
            let (name, ty, init, span) = {
                let s = &self.checked.state[slot];
                (s.name.clone(), s.ty.clone(), s.init.clone(), s.span)
            };
            match init {
                Some(h) => {
                    let v = self.lower_expr(&h, &ty);
                    self.emit(MInstr::StateSet {
                        slot: slot as u32,
                        src: v,
                        span,
                    });
                }
                None => {
                    let Ty::Chan(elem) = &ty else {
                        // Unreachable: the checker enforces bare ==
                        // channel and initialized == data.
                        debug_assert!(false, "bare state binding without channel type");
                        continue;
                    };
                    let dst = self.fresh(format!("{name} (state chan)"), ty.clone());
                    self.emit(MInstr::Safepoint {
                        kind: SpKind::Alloc,
                    });
                    self.emit(MInstr::ChanNew {
                        dst,
                        elem: (**elem).clone(),
                        cap: None,
                        span,
                    });
                    self.emit(MInstr::StateSet {
                        slot: slot as u32,
                        src: dst,
                        span,
                    });
                }
            }
        }
    }

    fn fresh(&mut self, name: String, ty: Ty) -> Local {
        let id = self.locals.len();
        self.locals.push((name, ty));
        id
    }

    fn fresh_block(&mut self) -> BlockId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn entry_safepoint(&mut self) {
        self.cur.push(MInstr::Safepoint {
            kind: SpKind::Entry,
        });
    }

    fn emit(&mut self, instr: MInstr) {
        if self.sealed {
            let id = self.fresh_block();
            self.open(id);
        }
        self.cur.push(instr);
    }

    fn open(&mut self, id: BlockId) {
        debug_assert!(self.sealed, "opening a block while another is open");
        self.cur_id = id;
        self.cur = Vec::new();
        self.sealed = false;
    }

    fn close(&mut self, term: MTerm) {
        if self.sealed {
            return;
        }
        let instrs = std::mem::take(&mut self.cur);
        self.blocks.push(MBlock {
            id: self.cur_id,
            instrs,
            term,
        });
        self.sealed = true;
    }

    fn finish_fn(mut self, hfn: &HFn) -> MFn {
        self.blocks.sort_by_key(|b| b.id);
        MFn {
            name: hfn.name.clone(),
            params: hfn
                .params
                .iter()
                .map(|(n, t, _)| (n.clone(), t.clone()))
                .collect(),
            ret: hfn.ret.clone(),
            locals: self.locals,
            blocks: self.blocks,
            entry: 0,
            body_span: hfn.body.span,
        }
    }

    fn materialize_unit(&mut self) -> Local {
        let u = self.fresh("unit".to_string(), Ty::Unit);
        self.emit(MInstr::Const {
            dst: u,
            val: MConst::Unit,
        });
        u
    }

    fn const_local(&mut self, val: MConst, ty: Ty, hint: &str) -> Local {
        let dst = self.fresh(format!("const {hint}"), ty);
        self.emit(MInstr::Const { dst, val });
        dst
    }

    fn lower_stmts(&mut self, stmts: &[crate::hir::HStmt], ret: &Ty) {
        for stmt in stmts {
            self.lower_stmt(stmt, ret);
        }
    }

    fn lower_stmt(&mut self, stmt: &crate::hir::HStmt, ret: &Ty) {
        match &stmt.kind {
            HStmtKind::Let {
                name, ty, value, ..
            } => {
                let v = self.lower_expr(value, ret);
                let dst = self.fresh(name.clone(), ty.clone());
                self.emit(MInstr::Copy { dst, src: v });
                self.scopes
                    .last_mut()
                    .expect("scope underflow")
                    .insert(name.clone(), dst);
            }
            HStmtKind::Assign { target, value } => {
                let v = self.lower_expr(value, ret);
                match target {
                    HAssignTarget::Var(name, varref, span) => {
                        // Declared-state stores bypass locals (see the
                        // `StateGet` arm above for why matching the
                        // resolved form is exact).
                        if let VarRef::State { .. } = varref {
                            let slot = self.state_slots.get(name).unwrap_or_else(|| {
                                panic!("checked state binding `{name}` has no slot")
                            });
                            self.emit(MInstr::StateSet {
                                slot: *slot,
                                src: v,
                                span: *span,
                            });
                        } else {
                            let dst = self.resolve(name);
                            self.emit(MInstr::Copy { dst, src: v });
                            let _ = span;
                        }
                    }
                    HAssignTarget::Field(base, name, span) => {
                        let b = self.lower_expr(base, ret);
                        self.emit(MInstr::StoreField {
                            base: b,
                            name: name.clone(),
                            value: v,
                            span: *span,
                        });
                    }
                    HAssignTarget::Index(base, index, span) => {
                        let b = self.lower_expr(base, ret);
                        let i = self.lower_expr(index, ret);
                        let ispan = index.span;
                        // OOB reports the whole store statement (the
                        // tree-walker's rule in `ops::index_write`),
                        // not just the assign target; `index_span`
                        // stays the index expression (non-int index).
                        let _ = span;
                        self.emit(MInstr::StoreIndex {
                            base: b,
                            index: i,
                            value: v,
                            span: stmt.span,
                            index_span: ispan,
                        });
                    }
                }
            }
            HStmtKind::Expr(e) => {
                // Evaluated for effects; the value is discarded (the
                // checker already rejected unused `Result`s).
                let _ = self.lower_expr(e, ret);
            }
            HStmtKind::Return(e) => {
                let v = self.lower_expr(e, ret);
                self.close(MTerm::Return(Some(v)));
            }
            HStmtKind::Break => {
                let span = stmt.span;
                match self.loop_stack.last() {
                    Some((break_id, _)) => {
                        let id = *break_id;
                        self.close(MTerm::Jump(id));
                    }
                    None => self.close(MTerm::FallOff { span }),
                }
            }
            HStmtKind::Continue => {
                let span = stmt.span;
                match self.loop_stack.last() {
                    Some((_, cont_id)) => {
                        let id = *cont_id;
                        self.close(MTerm::Jump(id));
                    }
                    None => self.close(MTerm::FallOff { span }),
                }
            }
            HStmtKind::While { cond, body } => self.lower_while(cond, body, ret),
            HStmtKind::For {
                name, iter, body, ..
            } => {
                self.lower_for(name, iter, body, ret, stmt.span);
            }
            HStmtKind::Go { body, captured } => self.lower_go(body, captured, stmt.span),
            HStmtKind::Select { cases, default } => {
                self.lower_select(cases, default.as_deref(), ret, stmt.span)
            }
        }
    }

    /// Lower `go <block>`: capture values are read at creation (spec
    /// §5.7/§13.2), the body runs as a parameterless unit body on the
    /// spawned task. A tail expression runs for effects and is discarded;
    /// `return` inside exits the task.
    fn lower_go(&mut self, body: &HBlock, captured: &[(String, Ty)], span: Span) {
        let mut srcs = Vec::with_capacity(captured.len());
        for (name, _) in captured {
            srcs.push(self.resolve(name));
        }
        let mut sub = FnBuilder::new(self.checked);
        for (name, ty) in captured {
            let l = sub.fresh(format!("{name} (capture)"), ty.clone());
            sub.scopes
                .last_mut()
                .expect("scope underflow")
                .insert(name.clone(), l);
        }
        sub.entry_safepoint();
        sub.lower_stmts(&body.stmts, &Ty::Unit);
        if !sub.sealed
            && let Some(t) = &body.tail
        {
            let _ = sub.lower_expr(t, &Ty::Unit);
        }
        if !sub.sealed {
            let u = sub.materialize_unit();
            sub.close(MTerm::Return(Some(u)));
        }
        sub.blocks.sort_by_key(|b| b.id);
        let mir_body = Arc::new(MirBody {
            params: Vec::new(),
            ret: Ty::Unit,
            captures: captured.to_vec(),
            locals: sub.locals,
            blocks: sub.blocks,
            entry: 0,
            body_span: body.span,
        });
        self.emit(MInstr::Safepoint {
            kind: SpKind::Alloc,
        });
        self.emit(MInstr::Spawn {
            body: mir_body,
            captures: srcs,
            span,
        });
    }

    /// Lower `select`: guards evaluate left-to-right, then one
    /// `Select` rendezvous picks the case index; a branch chain runs
    /// the chosen body. An empty guard list with no default never
    /// unblocks (the task parks forever; the deadlock detector fires).
    fn lower_select(
        &mut self,
        cases: &[crate::hir::HSelectCase],
        default: Option<&HBlock>,
        ret: &Ty,
        span: Span,
    ) {
        use crate::hir::HSelectCase;
        let mut mcases = Vec::with_capacity(cases.len());
        // Bind slots for receive-and-bind cases: created with the guards
        // (so the `Select` rendezvous can write them), installed into
        // scope only while lowering their own case body.
        let mut binds: Vec<Option<(String, Local)>> = Vec::with_capacity(cases.len());
        for case in cases {
            match case {
                HSelectCase::Recv { name, chan, .. } => {
                    let c = self.lower_expr(chan, ret);
                    let bind = match name {
                        Some((n, _)) => {
                            let elem = match &self.locals[c].1 {
                                Ty::Chan(t) => (**t).clone(),
                                other => {
                                    debug_assert!(
                                        false,
                                        "checker guarantees channels here, found {other}"
                                    );
                                    Ty::Unit
                                }
                            };
                            let b = self.fresh(n.clone(), elem);
                            binds.push(Some((n.clone(), b)));
                            Some(b)
                        }
                        None => {
                            binds.push(None);
                            None
                        }
                    };
                    mcases.push(MSelectCase::Recv {
                        chan: c,
                        bind,
                        span: chan.span,
                    });
                }
                HSelectCase::Send { chan, value, .. } => {
                    let c = self.lower_expr(chan, ret);
                    let v = self.lower_expr(value, ret);
                    binds.push(None);
                    mcases.push(MSelectCase::Send {
                        chan: c,
                        value: v,
                        span: chan.span,
                    });
                }
            }
        }
        let dst = self.fresh("select".to_string(), Ty::Int);
        self.emit(MInstr::Safepoint { kind: SpKind::Park });
        self.emit(MInstr::Select {
            dst,
            cases: mcases,
            has_default: default.is_some(),
            span,
        });
        let join_id = self.fresh_block();
        for (i, case) in cases.iter().enumerate() {
            let body_id = self.fresh_block();
            let next_id = self.fresh_block();
            let idx = self.const_local(MConst::Int(i as i64), Ty::Int, "case index");
            let c = self.fresh(format!("case {i} chosen"), Ty::Bool);
            self.emit(MInstr::Bin {
                dst: c,
                op: MBinOp::Eq,
                l: dst,
                r: idx,
                span,
            });
            self.close(MTerm::Branch {
                cond: c,
                then_b: body_id,
                else_b: next_id,
                span,
                while_cond: false,
            });
            self.open(body_id);
            // The recv-bind local (created with the guards) is visible
            // only in its own case body.
            self.scopes.push(HashMap::new());
            if let Some((n, b)) = &binds[i] {
                self.scopes
                    .last_mut()
                    .expect("scope underflow")
                    .insert(n.clone(), *b);
            }
            let hbody = match case {
                HSelectCase::Recv { body, .. } | HSelectCase::Send { body, .. } => body,
            };
            self.lower_stmts(&hbody.stmts, ret);
            if !self.sealed
                && let Some(t) = &hbody.tail
            {
                let _ = self.lower_expr(t, ret);
            }
            self.scopes.pop();
            if !self.sealed {
                self.close(MTerm::Jump(join_id));
            }
            self.open(next_id);
        }
        match default {
            Some(b) => {
                self.scopes.push(HashMap::new());
                self.lower_stmts(&b.stmts, ret);
                if !self.sealed
                    && let Some(t) = &b.tail
                {
                    let _ = self.lower_expr(t, ret);
                }
                self.scopes.pop();
                if !self.sealed {
                    self.close(MTerm::Jump(join_id));
                }
                self.open(join_id);
            }
            None => {
                self.close(MTerm::Jump(join_id));
                self.open(join_id);
            }
        }
    }

    fn resolve(&self, name: &str) -> Local {
        *self
            .scopes
            .iter()
            .rev()
            .find_map(|s| s.get(name))
            .expect("checked program resolves every name")
    }

    fn lower_while(&mut self, cond: &HExpr, body: &HBlock, ret: &Ty) {
        let cond_id = self.fresh_block();
        let body_id = self.fresh_block();
        let join_id = self.fresh_block();
        self.close(MTerm::Jump(cond_id));
        self.open(cond_id);
        self.emit(MInstr::Safepoint { kind: SpKind::Loop });
        let c = self.lower_expr(cond, ret);
        let cspan = cond.span;
        self.close(MTerm::Branch {
            cond: c,
            then_b: body_id,
            else_b: join_id,
            span: cspan,
            while_cond: true,
        });
        self.open(body_id);
        self.loop_stack.push((join_id, cond_id));
        self.scopes.push(HashMap::new());
        self.lower_stmts(&body.stmts, ret);
        if let Some(t) = &body.tail {
            let _ = self.lower_expr(t, ret);
        }
        self.scopes.pop();
        self.loop_stack.pop();
        if !self.sealed {
            self.close(MTerm::Jump(cond_id));
        }
        // The join is reachable from the condition's false edge even when
        // the body diverged; when both sides diverge it is dead code that
        // is still built and never executed.
        self.open(join_id);
    }

    fn lower_for(&mut self, name: &str, iter: &HExpr, body: &HBlock, ret: &Ty, span: Span) {
        // The checker guarantees Range or Array here.
        if matches!(iter.ty, Ty::Range) {
            // Fast path: a range literal lowers its bounds directly, in
            // left-to-right order like the interpreter's RangeLit.
            if let HExprKind::Range(lo_expr, hi_expr) = &iter.kind {
                let lo = self.lower_expr(lo_expr, ret);
                let hi = self.lower_expr(hi_expr, ret);
                self.lower_for_range(name, lo, hi, body, ret);
                return;
            }
            let it = self.lower_expr(iter, ret);
            let lo = self.fresh(format!("{name} lo"), Ty::Int);
            let hi = self.fresh(format!("{name} hi"), Ty::Int);
            self.emit(MInstr::SplitRange {
                lo,
                hi,
                range: it,
                span,
            });
            self.lower_for_range(name, lo, hi, body, ret);
        } else {
            self.lower_for_array(name, iter, body, ret, span);
        }
    }

    /// Mirror the interpreter exactly: `while i < end`, saturating
    /// increment, explicit `i == end` break (spec §5.5).
    fn lower_for_range(&mut self, name: &str, lo: Local, hi: Local, body: &HBlock, ret: &Ty) {
        let i = self.fresh(format!("{name} i"), Ty::Int);
        self.emit(MInstr::Copy { dst: i, src: lo });
        let cond_id = self.fresh_block();
        let body_id = self.fresh_block();
        let incr_id = self.fresh_block();
        let join_id = self.fresh_block();
        self.close(MTerm::Jump(cond_id));
        self.open(cond_id);
        self.emit(MInstr::Safepoint { kind: SpKind::Loop });
        let c = self.fresh(format!("{name} cond"), Ty::Bool);
        self.emit(MInstr::Bin {
            dst: c,
            op: MBinOp::Lt,
            l: i,
            r: hi,
            span: body.span,
        });
        self.close(MTerm::Branch {
            cond: c,
            then_b: body_id,
            else_b: join_id,
            span: body.span,
            while_cond: false,
        });
        self.open(body_id);
        self.loop_stack.push((join_id, incr_id));
        self.scopes.push(HashMap::new());
        let x = self.fresh(name.to_string(), Ty::Int);
        self.emit(MInstr::Copy { dst: x, src: i });
        self.scopes
            .last_mut()
            .expect("scope underflow")
            .insert(name.to_string(), x);
        self.lower_stmts(&body.stmts, ret);
        if let Some(t) = &body.tail {
            let _ = self.lower_expr(t, ret);
        }
        self.scopes.pop();
        self.loop_stack.pop();
        if !self.sealed {
            self.close(MTerm::Jump(incr_id));
        }
        self.open(incr_id);
        let i2 = self.fresh(format!("{name} next"), Ty::Int);
        self.emit(MInstr::SatIncr {
            dst: i2,
            operand: i,
        });
        let done = self.fresh(format!("{name} done"), Ty::Bool);
        self.emit(MInstr::Bin {
            dst: done,
            op: MBinOp::Eq,
            l: i2,
            r: hi,
            span: body.span,
        });
        self.emit(MInstr::Copy { dst: i, src: i2 });
        self.close(MTerm::Branch {
            cond: done,
            then_b: join_id,
            else_b: cond_id,
            span: body.span,
            while_cond: false,
        });
        self.open(join_id);
    }

    /// Mirror the interpreter exactly: iterate a snapshot of the elements
    /// present when the loop starts (spec §5.5).
    fn lower_for_array(&mut self, name: &str, iter: &HExpr, body: &HBlock, ret: &Ty, span: Span) {
        let it = self.lower_expr(iter, ret);
        // The checker guarantees an array; its element type is the loop
        // variable's type.
        let elem_ty = match &iter.ty {
            Ty::Array(t) => (**t).clone(),
            other => {
                debug_assert!(false, "checker guarantees arrays here, found {other}");
                Ty::Unit
            }
        };
        let snap = self.fresh(format!("{name} snap"), Ty::Array(Box::new(elem_ty.clone())));
        self.emit(MInstr::Snapshot {
            dst: snap,
            base: it,
            span,
        });
        let n = self.fresh(format!("{name} len"), Ty::Int);
        self.emit(MInstr::Len {
            dst: n,
            base: snap,
            span,
        });
        let i = self.fresh(format!("{name} i"), Ty::Int);
        let zero = self.const_local(MConst::Int(0), Ty::Int, "0");
        self.emit(MInstr::Copy { dst: i, src: zero });
        let cond_id = self.fresh_block();
        let body_id = self.fresh_block();
        let incr_id = self.fresh_block();
        let join_id = self.fresh_block();
        self.close(MTerm::Jump(cond_id));
        self.open(cond_id);
        self.emit(MInstr::Safepoint { kind: SpKind::Loop });
        let c = self.fresh(format!("{name} cond"), Ty::Bool);
        self.emit(MInstr::Bin {
            dst: c,
            op: MBinOp::Lt,
            l: i,
            r: n,
            span: body.span,
        });
        self.close(MTerm::Branch {
            cond: c,
            then_b: body_id,
            else_b: join_id,
            span: body.span,
            while_cond: false,
        });
        self.open(body_id);
        self.loop_stack.push((join_id, incr_id));
        self.scopes.push(HashMap::new());
        let x = self.fresh(name.to_string(), elem_ty);
        self.emit(MInstr::Index {
            dst: x,
            base: snap,
            index: i,
            span,
            index_span: span,
        });
        self.scopes
            .last_mut()
            .expect("scope underflow")
            .insert(name.to_string(), x);
        self.lower_stmts(&body.stmts, ret);
        if let Some(t) = &body.tail {
            let _ = self.lower_expr(t, ret);
        }
        self.scopes.pop();
        self.loop_stack.pop();
        if !self.sealed {
            self.close(MTerm::Jump(incr_id));
        }
        self.open(incr_id);
        let one = self.const_local(MConst::Int(1), Ty::Int, "1");
        let i2 = self.fresh(format!("{name} next"), Ty::Int);
        self.emit(MInstr::Bin {
            dst: i2,
            op: MBinOp::Add,
            l: i,
            r: one,
            span,
        });
        self.emit(MInstr::Copy { dst: i, src: i2 });
        self.close(MTerm::Jump(cond_id));
        self.open(join_id);
    }

    fn lower_expr(&mut self, expr: &HExpr, ret: &Ty) -> Local {
        match &expr.kind {
            HExprKind::Int(v) => self.const_local(MConst::Int(*v), Ty::Int, "int"),
            HExprKind::Float(v) => self.const_local(MConst::Float(*v), Ty::Float, "float"),
            HExprKind::Str(s) => self.const_local(MConst::Str(s.clone()), Ty::String, "string"),
            HExprKind::Bool(b) => self.const_local(MConst::Bool(*b), Ty::Bool, "bool"),
            HExprKind::Unit => self.const_local(MConst::Unit, Ty::Unit, "unit"),
            HExprKind::None => self.const_local(MConst::None, expr.ty.clone(), "none"),
            HExprKind::Var(name, varref) => {
                // Declared state resolves by slot (spec amendment,
                // Phase 6). A shadowing local would have resolved to
                // `VarRef::Local` in the checker, so matching the
                // resolved form is exact — no scope-order subtlety.
                if let VarRef::State { .. } = varref {
                    let slot = *self
                        .state_slots
                        .get(name)
                        .unwrap_or_else(|| panic!("checked state binding `{name}` has no slot"));
                    let dst = self.fresh(format!("{name} (state)"), expr.ty.clone());
                    self.emit(MInstr::StateGet {
                        dst,
                        slot,
                        span: expr.span,
                    });
                    return dst;
                }
                if let Some(l) = self.scopes.iter().rev().find_map(|s| s.get(name)) {
                    *l
                } else if let Some(idx) = self.checked_fn_index(name) {
                    let dst = self.fresh(format!("fn {name}"), expr.ty.clone());
                    self.emit(MInstr::FuncRef { dst, idx });
                    dst
                } else {
                    // Unreachable: the checker resolves every name.
                    debug_assert!(false, "unresolved name {name} in MIR lowering");
                    self.const_local(MConst::Unit, Ty::Unit, "unit")
                }
            }
            HExprKind::If {
                cond,
                then_b,
                else_b,
            } => self.lower_if(cond, then_b, else_b, &expr.ty, ret),
            HExprKind::Closure {
                params,
                ret: cret,
                body,
                captured,
            } => self.lower_closure(params, cret, body, captured, expr.span),
            HExprKind::StructLit { name, fields, .. } => {
                let mut hfields = Vec::with_capacity(fields.len());
                for (fname, fexpr, _) in fields {
                    let v = self.lower_expr(fexpr, ret);
                    hfields.push((fname.clone(), v));
                }
                let dst = self.fresh(format!("struct {name}"), expr.ty.clone());
                self.emit(MInstr::Safepoint {
                    kind: SpKind::Alloc,
                });
                self.emit(MInstr::StructLit {
                    dst,
                    name: name.clone(),
                    fields: hfields,
                    span: expr.span,
                });
                dst
            }
            HExprKind::Array(elems) => {
                let mut vs = Vec::with_capacity(elems.len());
                for e in elems {
                    vs.push(self.lower_expr(e, ret));
                }
                let dst = self.fresh("array".to_string(), expr.ty.clone());
                self.emit(MInstr::Safepoint {
                    kind: SpKind::Alloc,
                });
                self.emit(MInstr::ArrayLit {
                    dst,
                    elems: vs,
                    span: expr.span,
                });
                dst
            }
            HExprKind::Range(lo, hi) => {
                let l = self.lower_expr(lo, ret);
                let h = self.lower_expr(hi, ret);
                let dst = self.fresh("range".to_string(), Ty::Range);
                self.emit(MInstr::Safepoint {
                    kind: SpKind::Alloc,
                });
                self.emit(MInstr::RangeLit {
                    dst,
                    lo: l,
                    hi: h,
                    span: expr.span,
                });
                dst
            }
            HExprKind::Binary { op, left, right } => {
                self.lower_binary(*op, left, right, &expr.ty, ret, expr.span)
            }
            HExprKind::Unary { op, expr: inner } => {
                let v = self.lower_expr(inner, ret);
                let dst = self.fresh("unary".to_string(), expr.ty.clone());
                match op {
                    crate::ast::UnOp::Neg => self.emit(MInstr::Neg {
                        dst,
                        operand: v,
                        span: expr.span,
                    }),
                    crate::ast::UnOp::Not => self.emit(MInstr::Not {
                        dst,
                        operand: v,
                        span: expr.span,
                    }),
                }
                dst
            }
            HExprKind::Send { chan, value } => {
                let c = self.lower_expr(chan, ret);
                let v = self.lower_expr(value, ret);
                let dst = self.fresh("send".to_string(), Ty::Unit);
                self.emit(MInstr::Safepoint { kind: SpKind::Park });
                self.emit(MInstr::Send {
                    chan: c,
                    value: v,
                    span: expr.span,
                });
                // A send statement discards the unit; keep the slot
                // well-formed for expression positions.
                let u = self.materialize_unit();
                self.emit(MInstr::Copy { dst, src: u });
                dst
            }
            HExprKind::Recv { chan } => {
                let c = self.lower_expr(chan, ret);
                let dst = self.fresh("recv".to_string(), expr.ty.clone());
                self.emit(MInstr::Safepoint { kind: SpKind::Park });
                self.emit(MInstr::Recv {
                    dst,
                    chan: c,
                    span: expr.span,
                });
                dst
            }
            HExprKind::ChanNew { cap, .. } => {
                let hcap = cap.as_ref().map(|c| self.lower_expr(c, ret));
                let elem = match &expr.ty {
                    Ty::Chan(t) => (**t).clone(),
                    other => {
                        debug_assert!(false, "checker guarantees chan types here, found {other}");
                        Ty::Unit
                    }
                };
                let dst = self.fresh("chan".to_string(), expr.ty.clone());
                self.emit(MInstr::Safepoint {
                    kind: SpKind::Alloc,
                });
                self.emit(MInstr::ChanNew {
                    dst,
                    elem,
                    cap: hcap,
                    span: expr.span,
                });
                dst
            }
            HExprKind::Call { callee, args } => {
                let display = match &callee.kind {
                    HExprKind::Var(name, _) => name.clone(),
                    _ => "<value>".to_string(),
                };
                let f = self.lower_callee(callee, ret);
                let mut vs = Vec::with_capacity(args.len());
                for a in args {
                    vs.push(self.lower_expr(a, ret));
                }
                let dst = self.fresh("call".to_string(), expr.ty.clone());
                self.emit(MInstr::Safepoint { kind: SpKind::Call });
                self.emit(MInstr::Call {
                    dst: Some(dst),
                    func: f,
                    args: vs,
                    span: expr.span,
                    display,
                });
                dst
            }
            HExprKind::CallBuiltin { name, args } => {
                let mut vs = Vec::with_capacity(args.len());
                for a in args {
                    vs.push(self.lower_expr(a, ret));
                }
                let dst = self.fresh(format!("{name}()"), expr.ty.clone());
                self.emit(MInstr::Safepoint { kind: SpKind::Call });
                self.emit(MInstr::CallBuiltin {
                    dst: Some(dst),
                    name: name.clone(),
                    args: vs,
                    span: expr.span,
                });
                dst
            }
            HExprKind::CallForeign { name, args } => {
                let mut vs = Vec::with_capacity(args.len());
                for a in args {
                    vs.push(self.lower_expr(a, ret));
                }
                let dst = self.fresh(format!("{name}()"), expr.ty.clone());
                self.emit(MInstr::Safepoint { kind: SpKind::Call });
                self.emit(MInstr::CallForeign {
                    dst: Some(dst),
                    name: name.clone(),
                    args: vs,
                    span: expr.span,
                });
                dst
            }
            HExprKind::Field { base, name } => {
                let b = self.lower_expr(base, ret);
                let dst = self.fresh(format!("field {name}"), expr.ty.clone());
                self.emit(MInstr::Field {
                    dst,
                    base: b,
                    name: name.clone(),
                    span: expr.span,
                });
                dst
            }
            HExprKind::Index { base, index } => {
                let b = self.lower_expr(base, ret);
                let i = self.lower_expr(index, ret);
                let dst = self.fresh("index".to_string(), expr.ty.clone());
                self.emit(MInstr::Index {
                    dst,
                    base: b,
                    index: i,
                    span: expr.span,
                    index_span: index.span,
                });
                dst
            }
            HExprKind::Ok(inner) => self.lower_wrap(inner, ret, &expr.ty, true),
            HExprKind::Err(inner) => self.lower_wrap(inner, ret, &expr.ty, false),
            HExprKind::Some(inner) => {
                let v = self.lower_expr(inner, ret);
                let dst = self.fresh("some".to_string(), expr.ty.clone());
                self.emit(MInstr::Safepoint {
                    kind: SpKind::Alloc,
                });
                self.emit(MInstr::WrapSome { dst, inner: v });
                dst
            }
            HExprKind::Question { operand, .. } => {
                let v = self.lower_expr(operand, ret);
                let dst = self.fresh("question".to_string(), expr.ty.clone());
                self.emit(MInstr::Propagate {
                    dst,
                    operand: v,
                    span: expr.span,
                });
                dst
            }
        }
    }

    fn checked_fn_index(&self, name: &str) -> Option<usize> {
        self.checked.fns.iter().position(|s| s.name == name)
    }

    fn lower_callee(&mut self, callee: &HExpr, ret: &Ty) -> Local {
        // Callee evaluated before arguments (spec §5.1).
        self.lower_expr(callee, ret)
    }

    fn lower_wrap(&mut self, inner: &HExpr, ret: &Ty, ty: &Ty, is_ok: bool) -> Local {
        let v = self.lower_expr(inner, ret);
        let dst = self.fresh(
            if is_ok {
                "ok".to_string()
            } else {
                "err".to_string()
            },
            ty.clone(),
        );
        self.emit(MInstr::Safepoint {
            kind: SpKind::Alloc,
        });
        if is_ok {
            self.emit(MInstr::WrapOk { dst, inner: v });
        } else {
            self.emit(MInstr::WrapErr { dst, inner: v });
        }
        dst
    }

    fn lower_binary(
        &mut self,
        op: crate::ast::BinOp,
        left: &HExpr,
        right: &HExpr,
        ty: &Ty,
        ret: &Ty,
        span: Span,
    ) -> Local {
        use crate::ast::BinOp;
        match op {
            BinOp::And => {
                let l = self.lower_expr(left, ret);
                let rhs_id = self.fresh_block();
                let false_id = self.fresh_block();
                let join_id = self.fresh_block();
                let t = self.fresh("and".to_string(), Ty::Bool);
                self.close(MTerm::Branch {
                    cond: l,
                    then_b: rhs_id,
                    else_b: false_id,
                    span,
                    while_cond: false,
                });
                self.open(rhs_id);
                let r = self.lower_expr(right, ret);
                self.emit(MInstr::Copy { dst: t, src: r });
                self.close(MTerm::Jump(join_id));
                self.open(false_id);
                let f = self.const_local(MConst::Bool(false), Ty::Bool, "false");
                self.emit(MInstr::Copy { dst: t, src: f });
                self.close(MTerm::Jump(join_id));
                self.open(join_id);
                t
            }
            BinOp::Or => {
                let l = self.lower_expr(left, ret);
                let rhs_id = self.fresh_block();
                let true_id = self.fresh_block();
                let join_id = self.fresh_block();
                let t = self.fresh("or".to_string(), Ty::Bool);
                self.close(MTerm::Branch {
                    cond: l,
                    then_b: true_id,
                    else_b: rhs_id,
                    span,
                    while_cond: false,
                });
                self.open(rhs_id);
                let r = self.lower_expr(right, ret);
                self.emit(MInstr::Copy { dst: t, src: r });
                self.close(MTerm::Jump(join_id));
                self.open(true_id);
                let tr = self.const_local(MConst::Bool(true), Ty::Bool, "true");
                self.emit(MInstr::Copy { dst: t, src: tr });
                self.close(MTerm::Jump(join_id));
                self.open(join_id);
                t
            }
            BinOp::Range => {
                let l = self.lower_expr(left, ret);
                let r = self.lower_expr(right, ret);
                let dst = self.fresh("range".to_string(), Ty::Range);
                self.emit(MInstr::Safepoint {
                    kind: SpKind::Alloc,
                });
                self.emit(MInstr::RangeLit {
                    dst,
                    lo: l,
                    hi: r,
                    span,
                });
                dst
            }
            _ => {
                let l = self.lower_expr(left, ret);
                let r = self.lower_expr(right, ret);
                let mop = match op {
                    BinOp::Add => MBinOp::Add,
                    BinOp::Sub => MBinOp::Sub,
                    BinOp::Mul => MBinOp::Mul,
                    BinOp::Div => MBinOp::Div,
                    BinOp::Mod => MBinOp::Mod,
                    BinOp::Eq => MBinOp::Eq,
                    BinOp::NotEq => MBinOp::NotEq,
                    BinOp::Lt => MBinOp::Lt,
                    BinOp::LtEq => MBinOp::LtEq,
                    BinOp::Gt => MBinOp::Gt,
                    BinOp::GtEq => MBinOp::GtEq,
                    BinOp::And | BinOp::Or | BinOp::Range => {
                        unreachable!("short-circuit and range handled above")
                    }
                };
                // Explicit zero check for integer `/` and `%` (trap
                // decision); float division is total (IEEE).
                let lty = self.locals[l].1.clone();
                let rty = self.locals[r].1.clone();
                if matches!(mop, MBinOp::Div | MBinOp::Mod) && lty == Ty::Int && rty == Ty::Int {
                    self.emit(MInstr::DivCheck { value: r, span });
                }
                let dst = self.fresh("bin".to_string(), ty.clone());
                self.emit(MInstr::Bin {
                    dst,
                    op: mop,
                    l,
                    r,
                    span,
                });
                dst
            }
        }
    }

    fn lower_if(
        &mut self,
        cond: &HExpr,
        then_b: &HBlock,
        else_b: &Option<Box<HBlock>>,
        ty: &Ty,
        ret: &Ty,
    ) -> Local {
        let c = self.lower_expr(cond, ret);
        let cspan = cond.span;
        let then_id = self.fresh_block();
        let else_id = self.fresh_block();
        let join_id = self.fresh_block();
        self.close(MTerm::Branch {
            cond: c,
            then_b: then_id,
            else_b: else_id,
            span: cspan,
            while_cond: false,
        });
        let join = self.fresh("if".to_string(), ty.clone());
        self.open(then_id);
        let tv = self.lower_block_value(then_b, ret);
        if !self.sealed {
            self.emit(MInstr::Copy { dst: join, src: tv });
            self.close(MTerm::Jump(join_id));
        }
        self.open(else_id);
        match else_b {
            Some(b) => {
                let ev = self.lower_block_value(b, ret);
                if !self.sealed {
                    self.emit(MInstr::Copy { dst: join, src: ev });
                    self.close(MTerm::Jump(join_id));
                }
            }
            None => {
                // No `else` implies a `unit` value (checked): join on `()`.
                let u = self.materialize_unit();
                self.emit(MInstr::Copy { dst: join, src: u });
                self.close(MTerm::Jump(join_id));
            }
        }
        // When both sides diverge the join is dead but well-formed.
        self.open(join_id);
        join
    }

    /// Lower a block for its value (tails of `if` branches). When the
    /// block diverges (ends in `return`), the tail is dead and skipped;
    /// callers ignore the returned local while sealed.
    fn lower_block_value(&mut self, b: &HBlock, ret: &Ty) -> Local {
        self.scopes.push(HashMap::new());
        self.lower_stmts(&b.stmts, ret);
        if self.sealed {
            self.scopes.pop();
            return 0;
        }
        let v = match &b.tail {
            Some(t) => self.lower_expr(t, ret),
            None => self.materialize_unit(),
        };
        self.scopes.pop();
        v
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_closure(
        &mut self,
        params: &[(String, Ty, bool)],
        cret: &Ty,
        body: &HBlock,
        captured: &[(String, Ty)],
        span: Span,
    ) -> Local {
        // Resolve capture sources in the enclosing scopes first: capture
        // reads each value at creation (spec §5.7).
        let mut srcs = Vec::with_capacity(captured.len());
        for (name, _) in captured {
            srcs.push(self.resolve(name));
        }
        let mut sub = FnBuilder::new(self.checked);
        for (name, ty, _) in params {
            let l = sub.fresh(format!("{name} (param)"), ty.clone());
            sub.scopes
                .last_mut()
                .expect("scope underflow")
                .insert(name.clone(), l);
        }
        for (name, ty) in captured {
            let l = sub.fresh(format!("{name} (capture)"), ty.clone());
            sub.scopes
                .last_mut()
                .expect("scope underflow")
                .insert(name.clone(), l);
        }
        sub.entry_safepoint();
        sub.lower_stmts(&body.stmts, cret);
        if !sub.sealed {
            match &body.tail {
                Some(t) => {
                    let v = sub.lower_expr(t, cret);
                    sub.close(MTerm::Return(Some(v)));
                }
                None => {
                    if *cret == Ty::Unit {
                        let u = sub.materialize_unit();
                        sub.close(MTerm::Return(Some(u)));
                    } else {
                        sub.close(MTerm::FallOff { span: body.span });
                    }
                }
            }
        }
        sub.blocks.sort_by_key(|b| b.id);
        let mir_body = Arc::new(MirBody {
            params: params
                .iter()
                .map(|(n, t, _)| (n.clone(), t.clone()))
                .collect(),
            ret: cret.clone(),
            captures: captured.to_vec(),
            locals: sub.locals,
            blocks: sub.blocks,
            entry: 0,
            body_span: body.span,
        });
        let dst = self.fresh(
            "closure".to_string(),
            Ty::Fn(
                params.iter().map(|(_, t, _)| t.clone()).collect(),
                Box::new(cret.clone()),
            ),
        );
        self.emit(MInstr::Safepoint {
            kind: SpKind::Alloc,
        });
        self.emit(MInstr::MakeClosure {
            dst,
            body: mir_body,
            captures: srcs,
            span,
        });
        dst
    }
}

impl fmt::Display for MProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, func) in self.fns.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{func}")?;
        }
        // Migration bodies print after functions (they share `MFn`
        // shape but never run during single-generation execution).
        for mig in self.migrates.iter() {
            writeln!(f)?;
            write!(f, "{mig}")?;
        }
        Ok(())
    }
}

impl fmt::Display for MFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn {}(", self.name)?;
        for (i, (name, ty)) in self.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name}: {ty}")?;
        }
        writeln!(f, ") -> {} {{", self.ret)?;
        for (id, (name, ty)) in self.locals.iter().enumerate() {
            writeln!(f, "  // %{id}: {ty} ({name})")?;
        }
        for block in &self.blocks {
            write!(f, "{block}")?;
        }
        writeln!(f, "}}")
    }
}

impl fmt::Display for MirBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "closure(")?;
        for (i, (name, ty)) in self.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name}: {ty}")?;
        }
        write!(f, ")[")?;
        for (i, (name, ty)) in self.captures.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name}: {ty}")?;
        }
        writeln!(f, "] {{")?;
        for block in &self.blocks {
            write!(f, "{block}")?;
        }
        write!(f, "}}")
    }
}

impl fmt::Display for MBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  bb{}:", self.id)?;
        for instr in &self.instrs {
            writeln!(f, "    {instr}")?;
        }
        writeln!(f, "    {term}", term = self.term)
    }
}

fn local(id: Local) -> String {
    format!("%{id}")
}

impl fmt::Display for MInstr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MInstr::Const { dst, val } => write!(f, "{} = const {val}", local(*dst)),
            MInstr::Copy { dst, src } => write!(f, "{} = copy {}", local(*dst), local(*src)),
            MInstr::Bin { dst, op, l, r, .. } => {
                write!(f, "{} = {op} {}, {}", local(*dst), local(*l), local(*r))
            }
            MInstr::Neg { dst, operand, .. } => {
                write!(f, "{} = neg {}", local(*dst), local(*operand))
            }
            MInstr::Not { dst, operand, .. } => {
                write!(f, "{} = not {}", local(*dst), local(*operand))
            }
            MInstr::DivCheck { value, .. } => write!(f, "divcheck {}", local(*value)),
            MInstr::Call {
                dst, func, args, ..
            } => {
                write!(
                    f,
                    "{} = call {}(",
                    dst.map(local).unwrap_or_else(|| "_".to_string()),
                    local(*func)
                )?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", local(*a))?;
                }
                write!(f, ")")
            }
            MInstr::CallBuiltin {
                dst, name, args, ..
            } => {
                write!(
                    f,
                    "{} = builtin {name}(",
                    dst.map(local).unwrap_or_else(|| "_".to_string())
                )?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", local(*a))?;
                }
                write!(f, ")")
            }
            MInstr::CallForeign {
                dst, name, args, ..
            } => {
                write!(
                    f,
                    "{} = foreign {name}(",
                    dst.map(local).unwrap_or_else(|| "_".to_string())
                )?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", local(*a))?;
                }
                write!(f, ")")
            }
            MInstr::FuncRef { dst, idx } => write!(f, "{} = funcref {idx}", local(*dst)),
            MInstr::StructLit {
                dst, name, fields, ..
            } => {
                write!(f, "{} = struct {name} {{ ", local(*dst))?;
                for (i, (n, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{n} = {}", local(*v))?;
                }
                write!(f, " }}")
            }
            MInstr::ArrayLit { dst, elems, .. } => {
                write!(f, "{} = array [", local(*dst))?;
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", local(*e))?;
                }
                write!(f, "]")
            }
            MInstr::Snapshot { dst, base, .. } => {
                write!(f, "{} = snapshot {}", local(*dst), local(*base))
            }
            MInstr::RangeLit { dst, lo, hi, .. } => {
                write!(f, "{} = range {}, {}", local(*dst), local(*lo), local(*hi))
            }
            MInstr::SplitRange { lo, hi, range, .. } => {
                write!(
                    f,
                    "{}, {} = split {}",
                    local(*lo),
                    local(*hi),
                    local(*range)
                )
            }
            MInstr::WrapOk { dst, inner } => {
                write!(f, "{} = wrap_ok {}", local(*dst), local(*inner))
            }
            MInstr::WrapErr { dst, inner } => {
                write!(f, "{} = wrap_err {}", local(*dst), local(*inner))
            }
            MInstr::WrapSome { dst, inner } => {
                write!(f, "{} = wrap_some {}", local(*dst), local(*inner))
            }
            MInstr::Propagate { dst, operand, .. } => {
                write!(f, "{} = propagate {}", local(*dst), local(*operand))
            }
            MInstr::Field {
                dst, base, name, ..
            } => {
                write!(f, "{} = field {}.{name}", local(*dst), local(*base))
            }
            MInstr::Index {
                dst, base, index, ..
            } => {
                write!(
                    f,
                    "{} = index {}[{}]",
                    local(*dst),
                    local(*base),
                    local(*index)
                )
            }
            MInstr::StoreField {
                base, name, value, ..
            } => {
                write!(f, "store_field {}.{name} = {}", local(*base), local(*value))
            }
            MInstr::StoreIndex {
                base, index, value, ..
            } => {
                write!(
                    f,
                    "store_index {}[{}] = {}",
                    local(*base),
                    local(*index),
                    local(*value)
                )
            }
            MInstr::Len { dst, base, .. } => write!(f, "{} = len {}", local(*dst), local(*base)),
            MInstr::StateGet { dst, slot, .. } => {
                write!(f, "{} = state_get #{slot}", local(*dst))
            }
            MInstr::StateSet { slot, src, .. } => {
                write!(f, "state_set #{slot} = {}", local(*src))
            }
            MInstr::SatIncr { dst, operand } => {
                write!(f, "{} = satincr {}", local(*dst), local(*operand))
            }
            MInstr::MakeClosure { dst, captures, .. } => {
                write!(f, "{} = make_closure [", local(*dst))?;
                for (i, c) in captures.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", local(*c))?;
                }
                write!(f, "]")
            }
            MInstr::ChanNew { dst, elem, cap, .. } => match cap {
                Some(c) => write!(f, "{} = channew {} cap {}", local(*dst), elem, local(*c)),
                None => write!(f, "{} = channew {}", local(*dst), elem),
            },
            MInstr::Send { chan, value, .. } => {
                write!(f, "send {} <- {}", local(*chan), local(*value))
            }
            MInstr::Recv { dst, chan, .. } => {
                write!(f, "{} = recv {}", local(*dst), local(*chan))
            }
            MInstr::Spawn { captures, .. } => {
                write!(f, "spawn [")?;
                for (i, c) in captures.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", local(*c))?;
                }
                write!(f, "]")
            }
            MInstr::Select {
                dst,
                cases,
                has_default,
                ..
            } => {
                write!(f, "{} = select [", local(*dst))?;
                for (i, c) in cases.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match c {
                        MSelectCase::Recv { chan, bind, .. } => match bind {
                            Some(b) => write!(f, "recv {} -> {}", local(*chan), local(*b))?,
                            None => write!(f, "recv {}", local(*chan))?,
                        },
                        MSelectCase::Send { chan, value, .. } => {
                            write!(f, "send {} <- {}", local(*chan), local(*value))?
                        }
                    }
                    let _ = i;
                }
                write!(f, "]")?;
                if *has_default {
                    write!(f, " else default")?;
                }
                Ok(())
            }
            MInstr::Safepoint { kind } => write!(f, "safepoint {kind}"),
        }
    }
}

impl fmt::Display for MTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MTerm::Return(None) => write!(f, "ret"),
            MTerm::Return(Some(v)) => write!(f, "ret {}", local(*v)),
            MTerm::FallOff { .. } => write!(f, "falloff"),
            MTerm::Jump(id) => write!(f, "jump bb{id}"),
            MTerm::Branch {
                cond,
                then_b,
                else_b,
                ..
            } => {
                write!(f, "br {} bb{then_b} bb{else_b}", local(*cond))
            }
        }
    }
}

impl fmt::Display for MBinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MBinOp::Add => "add",
            MBinOp::Sub => "sub",
            MBinOp::Mul => "mul",
            MBinOp::Div => "div",
            MBinOp::Mod => "mod",
            MBinOp::Eq => "eq",
            MBinOp::NotEq => "ne",
            MBinOp::Lt => "lt",
            MBinOp::LtEq => "le",
            MBinOp::Gt => "gt",
            MBinOp::GtEq => "ge",
        };
        write!(f, "{s}")
    }
}

impl fmt::Display for SpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SpKind::Entry => "entry",
            SpKind::Loop => "loop",
            SpKind::Call => "call",
            SpKind::Alloc => "alloc",
            SpKind::Park => "park",
        };
        write!(f, "{s}")
    }
}

impl fmt::Display for MConst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MConst::Int(v) => write!(f, "{v}:int"),
            MConst::Float(v) => write!(f, "{v:?}:float"),
            MConst::Str(s) => write!(f, "{s:?}:string"),
            MConst::Bool(b) => write!(f, "{b}:bool"),
            MConst::Unit => write!(f, "():unit"),
            MConst::None => write!(f, "none"),
        }
    }
}
