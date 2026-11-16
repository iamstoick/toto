//! Scope + type checker (Phase 2, Stage 1): resolves names and types,
//! enforces the spec's type rules, and emits typed HIR. `toto run` checks
//! before interpreting; a program with type errors never executes.

use std::collections::HashMap;

use crate::ast::{BinOp, Block, Expr, Program, Stmt, Type, TypeKind, UnOp};
use crate::diag::Diag;
use crate::hir::{
    CheckedProgram, FnSig, HAssignTarget, HBlock, HExpr, HExprKind, HFn, HItem, HItemKind,
    HProgram, HSelectCase, HStmt, HStmtKind, HStructDecl, Ty, VarRef,
};
use crate::span::Span;

type CResult<T> = Result<T, ()>;

/// A scope binding: type, mutability, and defining depth (for capture).
#[derive(Clone)]
struct Binding {
    ty: Ty,
    mutable: bool,
    depth: u32,
    /// Set when the binding was moved into a channel send (spec §13.1):
    /// any later use is E0314.
    moved: bool,
}

pub struct Checker {
    structs: HashMap<String, HStructDecl>,
    fns: Vec<FnSig>,
    fn_index: HashMap<String, usize>,
    scopes: Vec<HashMap<String, Binding>>,
    depth: u32,
    errors: Vec<Diag>,
    /// Creation depths of enclosing `go` bodies (spec §13.2). Any use of
    /// an outer `mut` binding at or outside the innermost depth is E0314.
    go_stack: Vec<u32>,
    /// Set when the program uses concurrency; copied to CheckedProgram
    /// so the CLI can route it to the runtime executor.
    uses_concurrency: bool,
}

impl Checker {
    fn new() -> Self {
        Self {
            structs: HashMap::new(),
            fns: Vec::new(),
            fn_index: HashMap::new(),
            scopes: Vec::new(),
            depth: 0,
            errors: Vec::new(),
            go_stack: Vec::new(),
            uses_concurrency: false,
        }
    }

    fn error(&mut self, code: &'static str, message: String, span: Span, help: Option<String>) {
        self.errors.push(Diag::new(code, message, span, help));
    }

    fn resolve_ty(&mut self, ty: &Type) -> CResult<Ty> {
        match &ty.kind {
            TypeKind::Fn { params, ret } => {
                let mut ps = Vec::with_capacity(params.len());
                for p in params {
                    ps.push(self.resolve_ty(p)?);
                }
                Ok(Ty::Fn(ps, Box::new(self.resolve_ty(ret)?)))
            }
            TypeKind::Named { name, args } => {
                let arity = args.len();
                match name.as_str() {
                    "int" | "float" | "string" | "bool" | "unit" | "Range" => {
                        if arity != 0 {
                            self.error(
                                "E0302",
                                format!("type `{name}` takes no arguments, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        Ok(match name.as_str() {
                            "int" => Ty::Int,
                            "float" => Ty::Float,
                            "string" => Ty::String,
                            "bool" => Ty::Bool,
                            "unit" => Ty::Unit,
                            _ => Ty::Range,
                        })
                    }
                    "Array" => {
                        if arity != 1 {
                            self.error(
                                "E0302",
                                format!("type `Array` takes 1 argument, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        Ok(Ty::Array(Box::new(self.resolve_ty(&args[0])?)))
                    }
                    "Map" => {
                        if arity != 2 {
                            self.error(
                                "E0302",
                                format!("type `Map` takes 2 arguments, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        Ok(Ty::Map(
                            Box::new(self.resolve_ty(&args[0])?),
                            Box::new(self.resolve_ty(&args[1])?),
                        ))
                    }
                    "Result" => {
                        if arity != 2 {
                            self.error(
                                "E0302",
                                format!("type `Result` takes 2 arguments, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        Ok(Ty::Result(
                            Box::new(self.resolve_ty(&args[0])?),
                            Box::new(self.resolve_ty(&args[1])?),
                        ))
                    }
                    "Option" => {
                        if arity != 1 {
                            self.error(
                                "E0302",
                                format!("type `Option` takes 1 argument, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        Ok(Ty::Option(Box::new(self.resolve_ty(&args[0])?)))
                    }
                    "chan" => {
                        if arity != 1 {
                            self.error(
                                "E0302",
                                format!("type `chan` takes 1 argument, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        Ok(Ty::Chan(Box::new(self.resolve_ty(&args[0])?)))
                    }
                    _ => {
                        if !args.is_empty() {
                            self.error(
                                "E0302",
                                format!("type `{name}` takes no arguments, got {arity}"),
                                ty.span,
                                None,
                            );
                            return Err(());
                        }
                        if self.structs.contains_key(name) {
                            Ok(Ty::Struct(name.clone()))
                        } else {
                            self.error(
                                "E0302",
                                format!("unknown type `{name}`"),
                                ty.span,
                                Some(
                                    "declare a `struct` with this name, or fix the spelling"
                                        .to_string(),
                                ),
                            );
                            Err(())
                        }
                    }
                }
            }
        }
    }

    fn expect_ty(&mut self, found: &Ty, expected: &Ty, span: Span) -> CResult<()> {
        if found == expected {
            Ok(())
        } else {
            self.error(
                "E0303",
                format!("mismatched types: expected {expected}, found {found}"),
                span,
                None,
            );
            Err(())
        }
    }

    /// Whether the innermost binding of `name` was moved into a channel.
    fn is_moved(&self, name: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.get(name) {
                return b.moved;
            }
        }
        false
    }

    /// Mark the innermost binding of `name` as moved (spec §13.1).
    fn mark_moved(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(b) = scope.get_mut(name) {
                b.moved = true;
                return;
            }
        }
    }

    /// D3 (spec §13.1): values that are deeply immutable (scalars,
    /// strings, ranges, `None`-style empties, and containers thereof, plus
    /// channel handles, which the scheduler synchronizes) may be shared
    /// across tasks. Anything else must move on send.
    /// Deeply immutable = no in-place mutation path, so sharing across
    /// tasks cannot race. Scalars, strings (no string-mutation builtin),
    /// ranges, and channel handles (the intended sharing mechanism) qualify.
    /// `Array` NEVER qualifies — `push`/`pop`/index-store mutate in place —
    /// even `Array<int>`. `Option`/`Result` qualify iff their payloads do
    /// (wrappers have no mutation API). Structs have field stores.
    fn deeply_immutable(ty: &Ty) -> bool {
        match ty {
            Ty::Int | Ty::Float | Ty::String | Ty::Bool | Ty::Unit | Ty::Range => true,
            Ty::Chan(_) => true,
            Ty::Array(_) => false,
            Ty::Option(t) => Self::deeply_immutable(t),
            Ty::Result(t, e) => Self::deeply_immutable(t) && Self::deeply_immutable(e),
            Ty::Struct(_) | Ty::Fn(_, _) | Ty::Map(_, _) => false,
        }
    }

    /// Enforce move-on-send (spec §13.1): a send of a value that is not
    /// deeply immutable moves a whole binding (`ch <- x`); any other
    /// place (`a[i]`, `p.x`) holding such a value is rejected, since the
    /// sender would retain access. Fresh values (calls, literals,
    /// constructors) transfer ownership with nothing to mark.
    fn check_send_move(&mut self, hvalue: &HExpr, span: Span) -> CResult<()> {
        if Self::deeply_immutable(&hvalue.ty) {
            return Ok(());
        }
        match &hvalue.kind {
            HExprKind::Var(name, VarRef::Local { .. }) => {
                self.mark_moved(name);
                Ok(())
            }
            HExprKind::Var(_, VarRef::GlobalFn { .. }) => {
                // Named functions are immutable code: safe to share.
                Ok(())
            }
            HExprKind::Call { .. }
            | HExprKind::CallBuiltin { .. }
            | HExprKind::StructLit { .. }
            | HExprKind::Array(_)
            | HExprKind::Range(..)
            | HExprKind::Ok(_)
            | HExprKind::Err(_)
            | HExprKind::Some(_)
            | HExprKind::None
            | HExprKind::ChanNew { .. }
            | HExprKind::Int(_)
            | HExprKind::Float(_)
            | HExprKind::Str(_)
            | HExprKind::Bool(_)
            | HExprKind::Unit => Ok(()),
            _ => {
                // Field/index reads alias the container (shared by
                // reference, spec §5.7), so sending a non-deeply-immutable
                // one would share mutable state the sender keeps. Only
                // deeply immutable field values can be sent.
                self.error(
                    "E0314",
                    "cannot send shared mutable state from a field or index; only deeply immutable field values can be sent".to_string(),
                    span,
                    None,
                );
                Err(())
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<(Ty, VarRef)> {
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.get(name) {
                return Some((
                    b.ty.clone(),
                    VarRef::Local {
                        name: name.to_string(),
                        mutable: b.mutable,
                        depth: b.depth,
                    },
                ));
            }
        }
        None
    }
}

/// Compute a closure's capture list: the names bound at or outside the
/// closure creation depth that its body uses, with their types, in
/// deterministic first-use order. Binding depths on `VarRef::Local` make
/// shadowing automatic: anything bound inside the closure body (parameters,
/// `let`s, `for` variables — including nested closures' parameters) always
/// has a greater depth than the creation depth, so only true captures pass
/// the filter. Assignment targets carry no type, so they reuse the type of
/// the assigned value (which the checker already forced to match).
fn captured_vars(body: &HBlock, creation_depth: u32) -> Vec<(String, Ty)> {
    struct Walker {
        creation_depth: u32,
        found: Vec<(String, Ty)>,
    }
    impl Walker {
        fn note(&mut self, name: &str, ty: &Ty, depth: u32) {
            if depth <= self.creation_depth && !self.found.iter().any(|(n, _)| n == name) {
                self.found.push((name.to_string(), ty.clone()));
            }
        }
        fn walk_expr(&mut self, e: &HExpr) {
            match &e.kind {
                HExprKind::Var(name, varref) => {
                    if let VarRef::Local { depth, .. } = varref {
                        self.note(name, &e.ty, *depth);
                    }
                }
                HExprKind::If {
                    cond,
                    then_b,
                    else_b,
                } => {
                    self.walk_expr(cond);
                    self.walk_block(then_b);
                    if let Some(b) = else_b {
                        self.walk_block(b);
                    }
                }
                HExprKind::Call { callee, args } => {
                    self.walk_expr(callee);
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                HExprKind::CallBuiltin { args, .. } => {
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                // Nested closures are walked too: a name they use that is
                // bound outside the OUTER closure flows through it, so the
                // outer closure must also capture it. Their own parameters
                // have greater depths and never pass the filter.
                HExprKind::Closure { body, .. } => self.walk_block(body),
                HExprKind::StructLit { fields, .. } => {
                    for (_, v, _) in fields {
                        self.walk_expr(v);
                    }
                }
                HExprKind::Array(elems) => {
                    for v in elems {
                        self.walk_expr(v);
                    }
                }
                HExprKind::Range(a, b) => {
                    self.walk_expr(a);
                    self.walk_expr(b);
                }
                HExprKind::Unary { expr, .. } => self.walk_expr(expr),
                HExprKind::Binary { left, right, .. } => {
                    self.walk_expr(left);
                    self.walk_expr(right);
                }
                HExprKind::Field { base, .. } => self.walk_expr(base),
                HExprKind::Index { base, index } => {
                    self.walk_expr(base);
                    self.walk_expr(index);
                }
                HExprKind::Ok(v) | HExprKind::Err(v) | HExprKind::Some(v) => {
                    self.walk_expr(v);
                }
                HExprKind::Question { operand, .. } => self.walk_expr(operand),
                HExprKind::Send { chan, value } => {
                    self.walk_expr(chan);
                    self.walk_expr(value);
                }
                HExprKind::Recv { chan } => self.walk_expr(chan),
                HExprKind::ChanNew { cap, .. } => {
                    if let Some(c) = cap {
                        self.walk_expr(c);
                    }
                }
                HExprKind::Int(_)
                | HExprKind::Float(_)
                | HExprKind::Str(_)
                | HExprKind::Bool(_)
                | HExprKind::Unit
                | HExprKind::None => {}
            }
        }
        fn walk_block(&mut self, b: &HBlock) {
            for stmt in &b.stmts {
                match &stmt.kind {
                    HStmtKind::Let { value, .. } => self.walk_expr(value),
                    HStmtKind::Expr(e) | HStmtKind::Return(e) => self.walk_expr(e),
                    HStmtKind::Assign { target, value } => {
                        self.walk_expr(value);
                        match target {
                            HAssignTarget::Var(name, varref, _) => {
                                if let VarRef::Local { depth, .. } = varref {
                                    self.note(name, &value.ty, *depth);
                                }
                            }
                            HAssignTarget::Field(base, _, _) => self.walk_expr(base),
                            HAssignTarget::Index(base, index, _) => {
                                self.walk_expr(base);
                                self.walk_expr(index);
                            }
                        }
                    }
                    HStmtKind::While { cond, body } => {
                        self.walk_expr(cond);
                        self.walk_block(body);
                    }
                    HStmtKind::For { iter, body, .. } => {
                        self.walk_expr(iter);
                        self.walk_block(body);
                    }
                    // A nested `go` body shares the outer creation site:
                    // names it uses that are bound outside flow through
                    // the outer capture list, like nested closures.
                    HStmtKind::Go { body, .. } => self.walk_block(body),
                    HStmtKind::Select { cases, default } => {
                        for case in cases {
                            match case {
                                HSelectCase::Recv { chan, body, .. } => {
                                    self.walk_expr(chan);
                                    self.walk_block(body);
                                }
                                HSelectCase::Send { chan, value, body } => {
                                    self.walk_expr(chan);
                                    self.walk_expr(value);
                                    self.walk_block(body);
                                }
                            }
                        }
                        if let Some(b) = default {
                            self.walk_block(b);
                        }
                    }
                    HStmtKind::Break | HStmtKind::Continue => {}
                }
            }
            if let Some(tail) = &b.tail {
                self.walk_expr(tail);
            }
        }
    }
    let mut walker = Walker {
        creation_depth,
        found: Vec::new(),
    };
    walker.walk_block(body);
    walker.found
}

/// Check a program: returns typed HIR, or `None` with diagnostics.
pub fn check(program: &Program) -> (Option<CheckedProgram>, Vec<Diag>) {
    let mut c = Checker::new();
    // Pass 1a: struct declarations (field types resolved now so that
    // signatures can reference structs declared later in the file).
    for item in &program.items {
        if let crate::ast::ItemKind::Struct(decl) = &item.kind {
            if c.structs.contains_key(&decl.name) {
                c.error(
                    "E0311",
                    format!("duplicate definition of struct `{}`", decl.name),
                    decl.name_span,
                    None,
                );
                continue;
            }
            let mut fields = Vec::new();
            let mut ok = true;
            for f in &decl.fields {
                match c.resolve_ty(&f.ty) {
                    Ok(ty) => {
                        if fields.iter().any(|(n, _): &(String, Ty)| n == &f.name) {
                            c.error(
                                "E0311",
                                format!("duplicate field `{}` in struct `{}`", f.name, decl.name),
                                f.span,
                                None,
                            );
                            ok = false;
                        } else {
                            fields.push((f.name.clone(), ty));
                        }
                    }
                    Err(()) => ok = false,
                }
            }
            if ok {
                c.structs.insert(
                    decl.name.clone(),
                    HStructDecl {
                        name: decl.name.clone(),
                        fields,
                        span: item.span,
                    },
                );
            }
        }
    }
    // Pass 1b: function signatures.
    for item in &program.items {
        if let crate::ast::ItemKind::Fn(decl) = &item.kind {
            if c.fn_index.contains_key(&decl.name) {
                c.error(
                    "E0311",
                    format!("duplicate definition of function `{}`", decl.name),
                    decl.name_span,
                    None,
                );
                continue;
            }
            let mut params = Vec::new();
            let mut ok = true;
            for p in &decl.params {
                match c.resolve_ty(&p.ty) {
                    Ok(ty) => {
                        if params
                            .iter()
                            .any(|(n, _, _): &(String, Ty, bool)| n == &p.name)
                        {
                            c.error(
                                "E0311",
                                format!(
                                    "duplicate parameter `{}` in function `{}`",
                                    p.name, decl.name
                                ),
                                p.name_span,
                                None,
                            );
                            ok = false;
                        } else {
                            params.push((p.name.clone(), ty, p.is_mut));
                        }
                    }
                    Err(()) => ok = false,
                }
            }
            let ret = match c.resolve_ty(&decl.ret) {
                Ok(ty) => ty,
                Err(()) => {
                    ok = false;
                    Ty::Unit
                }
            };
            if ok {
                let idx = c.fns.len();
                c.fn_index.insert(decl.name.clone(), idx);
                c.fns.push(FnSig {
                    name: decl.name.clone(),
                    params,
                    ret,
                    idx,
                });
            }
        }
    }
    // Pass 2: bodies.
    let mut hitems = Vec::new();
    for item in &program.items {
        match &item.kind {
            crate::ast::ItemKind::Struct(decl) => {
                if let Some(h) = c.structs.get(&decl.name) {
                    hitems.push(HItem {
                        kind: HItemKind::Struct(h.clone()),
                        span: item.span,
                    });
                }
            }
            crate::ast::ItemKind::Fn(decl) => {
                if let Some(sig) = c.fns.iter().find(|s| s.name == decl.name).cloned()
                    && let Ok(f) = c.check_fn(decl, &sig)
                {
                    hitems.push(HItem {
                        kind: HItemKind::Fn(f),
                        span: item.span,
                    });
                }
            }
            crate::ast::ItemKind::Import(decl) => {
                // The multi-file driver strips resolved imports before checking,
                // so an Import item here comes from single-file mode: reject it.
                c.error(
                    "E0401",
                    format!(
                        "cannot resolve `import \"{}\"` in single-file mode",
                        decl.path
                    ),
                    decl.path_span,
                    None,
                );
            }
        }
    }
    // Entry point checks. (Cloned: the checks below borrow `c` mutably.)
    let mains: Vec<FnSig> = c.fns.iter().filter(|s| s.name == "main").cloned().collect();
    match mains.as_slice() {
        [] => c.error(
            "E0312",
            "program has no `fn main() -> int`".to_string(),
            Span::new(0, 0),
            None,
        ),
        [sig] => {
            if !sig.params.is_empty() {
                c.error(
                    "E0312",
                    "`main` takes no parameters".to_string(),
                    item_span_for(program, "main"),
                    None,
                );
            }
            if sig.ret != Ty::Int {
                c.error(
                    "E0312",
                    format!("`main` must return `int`, not {}", sig.ret),
                    item_span_for(program, "main"),
                    None,
                );
            }
        }
        _ => {}
    }
    if c.errors.is_empty() {
        let checked = CheckedProgram {
            program: HProgram { items: hitems },
            fns: c.fns,
            structs: c.structs,
            uses_concurrency: c.uses_concurrency,
        };
        (Some(checked), Vec::new())
    } else {
        (None, c.errors)
    }
}

fn item_span_for(program: &Program, name: &str) -> Span {
    for item in &program.items {
        if let crate::ast::ItemKind::Fn(decl) = &item.kind
            && decl.name == name
        {
            return decl.name_span;
        }
    }
    Span::new(0, 0)
}

impl Checker {
    fn check_fn(&mut self, decl: &crate::ast::FnDecl, sig: &FnSig) -> CResult<HFn> {
        self.scopes.clear();
        self.depth = 0;
        let mut params_scope = HashMap::new();
        for (name, ty, is_mut) in &sig.params {
            params_scope.insert(
                name.clone(),
                Binding {
                    ty: ty.clone(),
                    mutable: *is_mut,
                    depth: 0,
                    moved: false,
                },
            );
        }
        self.scopes.push(params_scope);
        let ret = sig.ret.clone();
        let body = self.check_block(&decl.body, &ret)?;
        self.scopes.pop();
        Ok(HFn {
            name: sig.name.clone(),
            params: sig.params.clone(),
            ret,
            body,
            span: decl.name_span,
        })
    }

    fn push_scope(&mut self) {
        self.depth += 1;
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.depth -= 1;
    }

    fn check_block(&mut self, block: &Block, ret: &Ty) -> CResult<HBlock> {
        self.push_scope();
        let mut stmts = Vec::new();
        for stmt in &block.stmts {
            if let Ok(s) = self.check_stmt(stmt, ret) {
                stmts.push(s);
            }
        }
        let tail = match &block.tail {
            Some(e) => self.check_expr(e, None, ret).ok(),
            None => None,
        };
        self.pop_scope();
        Ok(HBlock {
            stmts,
            tail,
            span: block.span,
        })
    }

    fn check_stmt(&mut self, stmt: &Stmt, ret: &Ty) -> CResult<HStmt> {
        use crate::ast::StmtKind;
        let hkind = match &stmt.node {
            StmtKind::Let {
                name,
                value,
                is_mut,
                ty,
                ..
            } => {
                let expected = match ty {
                    Some(t) => Some(self.resolve_ty(t)?),
                    None => None,
                };
                let h = self.check_expr(value, expected.as_ref(), ret)?;
                self.scopes.last_mut().expect("scope underflow").insert(
                    name.clone(),
                    Binding {
                        ty: h.ty.clone(),
                        mutable: *is_mut,
                        depth: self.depth,
                        moved: false,
                    },
                );
                HStmtKind::Let {
                    name: name.clone(),
                    ty: h.ty.clone(),
                    is_mut: *is_mut,
                    value: h,
                }
            }
            StmtKind::Assign { target, value } => {
                let htarget = self.check_assign_target(target, value, ret)?;
                HStmtKind::Assign {
                    target: htarget.0,
                    value: htarget.1,
                }
            }
            StmtKind::Expr(e) => {
                let h = self.check_expr(e, None, ret)?;
                // D4 / spec §11: unused `Result` is a compile error.
                // (`Option` is not a result: discarding one is legal.)
                if matches!(h.ty, Ty::Result(_, _)) {
                    self.error(
                        "E0307",
                        format!("unused `{}` value", h.ty),
                        e.span,
                        Some("bind it with `let`, propagate it with `?`, or return it".to_string()),
                    );
                    return Err(());
                }
                HStmtKind::Expr(h)
            }
            StmtKind::Return(e) => {
                let h = self.check_expr(e, Some(ret), ret)?;
                self.expect_ty(&h.ty, ret, e.span)?;
                HStmtKind::Return(h)
            }
            StmtKind::Break => HStmtKind::Break,
            StmtKind::Continue => HStmtKind::Continue,
            StmtKind::While { cond, body } => {
                let h = self.check_expr(cond, Some(&Ty::Bool), ret)?;
                self.expect_ty(&h.ty, &Ty::Bool, cond.span)?;
                HStmtKind::While {
                    cond: h,
                    body: Box::new(self.check_block(body, ret)?),
                }
            }
            StmtKind::Go { body } => {
                self.uses_concurrency = true;
                let creation_depth = self.depth;
                self.go_stack.push(creation_depth);
                // A `go` body is its own task (spec §13.2: a statement
                // yielding `unit`). `return` inside exits the task and
                // `?` must target the task's own `unit` return — which
                // the `Question` arm always rejects with E0308 — so the
                // body is checked against `unit`, never the enclosing
                // function's return type (checking against the enclosing
                // `ret` would let a task "return" a foreign value).
                let hbody = self.check_block(body, &Ty::Unit);
                self.go_stack.pop();
                let hbody = hbody?;
                let captured = captured_vars(&hbody, creation_depth);
                HStmtKind::Go {
                    body: Box::new(hbody),
                    captured,
                }
            }
            StmtKind::Select { cases, default } => {
                self.uses_concurrency = true;
                let mut hcases = Vec::with_capacity(cases.len());
                for case in cases {
                    match case {
                        crate::ast::SelectCase::Recv { name, chan, body } => {
                            let hchan = self.check_expr(chan, None, ret)?;
                            let Ty::Chan(elem) = &hchan.ty else {
                                self.error(
                                    "E0305",
                                    format!("cannot receive from {}", hchan.ty),
                                    chan.span,
                                    Some("receive targets a `chan<T>`".to_string()),
                                );
                                return Err(());
                            };
                            let elem = (**elem).clone();
                            self.push_scope();
                            if let Some((n, _)) = name {
                                self.scopes.last_mut().expect("scope underflow").insert(
                                    n.clone(),
                                    Binding {
                                        ty: elem,
                                        mutable: false,
                                        depth: self.depth,
                                        moved: false,
                                    },
                                );
                            }
                            let hbody = self.check_block(body, ret);
                            self.pop_scope();
                            hcases.push(HSelectCase::Recv {
                                name: name.clone(),
                                chan: hchan,
                                body: Box::new(hbody?),
                            });
                        }
                        crate::ast::SelectCase::Send { chan, value, body } => {
                            let hchan = self.check_expr(chan, None, ret)?;
                            let Ty::Chan(elem) = &hchan.ty else {
                                self.error(
                                    "E0305",
                                    format!("cannot send on {}", hchan.ty),
                                    chan.span,
                                    Some("send targets a `chan<T>`".to_string()),
                                );
                                return Err(());
                            };
                            let elem = (**elem).clone();
                            let hvalue = self.check_expr(value, Some(&elem), ret)?;
                            self.expect_ty(&hvalue.ty, &elem, value.span)?;
                            self.check_send_move(&hvalue, value.span)?;
                            let hbody = self.check_block(body, ret)?;
                            hcases.push(HSelectCase::Send {
                                chan: hchan,
                                value: hvalue,
                                body: Box::new(hbody),
                            });
                        }
                    }
                }
                let hdefault = match default {
                    Some(b) => Some(Box::new(self.check_block(b, ret)?)),
                    None => None,
                };
                HStmtKind::Select {
                    cases: hcases,
                    default: hdefault,
                }
            }
            StmtKind::For {
                name, iter, body, ..
            } => {
                let h = self.check_expr(iter, None, ret)?;
                let var_ty = match &h.ty {
                    Ty::Range => Ty::Int,
                    Ty::Array(t) => (**t).clone(),
                    other => {
                        self.error(
                            "E0306",
                            format!("cannot iterate over {other}"),
                            iter.span,
                            Some("for iterates ranges (`0..n`) and arrays only".to_string()),
                        );
                        return Err(());
                    }
                };
                self.push_scope();
                self.scopes.last_mut().expect("scope underflow").insert(
                    name.clone(),
                    Binding {
                        ty: var_ty.clone(),
                        mutable: false,
                        depth: self.depth,
                        moved: false,
                    },
                );
                let hbody = self.check_block(body, ret);
                self.pop_scope();
                HStmtKind::For {
                    name: name.clone(),
                    var_ty,
                    iter: h,
                    body: Box::new(hbody?),
                }
            }
        };
        Ok(HStmt {
            kind: hkind,
            span: stmt.span,
        })
    }

    fn check_assign_target(
        &mut self,
        target: &crate::ast::AssignTarget,
        value: &Expr,
        ret: &Ty,
    ) -> CResult<(HAssignTarget, HExpr)> {
        use crate::ast::AssignTarget;
        match target {
            AssignTarget::Var(name, span) => {
                let (ty, varref) = self.resolve_place(name, *span)?;
                // Rebinding gives the binding a fresh value: a moved
                // binding becomes usable again.
                for scope in self.scopes.iter_mut().rev() {
                    if let Some(b) = scope.get_mut(name) {
                        b.moved = false;
                        break;
                    }
                }
                let h = self.check_expr(value, Some(&ty), ret)?;
                self.expect_ty(&h.ty, &ty, value.span)?;
                Ok((HAssignTarget::Var(name.clone(), varref, *span), h))
            }
            AssignTarget::Field(base, field_name, field_span) => {
                let hbase = self.check_expr(base, None, ret)?;
                let Ty::Struct(sname) = &hbase.ty else {
                    self.error(
                        "E0310",
                        format!("cannot assign field on {}", hbase.ty),
                        *field_span,
                        None,
                    );
                    return Err(());
                };
                let fty = self
                    .structs
                    .get(sname)
                    .and_then(|d| d.fields.iter().find(|(n, _)| n == field_name))
                    .map(|(_, t)| t.clone());
                match fty {
                    Some(ty) => {
                        let h = self.check_expr(value, Some(&ty), ret)?;
                        self.expect_ty(&h.ty, &ty, value.span)?;
                        Ok((
                            HAssignTarget::Field(Box::new(hbase), field_name.clone(), *field_span),
                            h,
                        ))
                    }
                    None => {
                        self.error(
                            "E0310",
                            format!("struct {sname} has no field `{field_name}`"),
                            *field_span,
                            None,
                        );
                        Err(())
                    }
                }
            }
            AssignTarget::Index(base, index, span) => {
                let hbase = self.check_expr(base, None, ret)?;
                let hindex = self.check_expr(index, Some(&Ty::Int), ret)?;
                self.expect_ty(&hindex.ty, &Ty::Int, index.span)?;
                let Ty::Array(elem) = &hbase.ty else {
                    self.error("E0310", format!("cannot index {}", hbase.ty), *span, None);
                    return Err(());
                };
                let elem = (**elem).clone();
                let h = self.check_expr(value, Some(&elem), ret)?;
                self.expect_ty(&h.ty, &elem, value.span)?;
                Ok((
                    HAssignTarget::Index(Box::new(hbase), Box::new(hindex), *span),
                    h,
                ))
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr, expected: Option<&Ty>, ret: &Ty) -> CResult<HExpr> {
        use crate::ast::ExprKind;
        let span = expr.span;
        let (kind, ty) = match &expr.node {
            ExprKind::Int(v) => (HExprKind::Int(*v), Ty::Int),
            ExprKind::Float(v) => (HExprKind::Float(*v), Ty::Float),
            ExprKind::Str(s) => (HExprKind::Str(s.clone()), Ty::String),
            ExprKind::Bool(b) => (HExprKind::Bool(*b), Ty::Bool),
            ExprKind::Unit => (HExprKind::Unit, Ty::Unit),
            ExprKind::Var(name) => {
                let (ty, varref) = self.resolve_use(name, span)?;
                if let VarRef::Local { mutable, depth, .. } = &varref {
                    // D3 (spec §13.1/§13.2): a `go` body may not use an
                    // outer `mut` binding — no capture by reference.
                    if *mutable && self.go_stack.last().is_some_and(|g| depth <= g) {
                        self.error(
                            "E0314",
                            format!(
                                "`go` blocks cannot capture `mut` binding `{name}` by reference"
                            ),
                            span,
                            Some(
                                "send the value over a channel, or bind it immutably outside"
                                    .to_string(),
                            ),
                        );
                        return Err(());
                    }
                    // A binding moved into a channel send is gone.
                    if self.is_moved(name) {
                        self.error(
                            "E0314",
                            format!("use of moved value `{name}` (moved into a channel send)"),
                            span,
                            None,
                        );
                        return Err(());
                    }
                }
                (HExprKind::Var(name.clone(), varref), ty)
            }
            ExprKind::If {
                cond,
                then_b,
                else_b,
            } => {
                let hcond = self.check_expr(cond, Some(&Ty::Bool), ret)?;
                self.expect_ty(&hcond.ty, &Ty::Bool, cond.span)?;
                let hthen = self.check_block(then_b, ret)?;
                let tty = self.block_ty(&hthen);
                let helse = match else_b {
                    Some(b) => Some(Box::new(self.check_block(b, ret)?)),
                    None => None,
                };
                let ty = match &helse {
                    Some(b) => {
                        let ety = self.block_ty(b);
                        if tty != ety {
                            self.error(
                                "E0303",
                                format!(
                                    "if branches must agree in type: then is {tty}, else is {ety}"
                                ),
                                span,
                                None,
                            );
                            return Err(());
                        }
                        tty
                    }
                    None => Ty::Unit,
                };
                (
                    HExprKind::If {
                        cond: Box::new(hcond),
                        then_b: Box::new(hthen),
                        else_b: helse,
                    },
                    ty,
                )
            }
            ExprKind::Closure {
                params,
                ret: cret,
                body,
            } => {
                let mut ps = Vec::new();
                for p in params {
                    let ty = self.resolve_ty(&p.ty)?;
                    ps.push((p.name.clone(), ty, p.is_mut));
                }
                let cret = self.resolve_ty(cret)?;
                // The closure body sees enclosing scopes (capture); push a
                // parameter scope on top. Creation depth is the enclosing
                // depth: captures are uses bound at or outside it.
                let creation_depth = self.depth;
                self.push_scope();
                for (name, ty, is_mut) in &ps {
                    self.scopes.last_mut().expect("scope underflow").insert(
                        name.clone(),
                        Binding {
                            ty: ty.clone(),
                            mutable: *is_mut,
                            depth: self.depth,
                            moved: false,
                        },
                    );
                }
                let hbody = self.check_block(body, &cret);
                self.pop_scope();
                let hbody = hbody?;
                let captured = captured_vars(&hbody, creation_depth);
                let fn_ty = Ty::Fn(
                    ps.iter().map(|(_, t, _)| t.clone()).collect(),
                    Box::new(cret.clone()),
                );
                (
                    HExprKind::Closure {
                        params: ps,
                        ret: cret,
                        body: Box::new(hbody),
                        captured,
                    },
                    fn_ty,
                )
            }
            ExprKind::StructLit { name, fields } => {
                let decl = match self.structs.get(name) {
                    Some(d) => d.clone(),
                    None => {
                        self.error("E0302", format!("unknown struct `{name}`"), span, None);
                        return Err(());
                    }
                };
                let mut seen = Vec::new();
                let mut hfields = Vec::new();
                for (fname, fexpr, fspan) in fields {
                    if seen.contains(fname) {
                        self.error(
                            "E0310",
                            format!("duplicate field `{fname}` in `{name}` literal"),
                            *fspan,
                            None,
                        );
                        return Err(());
                    }
                    let want = match decl.fields.iter().find(|(n, _)| n == fname) {
                        Some((_, t)) => t.clone(),
                        None => {
                            self.error(
                                "E0310",
                                format!("unknown field `{fname}` for struct `{name}`"),
                                *fspan,
                                None,
                            );
                            return Err(());
                        }
                    };
                    seen.push(fname.clone());
                    let h = self.check_expr(fexpr, Some(&want), ret)?;
                    self.expect_ty(&h.ty, &want, fexpr.span)?;
                    hfields.push((fname.clone(), h, *fspan));
                }
                for (fname, _) in &decl.fields {
                    if !seen.contains(fname) {
                        self.error(
                            "E0310",
                            format!("missing field `{fname}` in `{name}` literal"),
                            span,
                            None,
                        );
                        return Err(());
                    }
                }
                let mut ordered = Vec::with_capacity(decl.fields.len());
                for (fname, _) in &decl.fields {
                    let pos = hfields
                        .iter()
                        .position(|(n, _, _)| n == fname)
                        .expect("checked above");
                    let (_, h, fs) = hfields.remove(pos);
                    ordered.push((fname.clone(), h, fs));
                }
                (
                    HExprKind::StructLit {
                        name: name.clone(),
                        fields: ordered,
                    },
                    Ty::Struct(name.clone()),
                )
            }
            ExprKind::Array(elems) => {
                if elems.is_empty() {
                    match expected {
                        Some(Ty::Array(t)) => (HExprKind::Array(Vec::new()), Ty::Array(t.clone())),
                        _ => {
                            self.error(
                                "E0309",
                                "cannot infer the element type of an empty array".to_string(),
                                span,
                                Some(
                                    "add an annotation, e.g. `let a: Array<int> = []`".to_string(),
                                ),
                            );
                            return Err(());
                        }
                    }
                } else {
                    let h0 = self.check_expr(&elems[0], None, ret)?;
                    let mut hs = vec![h0];
                    for e in &elems[1..] {
                        let h = self.check_expr(e, Some(&hs[0].ty), ret)?;
                        self.expect_ty(&h.ty, &hs[0].ty.clone(), e.span)?;
                        hs.push(h);
                    }
                    let ty = Ty::Array(Box::new(hs[0].ty.clone()));
                    (HExprKind::Array(hs), ty)
                }
            }
            ExprKind::Range(lo, hi) => {
                let hlo = self.check_expr(lo, Some(&Ty::Int), ret)?;
                self.expect_ty(&hlo.ty, &Ty::Int, lo.span)?;
                let hhi = self.check_expr(hi, Some(&Ty::Int), ret)?;
                self.expect_ty(&hhi.ty, &Ty::Int, hi.span)?;
                (HExprKind::Range(Box::new(hlo), Box::new(hhi)), Ty::Range)
            }
            ExprKind::Binary { op, left, right } => {
                let (h, ty) = self.check_binary(*op, left, right, span, ret)?;
                (h, ty)
            }
            ExprKind::Unary { op, expr: inner } => {
                let h = self.check_expr(inner, None, ret)?;
                let ty = match op {
                    UnOp::Neg => match &h.ty {
                        Ty::Int => Ty::Int,
                        Ty::Float => Ty::Float,
                        other => {
                            self.error("E0303", format!("cannot negate {other}"), inner.span, None);
                            return Err(());
                        }
                    },
                    UnOp::Not => {
                        self.expect_ty(&h.ty, &Ty::Bool, inner.span)?;
                        Ty::Bool
                    }
                };
                (
                    HExprKind::Unary {
                        op: *op,
                        expr: Box::new(h),
                    },
                    ty,
                )
            }
            ExprKind::Call { callee, args } => {
                let (h, ty) = self.check_call(callee, args, span, ret)?;
                (h, ty)
            }
            ExprKind::Send { chan, value } => {
                self.uses_concurrency = true;
                let hchan = self.check_expr(chan, None, ret)?;
                let Ty::Chan(elem) = &hchan.ty else {
                    self.error(
                        "E0305",
                        format!("cannot send on {}", hchan.ty),
                        chan.span,
                        Some("send targets a `chan<T>`".to_string()),
                    );
                    return Err(());
                };
                let elem = (**elem).clone();
                let hvalue = self.check_expr(value, Some(&elem), ret)?;
                self.expect_ty(&hvalue.ty, &elem, value.span)?;
                self.check_send_move(&hvalue, value.span)?;
                (
                    HExprKind::Send {
                        chan: Box::new(hchan),
                        value: Box::new(hvalue),
                    },
                    Ty::Unit,
                )
            }
            ExprKind::Recv { chan } => {
                self.uses_concurrency = true;
                let hchan = self.check_expr(chan, None, ret)?;
                let Ty::Chan(elem) = &hchan.ty else {
                    self.error(
                        "E0305",
                        format!("cannot receive from {}", hchan.ty),
                        chan.span,
                        Some("receive targets a `chan<T>`".to_string()),
                    );
                    return Err(());
                };
                let ty = (**elem).clone();
                (
                    HExprKind::Recv {
                        chan: Box::new(hchan),
                    },
                    ty,
                )
            }
            ExprKind::ChanNew { elem, cap } => {
                self.uses_concurrency = true;
                let elem_ty = self.resolve_ty(elem)?;
                let hcap = match cap {
                    Some(c) => {
                        let h = self.check_expr(c, Some(&Ty::Int), ret)?;
                        self.expect_ty(&h.ty, &Ty::Int, c.span)?;
                        Some(Box::new(h))
                    }
                    None => None,
                };
                (
                    HExprKind::ChanNew {
                        elem_ty: elem_ty.clone(),
                        cap: hcap,
                    },
                    Ty::Chan(Box::new(elem_ty)),
                )
            }
            ExprKind::Field { base, name } => {
                let hbase = self.check_expr(base, None, ret)?;
                let Ty::Struct(sname) = &hbase.ty else {
                    self.error(
                        "E0310",
                        format!("cannot access field of {}", hbase.ty),
                        span,
                        None,
                    );
                    return Err(());
                };
                let fty = self
                    .structs
                    .get(sname)
                    .and_then(|d| d.fields.iter().find(|(n, _)| n == name))
                    .map(|(_, t)| t.clone());
                match fty {
                    Some(ty) => (
                        HExprKind::Field {
                            base: Box::new(hbase),
                            name: name.clone(),
                        },
                        ty,
                    ),
                    None => {
                        self.error(
                            "E0310",
                            format!("struct {sname} has no field `{name}`"),
                            span,
                            None,
                        );
                        return Err(());
                    }
                }
            }
            ExprKind::Index { base, index } => {
                let hbase = self.check_expr(base, None, ret)?;
                let hindex = self.check_expr(index, Some(&Ty::Int), ret)?;
                self.expect_ty(&hindex.ty, &Ty::Int, index.span)?;
                let Ty::Array(elem) = &hbase.ty else {
                    self.error("E0310", format!("cannot index {}", hbase.ty), span, None);
                    return Err(());
                };
                let ty = (**elem).clone();
                (
                    HExprKind::Index {
                        base: Box::new(hbase),
                        index: Box::new(hindex),
                    },
                    ty,
                )
            }
            ExprKind::Ok(inner) => {
                let (t, e) = self.expected_result(expected, span, "Ok")?;
                let h = self.check_expr(inner, Some(&t), ret)?;
                self.expect_ty(&h.ty, &t, inner.span)?;
                (
                    HExprKind::Ok(Box::new(h)),
                    Ty::Result(Box::new(t), Box::new(e)),
                )
            }
            ExprKind::Err(inner) => {
                let (t, e) = self.expected_result(expected, span, "Err")?;
                let h = self.check_expr(inner, Some(&e), ret)?;
                self.expect_ty(&h.ty, &e, inner.span)?;
                (
                    HExprKind::Err(Box::new(h)),
                    Ty::Result(Box::new(t), Box::new(e)),
                )
            }
            ExprKind::Some(inner) => {
                let h = self.check_expr(
                    inner,
                    expected.as_ref().and_then(|t| match t {
                        Ty::Option(inner) => Some(&**inner),
                        _ => None,
                    }),
                    ret,
                )?;
                if let Some(want) = expected {
                    let Ty::Option(want_t) = want else {
                        self.error(
                            "E0303",
                            format!("mismatched types: expected {want}, found Option<{}>", h.ty),
                            span,
                            None,
                        );
                        return Err(());
                    };
                    self.expect_ty(&h.ty, want_t, inner.span)?;
                }
                let ty = Ty::Option(Box::new(h.ty.clone()));
                (HExprKind::Some(Box::new(h)), ty)
            }
            ExprKind::None => match expected {
                Some(Ty::Option(t)) => (HExprKind::None, Ty::Option(t.clone())),
                _ => {
                    self.error(
                        "E0309",
                        "cannot infer the type of `None`".to_string(),
                        span,
                        Some("add an annotation, e.g. `let x: Option<int> = None`".to_string()),
                    );
                    return Err(());
                }
            },
            ExprKind::Question(inner) => {
                let h = self.check_expr(inner, None, ret)?;
                let (inner_ty, fn_ret) = match &h.ty {
                    Ty::Result(t, e) => {
                        let Ty::Result(_, want_e) = ret else {
                            self.error(
                                "E0308",
                                "`?` on a `Result` needs an enclosing function returning `Result`"
                                    .to_string(),
                                span,
                                None,
                            );
                            return Err(());
                        };
                        if e.as_ref() != want_e.as_ref() {
                            self.error(
                                "E0308",
                                format!(
                                    "cannot convert error type {e} to {want_e} with `?`; handle the error explicitly"
                                ),
                                span,
                                None,
                            );
                            return Err(());
                        }
                        ((**t).clone(), ret.clone())
                    }
                    Ty::Option(t) => {
                        if !matches!(ret, Ty::Option(_)) {
                            self.error(
                                "E0308",
                                "`?` on an `Option` needs an enclosing function returning `Option`"
                                    .to_string(),
                                span,
                                None,
                            );
                            return Err(());
                        }
                        ((**t).clone(), ret.clone())
                    }
                    other => {
                        self.error(
                            "E0308",
                            format!("`?` needs a `Result` or `Option`, found {other}"),
                            span,
                            None,
                        );
                        return Err(());
                    }
                };
                let ty = inner_ty.clone();
                (
                    HExprKind::Question {
                        operand: Box::new(h),
                        inner: inner_ty,
                        fn_ret,
                    },
                    ty,
                )
            }
        };
        if let Some(want) = expected {
            self.expect_ty(&ty, want, span)?;
        }
        Ok(HExpr { ty, span, kind })
    }

    fn resolve_use(&mut self, name: &str, span: Span) -> CResult<(Ty, VarRef)> {
        // Locals and parameters first, then top-level functions: user code
        // shadows builtins, exactly like the interpreter's lookup order.
        if let Some((ty, varref)) = self.lookup(name) {
            return Ok((ty, varref));
        }
        if let Some(&idx) = self.fn_index.get(name) {
            let sig = &self.fns[idx];
            return Ok((
                Ty::Fn(
                    sig.params.iter().map(|(_, t, _)| t.clone()).collect(),
                    Box::new(sig.ret.clone()),
                ),
                VarRef::GlobalFn {
                    name: name.to_string(),
                    idx,
                },
            ));
        }
        if crate::builtins::is_builtin(name) {
            self.error(
                "E0305",
                format!("builtin `{name}` is not a first-class value (call it directly)"),
                span,
                None,
            );
            return Err(());
        }
        self.error("E0301", format!("undefined variable `{name}`"), span, None);
        Err(())
    }

    fn block_ty(&self, block: &HBlock) -> Ty {
        match &block.tail {
            Some(e) => e.ty.clone(),
            None => Ty::Unit,
        }
    }

    /// The `Result(T, E)` an `Ok`/`Err` constructor is checked against: it
    /// must come from an annotation, parameter, return type, or another
    /// `Result`-typed context — never guessed.
    fn expected_result(
        &mut self,
        expected: Option<&Ty>,
        span: Span,
        ctx: &str,
    ) -> CResult<(Ty, Ty)> {
        match expected {
            Some(Ty::Result(t, e)) => Ok((((**t).clone()), ((**e).clone()))),
            _ => {
                self.error(
                    "E0309",
                    format!("cannot infer the error type of `{ctx}`"),
                    span,
                    Some(format!(
                        "add an annotation, e.g. `let x: Result<int, string> = {ctx}(…)`"
                    )),
                );
                Err(())
            }
        }
    }

    fn check_binary(
        &mut self,
        op: BinOp,
        left: &Expr,
        right: &Expr,
        span: Span,
        ret: &Ty,
    ) -> CResult<(HExprKind, Ty)> {
        let no_hint = |l: &Ty, r: &Ty| {
            if matches!((l, r), (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int)) {
                Some("int and float do not mix implicitly; convert explicitly with to_int or to_float".to_string())
            } else {
                None
            }
        };
        let arith = |c: &mut Checker, l: &HExpr, r: &HExpr, verb: &str| -> CResult<Ty> {
            match (&l.ty, &r.ty) {
                (Ty::Int, Ty::Int) => Ok(Ty::Int),
                (Ty::Float, Ty::Float) => Ok(Ty::Float),
                (Ty::String, Ty::String) if verb == "add" => Ok(Ty::String),
                (a, b) => {
                    c.error(
                        "E0303",
                        format!("cannot {verb} {a} and {b}"),
                        span,
                        no_hint(a, b),
                    );
                    Err(())
                }
            }
        };
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let l = self.check_expr(left, None, ret)?;
                let r = self.check_expr(right, None, ret)?;
                let verb = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "subtract",
                    _ => "multiply",
                };
                let ty = arith(self, &l, &r, verb)?;
                Ok((
                    HExprKind::Binary {
                        op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                    ty,
                ))
            }
            BinOp::Div | BinOp::Mod => {
                let l = self.check_expr(left, None, ret)?;
                let r = self.check_expr(right, None, ret)?;
                let ty = match (&l.ty, &r.ty) {
                    (Ty::Int, Ty::Int) => Ty::Int,
                    (Ty::Float, Ty::Float) if op == BinOp::Div => Ty::Float,
                    (a, b) => {
                        let verb = if op == BinOp::Div {
                            "divide"
                        } else {
                            "take modulo of"
                        };
                        self.error(
                            "E0303",
                            format!("cannot {verb} {a} and {b}"),
                            span,
                            no_hint(a, b),
                        );
                        return Err(());
                    }
                };
                Ok((
                    HExprKind::Binary {
                        op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                    ty,
                ))
            }
            BinOp::Range => {
                let l = self.check_expr(left, Some(&Ty::Int), ret)?;
                self.expect_ty(&l.ty, &Ty::Int, left.span)?;
                let r = self.check_expr(right, Some(&Ty::Int), ret)?;
                self.expect_ty(&r.ty, &Ty::Int, right.span)?;
                Ok((HExprKind::Range(Box::new(l), Box::new(r)), Ty::Range))
            }
            BinOp::Eq | BinOp::NotEq => {
                let l = self.check_expr(left, None, ret)?;
                let r = self.check_expr(right, None, ret)?;
                // Cross-type `==` is statically rejected (spec §5.4 note).
                // `==` needs no expected type: infer left, require right.
                self.expect_ty(&r.ty, &l.ty, right.span)?;
                Ok((
                    HExprKind::Binary {
                        op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                    Ty::Bool,
                ))
            }
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                let l = self.check_expr(left, None, ret)?;
                let r = self.check_expr(right, None, ret)?;
                match (&l.ty, &r.ty) {
                    (Ty::Int, Ty::Int) | (Ty::Float, Ty::Float) | (Ty::String, Ty::String) => {}
                    (a, b) => {
                        self.error(
                            "E0303",
                            format!("cannot order {a} and {b}"),
                            span,
                            no_hint(a, b),
                        );
                        return Err(());
                    }
                }
                Ok((
                    HExprKind::Binary {
                        op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                    Ty::Bool,
                ))
            }
            BinOp::And | BinOp::Or => {
                let l = self.check_expr(left, Some(&Ty::Bool), ret)?;
                self.expect_ty(&l.ty, &Ty::Bool, left.span)?;
                let r = self.check_expr(right, Some(&Ty::Bool), ret)?;
                self.expect_ty(&r.ty, &Ty::Bool, right.span)?;
                Ok((
                    HExprKind::Binary {
                        op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                    Ty::Bool,
                ))
            }
        }
    }

    fn check_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        ret: &Ty,
    ) -> CResult<(HExprKind, Ty)> {
        // Builtins are called by name only (never first-class values):
        // resolve the name here so shadowing rules apply, then check the
        // frozen signature. Anything else goes through normal checking,
        // where a bare builtin name is E0305.
        if let crate::ast::ExprKind::Var(name) = &callee.node
            && self.lookup(name).is_none()
            && !self.fn_index.contains_key(name)
            && crate::builtins::is_builtin(name)
        {
            let mut hargs = Vec::with_capacity(args.len());
            for a in args {
                hargs.push(self.check_expr(a, None, ret)?);
            }
            let ty = self.check_builtin(name, &hargs, span)?;
            return Ok((
                HExprKind::CallBuiltin {
                    name: name.clone(),
                    args: hargs,
                },
                ty,
            ));
        }
        let hcallee = self.check_expr(callee, None, ret)?;
        let Ty::Fn(params, fret) = &hcallee.ty else {
            self.error(
                "E0305",
                format!("call of non-function value ({})", hcallee.ty),
                callee.span,
                None,
            );
            return Err(());
        };
        if args.len() != params.len() {
            self.error(
                "E0304",
                format!(
                    "wrong number of arguments: takes {}, got {}",
                    params.len(),
                    args.len()
                ),
                span,
                None,
            );
            return Err(());
        }
        let mut hargs = Vec::with_capacity(args.len());
        for (a, p) in args.iter().zip(params.iter()) {
            let h = self.check_expr(a, Some(p), ret)?;
            self.expect_ty(&h.ty, p, a.span)?;
            hargs.push(h);
        }
        let ty = (**fret).clone();
        Ok((
            HExprKind::Call {
                callee: Box::new(hcallee),
                args: hargs,
            },
            ty,
        ))
    }

    /// Frozen builtin signatures (spec §7). The catalog does not grow.
    fn check_builtin(&mut self, name: &str, args: &[HExpr], span: Span) -> CResult<Ty> {
        let nargs = args.len();
        match name {
            "print" | "println" => {
                self.builtin_arity(name, nargs, 1, span)?;
                Ok(Ty::Int)
            }
            "len" => {
                self.builtin_arity(name, nargs, 1, span)?;
                match &args[0].ty {
                    Ty::String => Ok(Ty::Int),
                    Ty::Array(_) => Ok(Ty::Int),
                    other => {
                        self.error(
                            "E0303",
                            format!("`len` needs a string or array, found {other}"),
                            span,
                            None,
                        );
                        Err(())
                    }
                }
            }
            "split" => {
                self.builtin_arity(name, nargs, 2, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                self.expect_arg(name, &args[1], 1, &Ty::String)?;
                Ok(Ty::Array(Box::new(Ty::String)))
            }
            "trim" | "upper" | "lower" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                Ok(Ty::String)
            }
            "contains" => {
                self.builtin_arity(name, nargs, 2, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                self.expect_arg(name, &args[1], 1, &Ty::String)?;
                Ok(Ty::Bool)
            }
            "to_string" => {
                self.builtin_arity(name, nargs, 1, span)?;
                match &args[0].ty {
                    Ty::Int | Ty::Float | Ty::Bool => Ok(Ty::String),
                    other => {
                        self.error(
                            "E0303",
                            format!("`to_string` needs int, float, or bool, found {other}"),
                            span,
                            None,
                        );
                        Err(())
                    }
                }
            }
            "parse_int" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                Ok(Ty::Int)
            }
            "to_int" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::Float)?;
                Ok(Ty::Int)
            }
            "to_float" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::Int)?;
                Ok(Ty::Float)
            }
            "is_nan" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::Float)?;
                Ok(Ty::Bool)
            }
            "abs" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::Int)?;
                Ok(Ty::Int)
            }
            "min" | "max" => {
                self.builtin_arity(name, nargs, 2, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::Int)?;
                self.expect_arg(name, &args[1], 1, &Ty::Int)?;
                Ok(Ty::Int)
            }
            "sqrt" | "floor" | "ceil" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::Float)?;
                Ok(Ty::Float)
            }
            "push" => {
                self.builtin_arity(name, nargs, 2, span)?;
                let Ty::Array(elem) = &args[0].ty else {
                    self.error(
                        "E0303",
                        format!("`push` needs an array, found {}", args[0].ty),
                        args[0].span,
                        None,
                    );
                    return Err(());
                };
                let elem = (**elem).clone();
                self.expect_ty(&args[1].ty, &elem, args[1].span)?;
                Ok(Ty::Int)
            }
            "pop" => {
                self.builtin_arity(name, nargs, 1, span)?;
                let Ty::Array(elem) = &args[0].ty else {
                    self.error(
                        "E0303",
                        format!("`pop` needs an array, found {}", args[0].ty),
                        args[0].span,
                        None,
                    );
                    return Err(());
                };
                Ok((**elem).clone())
            }
            "read_file" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                Ok(Ty::String)
            }
            "write_file" => {
                self.builtin_arity(name, nargs, 2, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                self.expect_arg(name, &args[1], 1, &Ty::String)?;
                Ok(Ty::Int)
            }
            "file_exists" => {
                self.builtin_arity(name, nargs, 1, span)?;
                self.expect_arg(name, &args[0], 0, &Ty::String)?;
                Ok(Ty::Bool)
            }
            "close" => {
                self.uses_concurrency = true;
                self.builtin_arity(name, nargs, 1, span)?;
                match &args[0].ty {
                    Ty::Chan(elem) => {
                        // Recv-on-closed yields the element zero value; spec
                        // §13.1 rows `chan`/`fn` elements to E0315 (they
                        // have no zero value). All other element types —
                        // incl. [Proposal] TBD rows — stay allowed: only
                        // what the spec bans is banned.
                        if matches!(elem.as_ref(), Ty::Chan(_) | Ty::Fn(..)) {
                            self.error(
                                "E0315",
                                format!(
                                    "cannot close `chan<{}>`: element type has no zero value",
                                    elem
                                ),
                                span,
                                Some(
                                    "recv on a closed channel must yield the zero value; `chan` and `fn` elements have none".to_string(),
                                ),
                            );
                            return Err(());
                        }
                        Ok(Ty::Unit)
                    }
                    other => {
                        self.error(
                            "E0303",
                            format!("`close` needs a channel, found {other}"),
                            span,
                            None,
                        );
                        Err(())
                    }
                }
            }
            "closed" => {
                self.uses_concurrency = true;
                self.builtin_arity(name, nargs, 1, span)?;
                match &args[0].ty {
                    Ty::Chan(_) => Ok(Ty::Bool),
                    other => {
                        self.error(
                            "E0303",
                            format!("`closed` needs a channel, found {other}"),
                            span,
                            None,
                        );
                        Err(())
                    }
                }
            }
            _ => {
                self.error("E0301", format!("undefined variable `{name}`"), span, None);
                Err(())
            }
        }
    }

    fn builtin_arity(&mut self, name: &str, got: usize, want: usize, span: Span) -> CResult<()> {
        if got == want {
            Ok(())
        } else {
            self.error(
                "E0304",
                format!("wrong number of arguments: `{name}` takes {want}, got {got}"),
                span,
                None,
            );
            Err(())
        }
    }

    fn expect_arg(&mut self, name: &str, arg: &HExpr, i: usize, want: &Ty) -> CResult<()> {
        if &arg.ty == want {
            Ok(())
        } else {
            self.error(
                "E0303",
                format!(
                    "`{name}` argument {} must be {want}, found {}",
                    i + 1,
                    arg.ty
                ),
                arg.span,
                None,
            );
            Err(())
        }
    }

    /// Resolve an assignment place: must be a mutable local or parameter.
    fn resolve_place(&mut self, name: &str, span: Span) -> CResult<(Ty, VarRef)> {
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.get(name) {
                if !b.mutable {
                    self.error(
                        "E0313",
                        format!("cannot reassign immutable binding `{name}`"),
                        span,
                        Some("declare it with `let mut` to reassign".to_string()),
                    );
                    return Err(());
                }
                return Ok((
                    b.ty.clone(),
                    VarRef::Local {
                        name: name.to_string(),
                        mutable: true,
                        depth: b.depth,
                    },
                ));
            }
        }
        // Parameters live in the outermost scope and are covered above.
        // Functions and builtins are not assignable places.
        if self.fn_index.contains_key(name) {
            self.error(
                "E0313",
                format!("cannot assign to function `{name}`"),
                span,
                None,
            );
        } else {
            self.error("E0301", format!("undefined variable `{name}`"), span, None);
        }
        Err(())
    }
}
