//! Declared-state carry planning + migration evaluation (Phase 6,
//! Stage 3): decide, at reload, which declared state survives into the
//! next generation and compute its values. Never silent: every refusal
//! is a loud `CarryAbort` routing to atomic restart (drain-and-restart
//! with fresh initialization — the Phase 4 behavior).
//!
//! Rules (spec §14.3, authorization §2):
//! - Same names + same types, no layout change: carry verbatim.
//! - Added or removed state bindings: abort (the new generation
//!   re-initializes from scratch on the atomic path; initializers
//!   never run twice on the preserving path).
//! - Struct layout change: run the new program's `migrate` for that
//!   struct (missing migrate, failed reload-time totality, or a
//!   migrate panic all abort loudly). No implicit defaults.
//! - Channel type change: empty queue carries with the new type;
//!   non-empty queue aborts loudly. (Post-settle state channels are
//!   always unbuffered with no waiters, so the non-empty case is
//!   defense-only — still asserted.)
//! - `Func` values anywhere in carried state: the old/new MIR fn
//!   tables must match in order and signature (indices stay valid);
//!   otherwise abort loudly. Uniform for top-level and nested
//!   (unlike the Stage 2a explicit carry, which relinks by name).
//! - Fn-signature changes do NOT involve `migrate` (per-struct only):
//!   code restarts, state carries subject to the rules above.
//! - Migration evaluates on a snapshot: the old heap is never
//!   mutated (fresh objects for the result; channels, closures, and
//!   functions pass through by handle). Retained old closures keep
//!   seeing pre-migration objects.

use std::collections::{HashMap, HashSet};

use toto_compiler::ast::{BinOp, UnOp};
use toto_compiler::builtins;
use toto_compiler::check::migrate_param_reads;
use toto_compiler::diag::Diag;
use toto_compiler::hir::{
    CheckedProgram, HAssignTarget, HBlock, HExpr, HExprKind, HMigrate, HStmtKind, HStructDecl, Ty,
};
use toto_compiler::interp::Value;
use toto_compiler::ops;
use toto_compiler::span::Span;
use toto_runtime::channel::ChannelTable;
use toto_runtime::heap::{Heap, Payload};
use toto_runtime::value::RtVal;

/// Why a carry failed: loud atomic restart, never silent reinterpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarryAbort {
    pub reason: String,
}

impl CarryAbort {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Seed for a preserving reload: carried state values (every slot, in
/// declaration order) plus the moved channel table. The driver moves
/// the old heap alongside; handles stay valid because the heap moves
/// whole.
pub struct PreservingSeed {
    pub state_vars: Vec<RtVal>,
    pub channels: ChannelTable,
}

/// Plan the declared-state carry old → new. `old_values` are the old
/// generation's state slots in declaration order; `heap` holds them
/// (mutated only by migration evaluation, which allocates fresh
/// results — never mutates old objects); `channels` is the old
/// generation's channel table (moved into the new generation on
/// success). Returns the new generation's state slots in declaration
/// order, or a loud abort.
pub fn plan_carry(
    old: &CheckedProgram,
    old_values: &[RtVal],
    heap: &mut Heap,
    channels: &ChannelTable,
    new: &CheckedProgram,
) -> Result<Vec<RtVal>, CarryAbort> {
    let old_by_name: HashMap<&str, (usize, &Ty)> = old
        .state
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name.as_str(), (i, &b.ty)))
        .collect();
    let new_by_name: HashMap<&str, (usize, &Ty)> = new
        .state
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name.as_str(), (i, &b.ty)))
        .collect();
    for name in old_by_name.keys() {
        if !new_by_name.contains_key(name) {
            return Err(CarryAbort::new(format!(
                "state binding `{name}` removed: loud atomic restart (never silently dropped)"
            )));
        }
    }
    for name in new_by_name.keys() {
        if !old_by_name.contains_key(name) {
            return Err(CarryAbort::new(format!(
                "state binding `{name}` added: loud atomic restart (initializers run once, on fresh load)"
            )));
        }
    }
    // `Func` stability (spec §14.3 call directionality): indices are
    // valid across the reload iff the MIR fn tables match in order,
    // name, params, and return type. Otherwise any `Func` anywhere in
    // carried state aborts (a shifted index would silently call the
    // wrong function).
    let stable = fn_tables_stable(old, new);
    let mut out: Vec<RtVal> = Vec::with_capacity(new.state.len());
    for b in &new.state {
        let (old_idx, old_ty) = old_by_name[b.name.as_str()];
        let old_value = old_values.get(old_idx).cloned().unwrap_or(RtVal::Unit);
        out.push(carry_binding(
            &b.name, old_ty, &b.ty, old_value, old, new, heap, channels, stable,
        )?);
    }
    // Uniform `Func` rule: scan everything carried (migrated results
    // included — migrate can pass functions through). A `Func` under
    // unstable tables aborts the whole carry, never one slot.
    if !stable {
        let mut seen = HashSet::new();
        for v in &out {
            if contains_func(v, heap, &mut seen) {
                return Err(CarryAbort::new(
                    "carried state holds a function value but the fn table changed: \
                     loud atomic restart (indices would silently shift)"
                        .to_string(),
                ));
            }
        }
    }
    Ok(out)
}

/// MIR fn tables match in order, name, params, and return type.
fn fn_tables_stable(old: &CheckedProgram, new: &CheckedProgram) -> bool {
    let old_mir = toto_compiler::mir::lower(old);
    let new_mir = toto_compiler::mir::lower(new);
    old_mir.fns.len() == new_mir.fns.len()
        && old_mir
            .fns
            .iter()
            .zip(new_mir.fns.iter())
            .all(|(o, n)| o.name == n.name && o.params == n.params && o.ret == n.ret)
}

/// Carry one binding old type → new type.
#[allow(clippy::too_many_arguments)]
fn carry_binding(
    name: &str,
    old_ty: &Ty,
    new_ty: &Ty,
    old_value: RtVal,
    old: &CheckedProgram,
    new: &CheckedProgram,
    heap: &mut Heap,
    channels: &ChannelTable,
    stable: bool,
) -> Result<RtVal, CarryAbort> {
    // Top-level `Func` bindings: verbatim under stable tables (the
    // uniform scan above already approved them); anything else aborts
    // here rather than silently carrying a wrongly-typed value.
    if matches!(new_ty, Ty::Fn(..)) {
        match old_value {
            RtVal::Func(_) if stable && matches!(old_ty, Ty::Fn(..)) => return Ok(old_value),
            _ => {
                return Err(CarryAbort::new(format!(
                    "state binding `{name}` is a function value the reload cannot \
                     rebind: loud atomic restart"
                )));
            }
        }
    }
    // NOTE: `Ty::Struct` equality is name-only — layout lives in the
    // `structs` maps, so struct pairs are matched BEFORE the generic
    // identical-type arm (or layout changes would slip through).
    match (old_ty, new_ty) {
        // Struct layout change: migrate or loud restart.
        (Ty::Struct(old_s), Ty::Struct(new_s)) => {
            let old_decl = old.structs.get(old_s).ok_or_else(|| {
                CarryAbort::new(format!("internal: old struct `{old_s}` missing"))
            })?;
            let new_decl = new.structs.get(new_s).ok_or_else(|| {
                CarryAbort::new(format!("internal: new struct `{new_s}` missing"))
            })?;
            if same_layout(old_decl, new_decl) {
                return Ok(old_value);
            }
            // Same struct NAME, changed layout: the migrate for the
            // new definition runs over the old value.
            if old_s != new_s {
                return Err(CarryAbort::new(format!(
                    "state binding `{name}` changed struct type `{old_s}` → `{new_s}`: \
                     loud atomic restart (no cross-type migration)"
                )));
            }
            migrate_struct(name, old_decl, &old_value, new, heap)
        }
        // Channel revalidation (spec §14.3, Q4): empty queue carries
        // with the new type; non-empty queue + changed type aborts.
        (Ty::Chan(old_e), Ty::Chan(new_e)) => match old_value {
            RtVal::Chan(id) => {
                let queued = channels.queued_len(id).ok_or_else(|| {
                    CarryAbort::new(format!(
                        "state binding `{name}` names a dead channel: loud atomic restart"
                    ))
                })?;
                if old_e != new_e && queued > 0 {
                    return Err(CarryAbort::new(format!(
                        "state channel `{name}` changed element type with {queued} \
                         queued value(s): loud atomic restart (never reinterpreted)"
                    )));
                }
                Ok(RtVal::Chan(id))
            }
            _ => Err(CarryAbort::new(format!(
                "state binding `{name}` should hold a channel: loud atomic restart"
            ))),
        },
        // Identical residual types carry verbatim (nested `Func`s
        // approved by the uniform scan; nested closures ride
        // retention). Structs and channels are handled above.
        (o, n) if o == n => Ok(old_value),
        _ => Err(CarryAbort::new(format!(
            "state binding `{name}` changed type `{old_ty:?}` → `{new_ty:?}`: \
             loud atomic restart (never silently reinterpreted)"
        ))),
    }
}

/// Name-based layout equality (spec §14.3 tag discipline): same field
/// names with same types. Order is irrelevant (access is by name).
fn same_layout(old: &HStructDecl, new: &HStructDecl) -> bool {
    if old.fields.len() != new.fields.len() {
        return false;
    }
    let new_map: HashMap<&str, &Ty> = new.fields.iter().map(|(n, t)| (n.as_str(), t)).collect();
    old.fields
        .iter()
        .all(|(n, t)| new_map.get(n.as_str()) == Some(&t))
}

/// Run the new program's `migrate` for `target` over `old_value`.
fn migrate_struct(
    binding: &str,
    old_decl: &HStructDecl,
    old_value: &RtVal,
    new: &CheckedProgram,
    heap: &mut Heap,
) -> Result<RtVal, CarryAbort> {
    let mig = new.migrates.iter().find(|m| m.target == old_decl.name);
    let Some(mig) = mig else {
        return Err(CarryAbort::new(format!(
            "state binding `{binding}`: struct `{}` changed layout with no migrate \
             function: loud atomic restart (no implicit defaults)",
            old_decl.name
        )));
    };
    // Reload-time totality against the OLD fields (check time proved
    // it against the current definition; the old generation may have
    // had more fields — dropping one silently is never allowed).
    let (read, identical) = migrate_param_reads(&mig.body, &mig.param);
    if !identical {
        for (field, _) in &old_decl.fields {
            if !read.contains(field) {
                return Err(CarryAbort::new(format!(
                    "state binding `{binding}`: migrate `{}` never reads old field \
                     `{field}`: loud atomic restart (totality is reload-time, \
                     against the old definition)",
                    mig.target
                )));
            }
        }
    }
    let new_value = eval_migrate(mig, old_value, heap).map_err(|e| match e {
        // Genuine migrate failures keep their diagnostic identity.
        EvalFail::Panic(diag) => CarryAbort::new(format!(
            "state binding `{binding}`: migrate `{}` failed ({} {}): loud atomic \
             restart (reported, never swallowed)",
            mig.target, diag.code, diag.message
        )),
        EvalFail::Refused(reason) => CarryAbort::new(format!(
            "state binding `{binding}`: migrate `{}` refused ({reason}): loud atomic restart",
            mig.target
        )),
    })?;
    // Output validation (tag-mismatch discipline, spec §14.3): the
    // result's field types must match the NEW definition before the
    // value touches state. The identity case (`return old`) skips
    // this — it explicitly keeps the old shape (trivially total),
    // and new-field reads fail loud at use. Anything else producing
    // a wrongly-typed field aborts here, never silently.
    if !identical && let Some(new_decl) = new.structs.get(&mig.target) {
        validate_migrated(binding, &mig.target, &new_value, new_decl, heap)?;
    }
    Ok(new_value)
}

/// Shallow field-type check of a migrate result against the new
/// struct definition. Deep (nested-container) mismatches surface
/// loudly at use (every operator re-checks operand types).
fn validate_migrated(
    binding: &str,
    target: &str,
    value: &RtVal,
    new_decl: &HStructDecl,
    heap: &Heap,
) -> Result<(), CarryAbort> {
    let fields = match value {
        RtVal::Heap(r) => match heap.get(*r) {
            Some(Payload::Struct { name, fields }) if name == target => fields,
            _ => {
                return Err(CarryAbort::new(format!(
                    "state binding `{binding}`: migrate `{target}` produced a non-struct: \
                     loud atomic restart"
                )));
            }
        },
        _ => {
            return Err(CarryAbort::new(format!(
                "state binding `{binding}`: migrate `{target}` produced a non-struct: \
                 loud atomic restart"
            )));
        }
    };
    for (fname, fty) in &new_decl.fields {
        match fields.iter().find(|(n, _)| n == fname) {
            None => {
                return Err(CarryAbort::new(format!(
                    "state binding `{binding}`: migrate `{target}` dropped field \
                     `{fname}`: loud atomic restart"
                )));
            }
            Some((_, v)) => {
                if !value_matches_ty(v, fty, heap) {
                    return Err(CarryAbort::new(format!(
                        "state binding `{binding}`: migrate `{target}` gave field \
                         `{fname}` a `{}` where `{fty:?}` was declared: loud atomic \
                         restart (never silently reinterpreted)",
                        v.type_name(heap)
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Runtime type-tag check of one value against its declared type
/// (shallow: containers match by payload kind, contents validated
/// at use by every operator).
fn value_matches_ty(v: &RtVal, ty: &Ty, heap: &Heap) -> bool {
    match (v, ty) {
        (RtVal::Int(_), Ty::Int)
        | (RtVal::Float(_), Ty::Float)
        | (RtVal::Bool(_), Ty::Bool)
        | (RtVal::Unit, Ty::Unit)
        | (RtVal::None, Ty::Option(_))
        | (RtVal::Range(..), Ty::Range)
        | (RtVal::Ptr(_), Ty::CPtr(_)) => true,
        (RtVal::Heap(r), Ty::String) => matches!(heap.get(*r), Some(Payload::Str(_))),
        (RtVal::Heap(r), Ty::Array(_)) => matches!(heap.get(*r), Some(Payload::Array(_))),
        (RtVal::Heap(r), Ty::Struct(name)) => {
            matches!(heap.get(*r), Some(Payload::Struct { name: n, .. }) if n == name)
        }
        (RtVal::Heap(r), Ty::Range) => matches!(heap.get(*r), Some(Payload::Range { .. })),
        (RtVal::Heap(r), Ty::Result(_, _)) => {
            matches!(heap.get(*r), Some(Payload::Result { .. }))
        }
        (RtVal::Heap(r), Ty::Option(_)) => {
            matches!(heap.get(*r), Some(Payload::Option { .. }))
        }
        (RtVal::Chan(_), Ty::Chan(_)) => true,
        (RtVal::Func(_), Ty::Fn(..)) => true,
        (RtVal::Closure(_), Ty::Fn(..)) => true,
        (RtVal::Ok(_), Ty::Result(_, _)) => true,
        (RtVal::Err(_), Ty::Result(_, _)) => true,
        (RtVal::Some(_), Ty::Option(_)) => true,
        _ => false,
    }
}

/// True iff a `Func` value appears anywhere in `v` (through heap
/// containers, option/result wrappers, and closure captures).
/// `seen` guards heap cycles. Shared with the driver's retention
/// span (a carried `Func` force-retains its generation).
pub(crate) fn contains_func(
    v: &RtVal,
    heap: &Heap,
    seen: &mut HashSet<toto_runtime::heap::GcRef>,
) -> bool {
    match v {
        RtVal::Func(_) => true,
        RtVal::Heap(r) => {
            if !seen.insert(*r) {
                return false;
            }
            match heap.get(*r) {
                Some(Payload::Array(elems)) => elems.iter().any(|e| contains_func(e, heap, seen)),
                Some(Payload::Struct { fields, .. }) => {
                    fields.iter().any(|(_, e)| contains_func(e, heap, seen))
                }
                Some(Payload::Result { inner, .. }) | Some(Payload::Option { inner, .. }) => {
                    contains_func(inner, heap, seen)
                }
                Some(Payload::Closure { captures, .. }) => {
                    captures.iter().any(|e| contains_func(e, heap, seen))
                }
                _ => false,
            }
        }
        RtVal::Ok(inner) | RtVal::Err(inner) | RtVal::Some(inner) => {
            contains_func(inner, heap, seen)
        }
        RtVal::Closure(c) => c.captured.iter().any(|e| contains_func(e, heap, seen)),
        _ => false,
    }
}

/// How a migrate evaluation fails: a genuine migrate panic keeps its
/// diagnostic (overflow, div-zero, OOB, failed totality-by-construct);
/// anything unreachable in checked programs is a loud refusal.
enum EvalFail {
    Panic(Diag),
    Refused(String),
}

impl EvalFail {
    fn refuse(reason: impl Into<String>) -> Self {
        Self::Refused(reason.into())
    }
}

/// Block execution signal: completion, early return, or loop control.
enum Flow {
    Val(RtVal),
    Return(RtVal),
    Break,
    Continue,
}

struct Ev<'h> {
    heap: &'h mut Heap,
    env: Vec<HashMap<String, RtVal>>,
}

impl<'h> Ev<'h> {
    fn lookup(&self, name: &str) -> Option<RtVal> {
        self.env.iter().rev().find_map(|s| s.get(name).cloned())
    }

    fn assign(&mut self, name: &str, v: RtVal) -> bool {
        for s in self.env.iter_mut().rev() {
            if s.contains_key(name) {
                s.insert(name.to_string(), v);
                return true;
            }
        }
        false
    }
}

/// Evaluate a migrate body over the old struct value (bound to
/// `param`). Pure snapshot semantics: fresh heap objects for the
/// result, never a mutation of old state. Operator and builtin
/// semantics mirror `ops.rs` / `builtins.rs` exactly (scalar ops
/// delegate through conversion; container ops are structural
/// mirrors — each cites its source).
fn eval_migrate(mig: &HMigrate, old_value: &RtVal, heap: &mut Heap) -> Result<RtVal, EvalFail> {
    let mut ev = Ev {
        heap,
        env: vec![HashMap::new()],
    };
    ev.env[0].insert(mig.param.clone(), old_value.clone());
    match exec_block(&mut ev, &mig.body)? {
        Flow::Val(v) => Ok(v),
        Flow::Return(v) => Ok(v),
        Flow::Break | Flow::Continue => Err(EvalFail::refuse(
            "internal: break/continue past migrate top level (unreachable in checked programs)",
        )),
    }
}

fn exec_block(ev: &mut Ev<'_>, b: &HBlock) -> Result<Flow, EvalFail> {
    // Block scope (mirrors interp): `let`s land here, the tail (if
    // any) evaluates inside it, and nothing leaks out. Early
    // return/break/continue propagate with the scope popped.
    ev.env.push(HashMap::new());
    for stmt in &b.stmts {
        match exec_stmt(ev, stmt)? {
            Flow::Val(_) => {}
            flow => {
                ev.env.pop();
                return Ok(flow);
            }
        }
    }
    let out = match &b.tail {
        Some(t) => Flow::Val(eval_expr(ev, t)?),
        None => Flow::Val(RtVal::Unit),
    };
    ev.env.pop();
    Ok(out)
}

fn exec_stmt(ev: &mut Ev<'_>, stmt: &toto_compiler::hir::HStmt) -> Result<Flow, EvalFail> {
    match &stmt.kind {
        HStmtKind::Let { name, value, .. } => {
            let v = eval_expr(ev, value)?;
            ev.env
                .last_mut()
                .expect("scope underflow")
                .insert(name.clone(), v);
            Ok(Flow::Val(RtVal::Unit))
        }
        HStmtKind::Assign { target, value } => {
            let v = eval_expr(ev, value)?;
            match target {
                HAssignTarget::Var(name, _, _) => {
                    if !ev.assign(name, v) {
                        return Err(EvalFail::refuse(format!(
                            "internal: assign to unbound `{name}` (unreachable in checked programs)"
                        )));
                    }
                    Ok(Flow::Val(RtVal::Unit))
                }
                HAssignTarget::Field(base, name, _) => {
                    let b = eval_expr(ev, base)?;
                    field_write_rt(ev.heap, &b, name, v, stmt.span)?;
                    Ok(Flow::Val(RtVal::Unit))
                }
                HAssignTarget::Index(base, index, _) => {
                    let b = eval_expr(ev, base)?;
                    let i = eval_expr(ev, index)?;
                    index_write_rt(ev.heap, &b, &i, v, stmt.span)?;
                    Ok(Flow::Val(RtVal::Unit))
                }
            }
        }
        HStmtKind::Expr(e) => {
            eval_expr(ev, e)?;
            Ok(Flow::Val(RtVal::Unit))
        }
        HStmtKind::Return(e) => Ok(Flow::Return(eval_expr(ev, e)?)),
        HStmtKind::Break => Ok(Flow::Break),
        HStmtKind::Continue => Ok(Flow::Continue),
        HStmtKind::While { cond, body } => {
            loop {
                match eval_expr(ev, cond)? {
                    RtVal::Bool(true) => {}
                    RtVal::Bool(false) => break,
                    other => {
                        return Err(EvalFail::refuse(format!(
                            "internal: non-bool while condition ({}) \
                             (unreachable in checked programs)",
                            other.type_name(ev.heap)
                        )));
                    }
                }
                match exec_block(ev, body)? {
                    Flow::Val(_) => {}
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    Flow::Break => break,
                    Flow::Continue => {}
                }
                // Tails: HIR while-bodies carry no tail (checker
                // form); nothing to evaluate per iteration.
            }
            Ok(Flow::Val(RtVal::Unit))
        }
        HStmtKind::For {
            name, iter, body, ..
        } => {
            let it = eval_expr(ev, iter)?;
            // Normalize both range forms (immediates and heap
            // payloads from `alloc_range`) and snapshot arrays
            // (mirrors interp: body push/pop cannot invalidate
            // iteration). Anything else panics E0207, as in interp.
            enum IterSrc {
                Range(i64, i64),
                Elems(Vec<RtVal>),
            }
            let src = match &it {
                RtVal::Range(lo, hi) => IterSrc::Range(*lo, *hi),
                RtVal::Heap(r) => match ev.heap.get(*r) {
                    Some(Payload::Array(elems)) => IterSrc::Elems(elems.clone()),
                    Some(Payload::Range { lo, hi }) => IterSrc::Range(*lo, *hi),
                    _ => {
                        return Err(EvalFail::Panic(Diag::new(
                            "E0207",
                            format!("cannot iterate over {}", it.type_name(ev.heap)),
                            stmt.span,
                            None,
                        )));
                    }
                },
                _ => {
                    return Err(EvalFail::Panic(Diag::new(
                        "E0207",
                        format!("cannot iterate over {}", it.type_name(ev.heap)),
                        stmt.span,
                        None,
                    )));
                }
            };
            // Element driver shared by ranges and arrays.
            let drive_elem = |ev: &mut Ev<'_>, elem: RtVal| -> Result<Flow, EvalFail> {
                ev.env.push(HashMap::new());
                ev.env
                    .last_mut()
                    .expect("scope underflow")
                    .insert(name.clone(), elem);
                let res = exec_block(ev, body);
                ev.env.pop();
                res
            };
            match src {
                IterSrc::Range(start, end) => {
                    // Exclusive end, saturating increment (interp).
                    let mut i = start;
                    while i < end {
                        match drive_elem(ev, RtVal::Int(i))? {
                            Flow::Val(_) => {}
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Break => break,
                            Flow::Continue => {}
                        }
                        i = i.saturating_add(1);
                        if i == end {
                            break;
                        }
                    }
                }
                IterSrc::Elems(elems) => {
                    for elem in elems {
                        match drive_elem(ev, elem)? {
                            Flow::Val(_) => {}
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Break => break,
                            Flow::Continue => {}
                        }
                    }
                }
            }
            Ok(Flow::Val(RtVal::Unit))
        }
        // Unreachable in checked programs (E0325 bans all three in
        // migrate bodies): loud refusal, never silent.
        HStmtKind::Go { .. } => Err(EvalFail::refuse(
            "internal: `go` in migrate (unreachable: E0325 bans it)",
        )),
        HStmtKind::Select { .. } => Err(EvalFail::refuse(
            "internal: `select` in migrate (unreachable: E0325 bans it)",
        )),
    }
}

fn eval_expr(ev: &mut Ev<'_>, e: &HExpr) -> Result<RtVal, EvalFail> {
    let span = e.span;
    match &e.kind {
        HExprKind::Int(n) => Ok(RtVal::Int(*n)),
        HExprKind::Float(f) => Ok(RtVal::Float(*f)),
        HExprKind::Str(s) => Ok(RtVal::Heap(ev.heap.alloc_str(s))),
        HExprKind::Bool(b) => Ok(RtVal::Bool(*b)),
        HExprKind::Unit => Ok(RtVal::Unit),
        HExprKind::None => Ok(RtVal::None),
        HExprKind::Var(name, _) => ev.lookup(name).ok_or_else(|| {
            EvalFail::refuse(format!(
                "internal: unbound `{name}` in migrate (unreachable in checked programs)"
            ))
        }),
        HExprKind::If {
            cond,
            then_b,
            else_b,
        } => {
            match eval_expr(ev, cond)? {
                RtVal::Bool(true) => match exec_block(ev, then_b)? {
                    Flow::Val(v) | Flow::Return(v) => Ok(v),
                    Flow::Break | Flow::Continue => Err(EvalFail::refuse(
                        "internal: break/continue past if in migrate (unreachable)",
                    )),
                },
                RtVal::Bool(false) => match else_b {
                    Some(b) => match exec_block(ev, b)? {
                        Flow::Val(v) | Flow::Return(v) => Ok(v),
                        Flow::Break | Flow::Continue => Err(EvalFail::refuse(
                            "internal: break/continue past if in migrate (unreachable)",
                        )),
                    },
                    // Statement-`if` without `else` yields unit
                    // (spec §5.5); expression-`if` requires `else`
                    // (checker-enforced).
                    None => Ok(RtVal::Unit),
                },
                other => Err(EvalFail::refuse(format!(
                    "internal: non-bool if condition ({}) (unreachable in checked programs)",
                    other.type_name(ev.heap)
                ))),
            }
        }
        HExprKind::StructLit { name, fields, .. } => {
            let mut out = Vec::with_capacity(fields.len());
            for (fname, fexpr, _) in fields {
                out.push((fname.clone(), eval_expr(ev, fexpr)?));
            }
            Ok(RtVal::Heap(ev.heap.alloc_struct(name, out)))
        }
        HExprKind::Array(es) => {
            let mut out = Vec::with_capacity(es.len());
            for e in es {
                out.push(eval_expr(ev, e)?);
            }
            Ok(RtVal::Heap(ev.heap.alloc_array(out)))
        }
        HExprKind::Range(lo, hi) => {
            let l = eval_expr(ev, lo)?;
            let h = eval_expr(ev, hi)?;
            match (l, h) {
                // Mirrors interp `Range` exactly (E0203 otherwise).
                (RtVal::Int(a), RtVal::Int(b)) => Ok(RtVal::Range(a, b)),
                (a, b) => Err(EvalFail::Panic(Diag::new(
                    "E0203",
                    format!(
                        "range bounds must be int, got {} and {}",
                        a.type_name(ev.heap),
                        b.type_name(ev.heap)
                    ),
                    span,
                    None,
                ))),
            }
        }
        HExprKind::Binary { op, left, right } => {
            // Short-circuit first (mirrors interp eval order).
            if matches!(op, BinOp::And) {
                return match eval_expr(ev, left)? {
                    RtVal::Bool(false) => Ok(RtVal::Bool(false)),
                    RtVal::Bool(true) => match eval_expr(ev, right)? {
                        RtVal::Bool(b) => Ok(RtVal::Bool(b)),
                        other => Err(EvalFail::refuse(format!(
                            "internal: non-bool `and` operand ({}) (unreachable)",
                            other.type_name(ev.heap)
                        ))),
                    },
                    other => Err(EvalFail::refuse(format!(
                        "internal: non-bool `and` operand ({}) (unreachable)",
                        other.type_name(ev.heap)
                    ))),
                };
            }
            if matches!(op, BinOp::Or) {
                return match eval_expr(ev, left)? {
                    RtVal::Bool(true) => Ok(RtVal::Bool(true)),
                    RtVal::Bool(false) => match eval_expr(ev, right)? {
                        RtVal::Bool(b) => Ok(RtVal::Bool(b)),
                        other => Err(EvalFail::refuse(format!(
                            "internal: non-bool `or` operand ({}) (unreachable)",
                            other.type_name(ev.heap)
                        ))),
                    },
                    other => Err(EvalFail::refuse(format!(
                        "internal: non-bool `or` operand ({}) (unreachable)",
                        other.type_name(ev.heap)
                    ))),
                };
            }
            let l = eval_expr(ev, left)?;
            let r = eval_expr(ev, right)?;
            eval_binary(ev, *op, l, r, span)
        }
        HExprKind::Unary { op, expr } => {
            let v = eval_expr(ev, expr)?;
            // Delegates to ops.rs (checked neg, bool not); the inner
            // span is the operand's, exactly as in the interpreters.
            let inner = expr.span;
            match op {
                UnOp::Neg => {
                    let sv = to_scalar(&v, ev.heap)?;
                    scalar1(sv, ev.heap, inner, span, ops::neg_value)
                }
                UnOp::Not => {
                    // Mirrors ops::not_value exactly (span is the
                    // operand's, as in the interpreters).
                    match to_scalar(&v, ev.heap)? {
                        Value::Bool(b) => Ok(RtVal::Bool(!b)),
                        other => Err(EvalFail::Panic(Diag::new(
                            "E0203",
                            format!("`!` needs a bool, got {}", other.type_name()),
                            inner,
                            None,
                        ))),
                    }
                }
            }
        }
        HExprKind::CallBuiltin { name, args } => eval_builtin(ev, name, args, span),
        HExprKind::Field { base, name } => {
            let b = eval_expr(ev, base)?;
            field_read_rt(ev.heap, &b, name, span)
        }
        HExprKind::Index { base, index } => {
            let b = eval_expr(ev, base)?;
            let i = eval_expr(ev, index)?;
            index_read_rt(ev.heap, &b, &i, span)
        }
        HExprKind::Ok(e) => Ok(RtVal::Ok(Box::new(eval_expr(ev, e)?))),
        HExprKind::Err(e) => Ok(RtVal::Err(Box::new(eval_expr(ev, e)?))),
        HExprKind::Some(e) => Ok(RtVal::Some(Box::new(eval_expr(ev, e)?))),
        // Unreachable in checked migrate bodies (E0325): loud
        // refusal. `Closure` is checked-legal but not preservable:
        // its body would need a fresh namespace in the new
        // generation's tables, which does not exist — the whitelist
        // is channels + plain data (spec §14.3), so a closure in a
        // migration result (or anywhere evaluated) aborts loudly.
        HExprKind::Call { .. } => Err(EvalFail::refuse(
            "internal: user call in migrate (unreachable: E0325 bans it)",
        )),
        HExprKind::CallForeign { name, .. } => Err(EvalFail::refuse(format!(
            "internal: foreign call `{name}` in migrate (unreachable: E0325 bans it)"
        ))),
        HExprKind::Send { .. } | HExprKind::Recv { .. } | HExprKind::ChanNew { .. } => Err(
            EvalFail::refuse("internal: channel op in migrate (unreachable: E0325 bans it)"),
        ),
        HExprKind::Question { .. } => Err(EvalFail::refuse(
            "internal: `?` in migrate (unreachable: E0325/E0308 ban it)",
        )),
        HExprKind::Closure { .. } => Err(EvalFail::refuse(
            "migrate creates a closure: not preservable (whitelist is channels + \
             plain data, spec §14.3) — loud atomic restart",
        )),
    }
}

/// `Binary` minus short-circuit: arithmetic/comparison/equality with
/// exact interpreter semantics. Scalar ops delegate to `ops.rs`
/// through conversion; equality/ordering on heap values use the
/// executor's structural rules (same-type gate → E0203).
fn eval_binary(
    ev: &mut Ev<'_>,
    op: BinOp,
    l: RtVal,
    r: RtVal,
    span: Span,
) -> Result<RtVal, EvalFail> {
    match op {
        // `Result`/`Option`/`Range` immediates never reach scalar ops
        // in checked code; anything non-scalar here is defensive.
        BinOp::Add => {
            let (a, b) = (to_scalar(&l, ev.heap)?, to_scalar(&r, ev.heap)?);
            scalar2(a, b, ev.heap, span, "add", ops::arith, |x, y| {
                x.checked_add(y)
            })
        }
        BinOp::Sub => {
            let (a, b) = (to_scalar(&l, ev.heap)?, to_scalar(&r, ev.heap)?);
            scalar2(a, b, ev.heap, span, "subtract", ops::arith, |x, y| {
                x.checked_sub(y)
            })
        }
        BinOp::Mul => {
            let (a, b) = (to_scalar(&l, ev.heap)?, to_scalar(&r, ev.heap)?);
            scalar2(a, b, ev.heap, span, "multiply", ops::arith, |x, y| {
                x.checked_mul(y)
            })
        }
        BinOp::Div => {
            let (a, b) = (to_scalar(&l, ev.heap)?, to_scalar(&r, ev.heap)?);
            scalar2_op(a, b, ev.heap, span, ops::div_values)
        }
        BinOp::Mod => {
            let (a, b) = (to_scalar(&l, ev.heap)?, to_scalar(&r, ev.heap)?);
            scalar2_op(a, b, ev.heap, span, ops::mod_values)
        }
        BinOp::Eq => rt_eq(ev.heap, &l, &r, span).map(RtVal::Bool),
        BinOp::NotEq => rt_eq(ev.heap, &l, &r, span).map(|b| RtVal::Bool(!b)),
        BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
            rt_order(ev.heap, op, &l, &r, span).map(RtVal::Bool)
        }
        BinOp::Range => {
            // `a..b` as an operator (same rule as the Range expr).
            match (&l, &r) {
                (RtVal::Int(a), RtVal::Int(b)) => Ok(RtVal::Range(*a, *b)),
                _ => Err(EvalFail::Panic(Diag::new(
                    "E0203",
                    format!(
                        "range bounds must be int, got {} and {}",
                        l.type_name(ev.heap),
                        r.type_name(ev.heap)
                    ),
                    span,
                    None,
                ))),
            }
        }
        BinOp::And | BinOp::Or => Err(EvalFail::refuse(
            "internal: short-circuit op reached eval_binary (unreachable)",
        )),
    }
}

/// Scalars convert exactly (ints, floats, bools, strings, unit,
/// none, ranges). Anything else is unreachable in a scalar-op
/// position in checked code.
fn to_scalar(v: &RtVal, heap: &Heap) -> Result<Value, EvalFail> {
    match v {
        RtVal::Int(n) => Ok(Value::Int(*n)),
        RtVal::Float(f) => Ok(Value::Float(*f)),
        RtVal::Bool(b) => Ok(Value::Bool(*b)),
        RtVal::Unit => Ok(Value::Unit),
        RtVal::None => Ok(Value::None),
        RtVal::Range(a, b) => Ok(Value::Range(*a, *b)),
        RtVal::Heap(r) => match heap.get(*r) {
            Some(Payload::Str(s)) => Ok(Value::Str(std::rc::Rc::new(s.clone()))),
            _ => Err(EvalFail::refuse(format!(
                "internal: non-scalar in scalar op ({}) (unreachable in checked programs)",
                v.type_name(heap)
            ))),
        },
        _ => Err(EvalFail::refuse(format!(
            "internal: non-scalar in scalar op ({}) (unreachable in checked programs)",
            v.type_name(heap)
        ))),
    }
}

/// Scalar results convert back exactly. Container results are
/// unreachable from scalar ops; anything else is a bug.
fn from_scalar(v: Value, heap: &mut Heap, span: Span) -> Result<RtVal, EvalFail> {
    match v {
        Value::Int(n) => Ok(RtVal::Int(n)),
        Value::Float(f) => Ok(RtVal::Float(f)),
        Value::Bool(b) => Ok(RtVal::Bool(b)),
        Value::Unit => Ok(RtVal::Unit),
        Value::None => Ok(RtVal::None),
        Value::Range(a, b) => Ok(RtVal::Range(a, b)),
        Value::Str(s) => Ok(RtVal::Heap(heap.alloc_str(&s))),
        _ => Err(EvalFail::refuse(format!(
            "internal: non-scalar op result at {} (unreachable)",
            span.start
        ))),
    }
}

/// Unary scalar op through `ops.rs` (checked neg, bool not).
fn scalar1(
    v: Value,
    heap: &mut Heap,
    inner_span: Span,
    span: Span,
    f: fn(Value, Span, Span) -> ops::OpResult<Value>,
) -> Result<RtVal, EvalFail> {
    f(v, inner_span, span)
        .map_err(|(d, _)| EvalFail::Panic(d))
        .and_then(|o| from_scalar(o, heap, span))
}

/// `ops::arith`-shaped binary scalar op.
type ArithFn =
    fn(Value, Value, Span, &'static str, fn(i64, i64) -> Option<i64>) -> ops::OpResult<Value>;

/// Binary scalar op through `ops::arith` (checked arithmetic, string
/// concat on `add`, E0203 cross-type with the int/float hint —
/// exactly as in the interpreters).
fn scalar2(
    a: Value,
    b: Value,
    heap: &mut Heap,
    span: Span,
    verb: &'static str,
    f: ArithFn,
    op: fn(i64, i64) -> Option<i64>,
) -> Result<RtVal, EvalFail> {
    f(a, b, span, verb, op)
        .map_err(|(d, _)| EvalFail::Panic(d))
        .and_then(|o| from_scalar(o, heap, span))
}

/// Binary scalar op with its own signature (`div_values`,
/// `mod_values`, `ordering`).
fn scalar2_op(
    a: Value,
    b: Value,
    heap: &mut Heap,
    span: Span,
    f: fn(Value, Value, Span) -> ops::OpResult<Value>,
) -> Result<RtVal, EvalFail> {
    f(a, b, span)
        .map_err(|(d, _)| EvalFail::Panic(d))
        .and_then(|o| from_scalar(o, heap, span))
}

/// Pure builtins in migrate bodies. The 16 scalar builtins delegate
/// to `builtins::call_builtin` through full conversion (exact for
/// plain data); `len`/`push`/`pop` are structural mirrors below.
/// Non-data arguments (channels, closures, functions — all lossy or
/// refused in conversion) abort loudly instead of corrupting.
fn eval_builtin(
    ev: &mut Ev<'_>,
    name: &str,
    args: &[HExpr],
    span: Span,
) -> Result<RtVal, EvalFail> {
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_expr(ev, a)?);
    }
    let mut seen = HashSet::new();
    for v in &vals {
        if contains_nondata(v, ev.heap, &mut seen) {
            return Err(EvalFail::refuse(format!(
                "migrate passes channels, closures, or functions through builtin \
                 `{name}`: not supported — loud atomic restart"
            )));
        }
    }
    match name {
        // Structural mirrors of builtins.rs (E0210 preserved).
        "len" => {
            if vals.len() != 1 {
                return Err(EvalFail::Panic(Diag::new(
                    "E0204",
                    format!(
                        "wrong number of arguments: `len` takes 1, got {}",
                        vals.len()
                    ),
                    span,
                    None,
                )));
            }
            match &vals[0] {
                RtVal::Heap(r) => match ev.heap.get(*r) {
                    Some(Payload::Str(s)) => Ok(RtVal::Int(s.chars().count() as i64)),
                    Some(Payload::Array(elems)) => Ok(RtVal::Int(elems.len() as i64)),
                    _ => Err(EvalFail::Panic(Diag::new(
                        "E0210",
                        format!(
                            "`len` needs a string or array, got {}",
                            vals[0].type_name(ev.heap)
                        ),
                        span,
                        None,
                    ))),
                },
                other => Err(EvalFail::Panic(Diag::new(
                    "E0210",
                    format!(
                        "`len` needs a string or array, got {}",
                        other.type_name(ev.heap)
                    ),
                    span,
                    None,
                ))),
            }
        }
        "push" => {
            if vals.len() != 2 {
                return Err(EvalFail::Panic(Diag::new(
                    "E0204",
                    format!(
                        "wrong number of arguments: `push` takes 2, got {}",
                        vals.len()
                    ),
                    span,
                    None,
                )));
            }
            // Mirrors builtins.rs `push` exactly: mutates the array,
            // returns the new length as Int.
            let elem = vals[1].clone();
            match &vals[0] {
                RtVal::Heap(r) => match ev.heap.get_mut(*r) {
                    Some(Payload::Array(elems)) => {
                        elems.push(elem);
                        Ok(RtVal::Int(elems.len() as i64))
                    }
                    _ => Err(EvalFail::Panic(Diag::new(
                        "E0210",
                        format!("`push` needs an array, got {}", vals[0].type_name(ev.heap)),
                        span,
                        None,
                    ))),
                },
                other => Err(EvalFail::Panic(Diag::new(
                    "E0210",
                    format!("`push` needs an array, got {}", other.type_name(ev.heap)),
                    span,
                    None,
                ))),
            }
        }
        "pop" => {
            if vals.len() != 1 {
                return Err(EvalFail::Panic(Diag::new(
                    "E0204",
                    format!(
                        "wrong number of arguments: `pop` takes 1, got {}",
                        vals.len()
                    ),
                    span,
                    None,
                )));
            }
            match &vals[0] {
                RtVal::Heap(r) => match ev.heap.get_mut(*r) {
                    Some(Payload::Array(elems)) => match elems.pop() {
                        Some(v) => Ok(v),
                        None => Err(EvalFail::Panic(Diag::new(
                            "E0210",
                            "pop from empty array".to_string(),
                            span,
                            None,
                        ))),
                    },
                    _ => Err(EvalFail::Panic(Diag::new(
                        "E0210",
                        format!("`pop` needs an array, got {}", vals[0].type_name(ev.heap)),
                        span,
                        None,
                    ))),
                },
                other => Err(EvalFail::Panic(Diag::new(
                    "E0210",
                    format!("`pop` needs an array, got {}", other.type_name(ev.heap)),
                    span,
                    None,
                ))),
            }
        }
        _ => {
            // Scalar builtins delegate wholesale (arity, coercions,
            // and E0210 preserved by construction).
            let mut cargs = Vec::with_capacity(vals.len());
            for v in &vals {
                cargs.push(conv_to_value(v, ev.heap, span)?);
            }
            let mut output = Vec::new();
            // Function values never reach here (the pre-scan above
            // aborts them): display names are unreachable, so the
            // empty table matches the no-function case exactly.
            let out = builtins::call_builtin(name, cargs, span, &[], &mut output)
                .map_err(|(d, _)| EvalFail::Panic(d))?;
            if !output.is_empty() {
                // Unreachable (print is E0325-banned): never swallow.
                return Err(EvalFail::refuse(
                    "internal: builtin produced output in migrate (unreachable)",
                ));
            }
            conv_from_value(&out, ev.heap, span)
        }
    }
}

/// True iff `v` holds a channel, closure, or function anywhere
/// (through heap containers, wrappers, and captures). Those are
/// lossy-or-refused in conversion, so builtins over them abort.
fn contains_nondata(v: &RtVal, heap: &Heap, seen: &mut HashSet<toto_runtime::heap::GcRef>) -> bool {
    match v {
        RtVal::Chan(_) | RtVal::Func(_) | RtVal::Closure(_) => true,
        RtVal::Heap(r) => {
            if !seen.insert(*r) {
                return false;
            }
            match heap.get(*r) {
                Some(Payload::Array(elems)) => {
                    elems.iter().any(|e| contains_nondata(e, heap, seen))
                }
                Some(Payload::Struct { fields, .. }) => {
                    fields.iter().any(|(_, e)| contains_nondata(e, heap, seen))
                }
                Some(Payload::Result { inner, .. }) | Some(Payload::Option { inner, .. }) => {
                    contains_nondata(inner, heap, seen)
                }
                Some(Payload::Closure { captures, .. }) => {
                    captures.iter().any(|e| contains_nondata(e, heap, seen))
                }
                _ => false,
            }
        }
        RtVal::Ok(inner) | RtVal::Err(inner) | RtVal::Some(inner) => {
            contains_nondata(inner, heap, seen)
        }
        _ => false,
    }
}

/// Full conversion for delegated builtins (plain data only — the
/// caller pre-scanned for channels/closures/functions).
fn conv_to_value(v: &RtVal, heap: &Heap, span: Span) -> Result<Value, EvalFail> {
    toto_runtime::conv::to_value(v, heap).map_err(|_| {
        EvalFail::refuse(format!(
            "internal: builtin arg not convertible at {} (unreachable: pre-scanned)",
            span.start
        ))
    })
}

/// Full conversion back (fresh heap objects — snapshot semantics).
fn conv_from_value(v: &Value, heap: &mut Heap, span: Span) -> Result<RtVal, EvalFail> {
    toto_runtime::conv::from_value(v, heap).map_err(|_| {
        EvalFail::refuse(format!(
            "internal: builtin result not convertible at {} \
             (function values through builtins are not supported)",
            span.start
        ))
    })
}

/// Field read mirroring `ops::field_read` (E0209 preserved).
fn field_read_rt(heap: &Heap, base: &RtVal, name: &str, span: Span) -> Result<RtVal, EvalFail> {
    match base {
        RtVal::Heap(r) => match heap.get(*r) {
            Some(Payload::Struct { fields, .. }) => fields
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| {
                    let sname = match heap.get(*r) {
                        Some(Payload::Struct { name, .. }) => name.clone(),
                        _ => String::from("struct"),
                    };
                    EvalFail::Panic(Diag::new(
                        "E0209",
                        format!("struct {sname} has no field `{name}`"),
                        span,
                        None,
                    ))
                }),
            _ => Err(EvalFail::Panic(Diag::new(
                "E0209",
                format!("cannot access field of {}", base.type_name(heap)),
                span,
                None,
            ))),
        },
        _ => Err(EvalFail::Panic(Diag::new(
            "E0209",
            format!("cannot access field of {}", base.type_name(heap)),
            span,
            None,
        ))),
    }
}

/// Field write mirroring `ops::field_write` (E0209 preserved).
fn field_write_rt(
    heap: &mut Heap,
    base: &RtVal,
    name: &str,
    value: RtVal,
    span: Span,
) -> Result<(), EvalFail> {
    match base {
        RtVal::Heap(r) => match heap.get_mut(*r) {
            Some(Payload::Struct { fields, .. }) => {
                match fields.iter_mut().find(|(n, _)| n == name) {
                    Some((_, slot)) => {
                        *slot = value;
                        Ok(())
                    }
                    None => {
                        let sname = match heap.get(*r) {
                            Some(Payload::Struct { name, .. }) => name.clone(),
                            _ => String::from("struct"),
                        };
                        Err(EvalFail::Panic(Diag::new(
                            "E0209",
                            format!("struct {sname} has no field `{name}`"),
                            span,
                            None,
                        )))
                    }
                }
            }
            _ => Err(EvalFail::Panic(Diag::new(
                "E0209",
                format!("cannot assign field on {}", base.type_name(heap)),
                span,
                None,
            ))),
        },
        _ => Err(EvalFail::Panic(Diag::new(
            "E0209",
            format!("cannot assign field on {}", base.type_name(heap)),
            span,
            None,
        ))),
    }
}

/// Index read mirroring `ops::index_read` (E0209 preserved; strings
/// are not indexable — same as the interpreters).
fn index_read_rt(heap: &Heap, base: &RtVal, index: &RtVal, span: Span) -> Result<RtVal, EvalFail> {
    match (base, index) {
        (RtVal::Heap(r), RtVal::Int(n)) => match heap.get(*r) {
            Some(Payload::Array(elems)) => {
                if *n < 0 || (*n as usize) >= elems.len() {
                    return Err(EvalFail::Panic(Diag::new(
                        "E0209",
                        format!(
                            "index {n} out of bounds for array of length {}",
                            elems.len()
                        ),
                        span,
                        None,
                    )));
                }
                Ok(elems[*n as usize].clone())
            }
            _ => Err(EvalFail::Panic(Diag::new(
                "E0209",
                format!("cannot index {}", base.type_name(heap)),
                span,
                None,
            ))),
        },
        (RtVal::Heap(_), other) => Err(EvalFail::Panic(Diag::new(
            "E0209",
            format!("array index must be int, got {}", other.type_name(heap)),
            span,
            None,
        ))),
        (other, _) => Err(EvalFail::Panic(Diag::new(
            "E0209",
            format!("cannot index {}", other.type_name(heap)),
            span,
            None,
        ))),
    }
}

/// Index write mirroring `ops::index_write` (E0209 preserved).
fn index_write_rt(
    heap: &mut Heap,
    base: &RtVal,
    index: &RtVal,
    value: RtVal,
    span: Span,
) -> Result<(), EvalFail> {
    match (base, index) {
        (RtVal::Heap(r), RtVal::Int(n)) => match heap.get_mut(*r) {
            Some(Payload::Array(elems)) => {
                if *n < 0 || (*n as usize) >= elems.len() {
                    return Err(EvalFail::Panic(Diag::new(
                        "E0209",
                        format!(
                            "index {n} out of bounds for array of length {}",
                            elems.len()
                        ),
                        span,
                        None,
                    )));
                }
                elems[*n as usize] = value;
                Ok(())
            }
            _ => Err(EvalFail::Panic(Diag::new(
                "E0209",
                format!("cannot index {}", base.type_name(heap)),
                span,
                None,
            ))),
        },
        (RtVal::Heap(_), other) => Err(EvalFail::Panic(Diag::new(
            "E0209",
            format!("array index must be int, got {}", other.type_name(heap)),
            span,
            None,
        ))),
        (other, _) => Err(EvalFail::Panic(Diag::new(
            "E0209",
            format!("cannot index {}", other.type_name(heap)),
            span,
            None,
        ))),
    }
}

/// `==` with the exact executor rule: same-type gate first (E0203
/// on mismatch — struct names must match, closures never match
/// `Func`, `Ok`/`Err` and `Some`/`None` cross-match), then deep
/// structural comparison with the depth-100 identity fallback.
fn rt_eq(heap: &Heap, a: &RtVal, b: &RtVal, span: Span) -> Result<bool, EvalFail> {
    if !eq_same_type(heap, a, b) {
        return Err(EvalFail::Panic(Diag::new(
            "E0203",
            format!(
                "cannot compare {} with {}",
                a.type_name(heap),
                b.type_name(heap)
            ),
            span,
            None,
        )));
    }
    Ok(rt_values_equal(heap, a, b, 0))
}

fn eq_same_type(heap: &Heap, a: &RtVal, b: &RtVal) -> bool {
    match (a, b) {
        (RtVal::Heap(r), RtVal::Heap(s)) => match (heap.get(*r), heap.get(*s)) {
            (Some(Payload::Struct { name: xn, .. }), Some(Payload::Struct { name: yn, .. })) => {
                xn == yn
            }
            (Some(x), Some(y)) => payload_kind(x) == payload_kind(y),
            _ => false,
        },
        (RtVal::Ok(_), RtVal::Ok(_))
        | (RtVal::Ok(_), RtVal::Err(_))
        | (RtVal::Err(_), RtVal::Ok(_))
        | (RtVal::Err(_), RtVal::Err(_)) => true,
        (RtVal::Some(_), RtVal::Some(_))
        | (RtVal::Some(_), RtVal::None)
        | (RtVal::None, RtVal::Some(_)) => true,
        (RtVal::None, RtVal::None) => true,
        // Pointers cross-match `None` (null compares equal to it —
        // mirrors the executor; the value rule checks the address).
        (RtVal::Ptr(_), RtVal::None) | (RtVal::None, RtVal::Ptr(_)) => true,
        // Ranges have two representations (immediate and heap
        // payload): they compare across forms by bounds.
        (RtVal::Range(..), RtVal::Heap(r)) | (RtVal::Heap(r), RtVal::Range(..)) => {
            matches!(heap.get(*r), Some(Payload::Range { .. }))
        }
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

fn payload_kind(p: &Payload) -> u8 {
    match p {
        Payload::Str(_) => 0,
        Payload::Array(_) => 1,
        Payload::Struct { .. } => 2,
        Payload::Range { .. } => 3,
        Payload::Result { .. } => 4,
        Payload::Option { .. } => 5,
        Payload::Closure { .. } => 6,
    }
}

fn rt_values_equal(heap: &Heap, a: &RtVal, b: &RtVal, depth: usize) -> bool {
    // Cycle guard mirrors the executor: identity fallback past
    // depth 100 (arrays/structs by handle, closures/funcs by word).
    if depth > 100 {
        return match (a, b) {
            (RtVal::Heap(r), RtVal::Heap(s)) => match (heap.get(*r), heap.get(*s)) {
                (Some(Payload::Array(_)), Some(Payload::Array(_)))
                | (Some(Payload::Struct { .. }), Some(Payload::Struct { .. })) => r == s,
                _ => false,
            },
            (RtVal::Closure(x), RtVal::Closure(y)) => std::sync::Arc::ptr_eq(x, y),
            (RtVal::Func(x), RtVal::Func(y)) => x == y,
            _ => false,
        };
    }
    match (a, b) {
        (RtVal::Int(x), RtVal::Int(y)) => x == y,
        // NaN compares false (bitwise, exactly as the executor).
        (RtVal::Float(x), RtVal::Float(y)) => x.to_bits() == y.to_bits(),
        (RtVal::Bool(x), RtVal::Bool(y)) => x == y,
        (RtVal::Unit, RtVal::Unit) => true,
        (RtVal::Heap(r), RtVal::Heap(s)) => {
            if r == s {
                return true;
            }
            match (heap.get(*r), heap.get(*s)) {
                (Some(Payload::Str(x)), Some(Payload::Str(y))) => x == y,
                (Some(Payload::Array(x)), Some(Payload::Array(y))) => {
                    x.len() == y.len()
                        && x.iter()
                            .zip(y.iter())
                            .all(|(e, f)| rt_values_equal(heap, e, f, depth + 1))
                }
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
                            n == m && rt_values_equal(heap, e, f, depth + 1)
                        })
                }
                (
                    Some(Payload::Range { lo: a0, hi: a1 }),
                    Some(Payload::Range { lo: b0, hi: b1 }),
                ) => a0 == b0 && a1 == b1,
                (
                    Some(Payload::Result { is_ok: x, inner: e }),
                    Some(Payload::Result { is_ok: y, inner: f }),
                ) => x == y && rt_values_equal(heap, e, f, depth + 1),
                (Some(Payload::Option { inner: e }), Some(Payload::Option { inner: f })) => {
                    rt_values_equal(heap, e, f, depth + 1)
                }
                (
                    Some(Payload::Closure { body: x, .. }),
                    Some(Payload::Closure { body: y, .. }),
                ) => x == y,
                _ => false,
            }
        }
        (RtVal::Range(a, b), RtVal::Range(c, d)) => a == c && b == d,
        // Cross-form ranges compare by bounds.
        (RtVal::Range(a, b), RtVal::Heap(r)) => match heap.get(*r) {
            Some(Payload::Range { lo, hi }) => a == lo && b == hi,
            _ => false,
        },
        (RtVal::Heap(r), RtVal::Range(a, b)) => match heap.get(*r) {
            Some(Payload::Range { lo, hi }) => a == lo && b == hi,
            _ => false,
        },
        // Channels compare equal (the executor quirk, preserved).
        (RtVal::Chan(_), RtVal::Chan(_)) => true,
        (RtVal::Func(x), RtVal::Func(y)) => x == y,
        (RtVal::Closure(x), RtVal::Closure(y)) => std::sync::Arc::ptr_eq(x, y),
        (RtVal::Ok(x), RtVal::Ok(y)) | (RtVal::Err(x), RtVal::Err(y)) => {
            rt_values_equal(heap, x, y, depth + 1)
        }
        (RtVal::Ok(_), RtVal::Err(_)) | (RtVal::Err(_), RtVal::Ok(_)) => false,
        (RtVal::Some(x), RtVal::Some(y)) => rt_values_equal(heap, x, y, depth + 1),
        (RtVal::None, RtVal::None) => true,
        (RtVal::Some(_), RtVal::None) | (RtVal::None, RtVal::Some(_)) => false,
        (RtVal::Ptr(x), RtVal::Ptr(y)) => x == y,
        (RtVal::Ptr(p), RtVal::None) | (RtVal::None, RtVal::Ptr(p)) => *p == 0,
        _ => false,
    }
}

/// `<`/`<=`/`>`/`>=` with the exact executor rule
/// (`toto_values_order`): int, float (NaN compares false), and
/// string order; channels compare equal; anything else is E0203.
fn rt_order(heap: &Heap, op: BinOp, a: &RtVal, b: &RtVal, span: Span) -> Result<bool, EvalFail> {
    let ord = match (a, b) {
        (RtVal::Int(x), RtVal::Int(y)) => x.cmp(y),
        (RtVal::Float(x), RtVal::Float(y)) => match x.partial_cmp(y) {
            Some(o) => o,
            None => return Ok(false),
        },
        (RtVal::Heap(r), RtVal::Heap(s)) => match (heap.get(*r), heap.get(*s)) {
            (Some(Payload::Str(x)), Some(Payload::Str(y))) => x.cmp(y),
            _ => {
                return Err(EvalFail::Panic(Diag::new(
                    "E0203",
                    format!(
                        "cannot order {} and {}",
                        a.type_name(heap),
                        b.type_name(heap)
                    ),
                    span,
                    None,
                )));
            }
        },
        (RtVal::Chan(_), RtVal::Chan(_)) => std::cmp::Ordering::Equal,
        _ => {
            return Err(EvalFail::Panic(Diag::new(
                "E0203",
                format!(
                    "cannot order {} and {}",
                    a.type_name(heap),
                    b.type_name(heap)
                ),
                span,
                None,
            )));
        }
    };
    Ok(match op {
        BinOp::Lt => ord == std::cmp::Ordering::Less,
        BinOp::LtEq => ord != std::cmp::Ordering::Greater,
        BinOp::Gt => ord == std::cmp::Ordering::Greater,
        BinOp::GtEq => ord != std::cmp::Ordering::Less,
        _ => {
            return Err(EvalFail::refuse(
                "internal: non-ordering op in rt_order (unreachable)",
            ));
        }
    })
}

#[cfg(test)]
mod preserve_tests {
    use super::*;

    fn checked(src: &str) -> CheckedProgram {
        toto_compiler::parse_and_check(src)
            .map(|(_, c)| c)
            .expect("test program checks")
    }

    /// Old/new programs sharing one state binding; bodies differ.
    fn plain_pair() -> (CheckedProgram, CheckedProgram) {
        let old = checked(
            "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n",
        );
        let new = checked(
            "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count + 1);\n    return 0;\n}\n",
        );
        (old, new)
    }

    #[test]
    fn body_only_change_carries_verbatim() {
        let (old, new) = plain_pair();
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let carried = plan_carry(&old, &[RtVal::Int(41)], &mut heap, &table, &new)
            .expect("body-only change preserves state");
        assert_eq!(carried, vec![RtVal::Int(41)]);
    }

    #[test]
    fn initializer_change_does_not_rerun() {
        // Row 5 (global/state initializer): same names and types, new
        // init code — the old VALUE carries; initializers never rerun
        // on the preserving path.
        let old =
            checked("state {\n    count: int = 0;\n}\nfn main() -> int {\n    return 0;\n}\n");
        let new =
            checked("state {\n    count: int = 999;\n}\nfn main() -> int {\n    return 0;\n}\n");
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let carried = plan_carry(&old, &[RtVal::Int(7)], &mut heap, &table, &new)
            .expect("init change preserves the old value");
        assert_eq!(carried, vec![RtVal::Int(7)]);
    }

    #[test]
    fn added_binding_aborts_loudly() {
        let (old, _) = plain_pair();
        let new = checked(
            "state {\n    count: int = 0;\n    extra: int = 1;\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let err = plan_carry(&old, &[RtVal::Int(1)], &mut heap, &table, &new).expect_err("added");
        assert!(err.reason.contains("added"), "{}", err.reason);
    }

    #[test]
    fn removed_binding_aborts_loudly() {
        let (_, new) = plain_pair();
        let old = checked(
            "state {\n    count: int = 0;\n    extra: int = 1;\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let err = plan_carry(
            &old,
            &[RtVal::Int(1), RtVal::Int(2)],
            &mut heap,
            &table,
            &new,
        )
        .expect_err("removed");
        assert!(err.reason.contains("removed"), "{}", err.reason);
    }

    fn old_cfg(heap: &mut Heap) -> RtVal {
        let host = RtVal::Heap(heap.alloc_str("x"));
        RtVal::Heap(heap.alloc_struct(
            "Cfg",
            vec![
                ("host".to_string(), host),
                ("port".to_string(), RtVal::Int(3)),
            ],
        ))
    }

    #[test]
    fn layout_change_with_migrate_runs() {
        // Field TYPE change (old port:int → new port:string) with a
        // same-shape passthrough: the migrate checks (against the new
        // types) and runs, but output validation catches the
        // wrongly-typed field BEFORE it touches state — loud atomic
        // restart, never silent reinterpretation.
        let old = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\", port: 1 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    host: string,\n    port: string,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    return Cfg { host: old.host, port: old.port };\n}\nstate {\n    cfg: Cfg = Cfg { host: \"y\", port: \"0\" };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let err = plan_carry(&old, &[old_cfg(&mut heap)], &mut heap, &table, &new)
            .expect_err("validation aborts the mistyped field");
        assert!(err.reason.contains("port"), "{}", err.reason);
        assert!(err.reason.contains("String"), "{}", err.reason);
    }

    #[test]
    fn removal_with_identity_migrate_carries() {
        // Field REMOVAL with the identity migrate (`return old`,
        // trivially total): the old value carries verbatim — the
        // dropped data is preserved inertly, never silently dropped
        // (new code cannot name the removed field, so it is
        // unreachable). Non-identity removals cannot name the
        // dropped field (E0310) and restart loudly instead.
        let old = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n    dropped: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\", port: 1, dropped: 9 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    return old;\n}\nstate {\n    cfg: Cfg = Cfg { host: \"y\", port: 2 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let host = RtVal::Heap(heap.alloc_str("x"));
        let old_v = RtVal::Heap(heap.alloc_struct(
            "Cfg",
            vec![
                ("host".to_string(), host),
                ("port".to_string(), RtVal::Int(3)),
                ("dropped".to_string(), RtVal::Int(9)),
            ],
        ));
        let table = ChannelTable::new();
        let carried =
            plan_carry(&old, &[old_v], &mut heap, &table, &new).expect("identity carries");
        assert_eq!(carried.len(), 1);
        match &carried[0] {
            RtVal::Heap(r) => match heap.get(*r) {
                Some(Payload::Struct { fields, .. }) => {
                    let get = |n: &str| fields.iter().find(|(f, _)| f == n).map(|(_, v)| v.clone());
                    assert_eq!(get("host").map(|v| matches!(v, RtVal::Heap(_))), Some(true));
                    assert_eq!(get("port"), Some(RtVal::Int(3)));
                    assert_eq!(get("dropped"), Some(RtVal::Int(9)));
                }
                other => panic!("struct, got {other:?}"),
            },
            other => panic!("heap, got {other:?}"),
        }
    }

    #[test]
    fn layout_change_without_migrate_aborts() {
        let old = checked(
            "struct Cfg {\n    host: string,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\" };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"y\", port: 1 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let host = RtVal::Heap(heap.alloc_str("x"));
        let old_v = RtVal::Heap(heap.alloc_struct("Cfg", vec![("host".to_string(), host)]));
        let table = ChannelTable::new();
        let err = plan_carry(&old, &[old_v], &mut heap, &table, &new).expect_err("needs migrate");
        assert!(err.reason.contains("no migrate"), "{}", err.reason);
    }

    #[test]
    fn migrate_panic_aborts_loudly() {
        // Overflow inside a migrate is a genuine panic (E0201 kept):
        // loud atomic restart, reported never swallowed. The added
        // field has no old data, so its E0326-mandated read sits in
        // a never-taken branch (acknowledged but unevaluated) while
        // the value itself is explicit.
        let old = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\", port: 1 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n    big: int,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    let probe = if false { old.big } else { 0 };\n    return Cfg { host: old.host, port: 9223372036854775807 + old.port + probe, big: 0 };\n}\nstate {\n    cfg: Cfg = Cfg { host: \"y\", port: 2, big: 0 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let err = plan_carry(&old, &[old_cfg(&mut heap)], &mut heap, &table, &new)
            .expect_err("overflow aborts");
        assert!(err.reason.contains("E0201"), "{}", err.reason);
    }

    #[test]
    fn reload_totality_checks_old_fields() {
        // Old struct has an extra field the migrate never reads:
        // check-time totality passed (against the NEW def), but
        // reload-time totality against the OLD def aborts.
        let old = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n    dropped: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\", port: 1, dropped: 9 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    host: string,\n    port: int,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    return Cfg { host: old.host, port: old.port };\n}\nstate {\n    cfg: Cfg = Cfg { host: \"y\", port: 2 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let host = RtVal::Heap(heap.alloc_str("x"));
        let old_v = RtVal::Heap(heap.alloc_struct(
            "Cfg",
            vec![
                ("host".to_string(), host),
                ("port".to_string(), RtVal::Int(1)),
                ("dropped".to_string(), RtVal::Int(9)),
            ],
        ));
        let table = ChannelTable::new();
        let err =
            plan_carry(&old, &[old_v], &mut heap, &table, &new).expect_err("not total over old");
        assert!(err.reason.contains("dropped"), "{}", err.reason);
    }

    #[test]
    fn closure_in_migrate_aborts() {
        let old = checked(
            "struct Cfg {\n    v: int,\n}\nstate {\n    cfg: Cfg = Cfg { v: 1 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    v: int,\n    w: int,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    let f = fn() -> int { return old.v; };\n    return Cfg { v: old.v, w: old.w };\n}\nstate {\n    cfg: Cfg = Cfg { v: 2, w: 3 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let old_v = RtVal::Heap(heap.alloc_struct("Cfg", vec![("v".to_string(), RtVal::Int(1))]));
        let table = ChannelTable::new();
        let err = plan_carry(&old, &[old_v], &mut heap, &table, &new).expect_err("closure aborts");
        assert!(err.reason.contains("closure"), "{}", err.reason);
    }

    #[test]
    fn chan_type_change_empty_queue_carries() {
        let old = checked("state {\n    ch: chan<int>;\n}\nfn main() -> int {\n    return 0;\n}\n");
        let new =
            checked("state {\n    ch: chan<string>;\n}\nfn main() -> int {\n    return 0;\n}\n");
        let mut heap = Heap::new();
        let mut table = ChannelTable::new();
        let id = table.create(Ty::Int, 0);
        let carried = plan_carry(&old, &[RtVal::Chan(id)], &mut heap, &table, &new)
            .expect("empty queue carries with the new type");
        assert_eq!(carried, vec![RtVal::Chan(id)]);
    }

    #[test]
    fn chan_type_change_queued_values_abort() {
        // Defense-only: post-settle state channels are unbuffered with
        // no waiters, so a queued value is unreachable by
        // construction — the abort is still asserted on a synthetic
        // table.
        let old = checked("state {\n    ch: chan<int>;\n}\nfn main() -> int {\n    return 0;\n}\n");
        let new =
            checked("state {\n    ch: chan<string>;\n}\nfn main() -> int {\n    return 0;\n}\n");
        let mut heap = Heap::new();
        let mut table = ChannelTable::new();
        let id = table.create(Ty::Int, 1);
        table
            .get_mut(id)
            .expect("channel")
            .buf
            .push_back(RtVal::Int(9));
        let err = plan_carry(&old, &[RtVal::Chan(id)], &mut heap, &table, &new)
            .expect_err("queued values abort");
        assert!(err.reason.contains("queued"), "{}", err.reason);
    }

    #[test]
    fn func_table_reorder_with_func_state_aborts() {
        // Signature row + Q4: a `Func` value in state with a changed
        // fn table aborts (indices would silently shift); identical
        // tables carry verbatim.
        let old_src = "state {\n    f: fn(int) -> int = id;\n}\nfn id(x: int) -> int {\n    return x;\n}\nfn main() -> int {\n    return 0;\n}\n";
        let same_src = "state {\n    f: fn(int) -> int = id;\n}\nfn id(x: int) -> int {\n    return x + 0;\n}\nfn main() -> int {\n    return 0;\n}\n";
        let reordered_src = "state {\n    f: fn(int) -> int = id;\n}\nfn other(x: int) -> int {\n    return x;\n}\nfn id(x: int) -> int {\n    return x;\n}\nfn main() -> int {\n    return 0;\n}\n";
        let old = checked(old_src);
        let same = checked(same_src);
        let reordered = checked(reordered_src);
        let mut heap = Heap::new();
        let table = ChannelTable::new();
        let carried = plan_carry(&old, &[RtVal::Func(0)], &mut heap, &table, &same)
            .expect("identical tables carry");
        assert_eq!(carried, vec![RtVal::Func(0)]);
        let err = plan_carry(&old, &[RtVal::Func(0)], &mut heap, &table, &reordered)
            .expect_err("reordered tables abort");
        assert!(err.reason.contains("function value"), "{}", err.reason);
    }

    #[test]
    fn migrate_exercises_pure_subset() {
        // Builtins, control flow, and containers through migrate:
        // split/upper/len/contains/if/while/for with an exact result.
        // Addition scenario: the added field (`n`) has no old data,
        // so its E0326-mandated read sits in a never-taken branch
        // (acknowledged but unevaluated) while the carried value is
        // fully explicit.
        let old = checked(
            "struct Cfg {\n    name: string,\n    tags: Array<string>,\n}\nstate {\n    cfg: Cfg = Cfg { name: \"\", tags: [] };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let new = checked(
            "struct Cfg {\n    name: string,\n    tags: Array<string>,\n    n: int,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    let probe = if false { old.n } else { 0 };\n    let parts = split(old.name, \",\");\n    let mut total = probe;\n    for p in parts {\n        total = total + len(p);\n    }\n    let mut acc = 0;\n    let mut i = 0;\n    while i < len(old.tags) {\n        acc = acc + 1;\n        i = i + 1;\n    }\n    let upper_name = upper(old.name);\n    let has_x = contains(upper_name, \"X\");\n    let suffix = if has_x { 100 } else { 0 };\n    return Cfg { name: upper_name, tags: old.tags, n: total + acc + suffix };\n}\nstate {\n    cfg: Cfg = Cfg { name: \"\", tags: [], n: 0 };\n}\nfn main() -> int {\n    return 0;\n}\n",
        );
        let mut heap = Heap::new();
        let name = RtVal::Heap(heap.alloc_str("a,X"));
        let p = RtVal::Heap(heap.alloc_str("p"));
        let qq = RtVal::Heap(heap.alloc_str("qq"));
        let tags = RtVal::Heap(heap.alloc_array(vec![p, qq]));
        let old_v = RtVal::Heap(heap.alloc_struct(
            "Cfg",
            vec![("name".to_string(), name), ("tags".to_string(), tags)],
        ));
        let table = ChannelTable::new();
        let carried =
            plan_carry(&old, &[old_v], &mut heap, &table, &new).expect("pure-subset migrate runs");
        // parts ["a","X"] give total 0+1+1=2; tags len 2 give acc 2;
        // name "a,X" uppers to "A,X" (contains X, suffix 100);
        // n = 2+2+100 = 104.
        match &carried[0] {
            RtVal::Heap(r) => match heap.get(*r) {
                Some(Payload::Struct {
                    name: sname,
                    fields,
                }) => {
                    assert_eq!(sname, "Cfg");
                    assert_eq!(fields.len(), 3);
                    let get = |n: &str| fields.iter().find(|(f, _)| f == n).map(|(_, v)| v.clone());
                    match get("name") {
                        Some(RtVal::Heap(s)) => match heap.get(s) {
                            Some(Payload::Str(st)) => assert_eq!(st, "A,X"),
                            other => panic!("name str, got {other:?}"),
                        },
                        other => panic!("name, got {other:?}"),
                    }
                    match get("tags") {
                        Some(RtVal::Heap(t)) => match heap.get(t) {
                            Some(Payload::Array(elems)) => assert_eq!(elems.len(), 2),
                            other => panic!("tags array, got {other:?}"),
                        },
                        other => panic!("tags, got {other:?}"),
                    }
                    assert_eq!(get("n"), Some(RtVal::Int(104)));
                }
                other => panic!("struct, got {other:?}"),
            },
            other => panic!("heap, got {other:?}"),
        }
    }
}
