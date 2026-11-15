//! Tree-walking interpreter (spec §5). Self-contained value model with
//! reference-counted containers; checked arithmetic; total error paths
//! (no Rust panics on user programs).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::TypeKind;
use crate::ast::*;
use crate::diag::Diag;
use crate::span::Span;

pub use crate::builtins::BUILTIN_NAMES;

/// Maximum Toto call-stack frames (spec §5.9, interpreter-only).
pub const MAX_FRAMES: usize = 8192;

/// Runtime value.
#[derive(Clone, Debug)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
    Str(Rc<String>),
    Array(Rc<RefCell<Vec<Value>>>),
    Struct(Rc<RefCell<StructVal>>),
    Range(i64, i64),
    Closure(Rc<ClosureVal>),
    Func(usize),
    Ok(Box<Value>),
    Err(Box<Value>),
    Some(Box<Value>),
    None,
}

#[derive(Clone, Debug)]
pub struct StructVal {
    pub name: String,
    pub fields: Vec<(String, Value)>,
}

#[derive(Clone, Debug)]
pub struct ClosureVal {
    pub code: ClosureCode,
    pub captured: Vec<(String, Value)>,
}

/// Where a closure's code lives. The tree-walking interpreter creates
/// `Ast`; the MIR interpreter (Phase 2) creates `Mir`. Each interpreter
/// only ever executes its own variant: a value never crosses between them.
#[derive(Clone, Debug)]
pub enum ClosureCode {
    Ast {
        params: Vec<Param>,
        ret: Type,
        body: Box<Block>,
    },
    Mir {
        params: Vec<(String, crate::hir::Ty)>,
        ret: crate::hir::Ty,
        body: std::sync::Arc<crate::mir::MirBody>,
    },
}

impl Value {
    /// Runtime type name for diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Unit => "unit",
            Value::Str(_) => "string",
            Value::Array(_) => "Array",
            Value::Struct(_) => "struct",
            Value::Range(_, _) => "Range",
            Value::Closure(_) | Value::Func(_) => "function",
            Value::Ok(_) | Value::Err(_) => "Result",
            Value::Some(_) | Value::None => "Option",
        }
    }
}

/// Format a value with cycle protection for self-referential containers.
pub fn format_value(v: &Value, fns: &[FnDecl]) -> String {
    let mut out = String::new();
    let mut visiting: Vec<*const ()> = Vec::new();
    fmt_val(v, fns, &mut out, &mut visiting, 0);
    out
}

fn fmt_val(
    v: &Value,
    fns: &[FnDecl],
    out: &mut String,
    visiting: &mut Vec<*const ()>,
    depth: usize,
) {
    if depth > 100 {
        out.push_str("<...>");
        return;
    }
    match v {
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&format!("{f:?}")),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Unit => out.push_str("()"),
        Value::Str(s) => out.push_str(s),
        Value::Range(a, b) => out.push_str(&format!("{a}..{b}")),
        Value::Closure(_) => out.push_str("<closure>"),
        Value::Ok(v) => {
            out.push_str("Ok(");
            fmt_val(v, fns, out, visiting, depth + 1);
            out.push(')');
        }
        Value::Err(v) => {
            out.push_str("Err(");
            fmt_val(v, fns, out, visiting, depth + 1);
            out.push(')');
        }
        Value::Some(v) => {
            out.push_str("Some(");
            fmt_val(v, fns, out, visiting, depth + 1);
            out.push(')');
        }
        Value::None => out.push_str("None"),
        Value::Func(idx) => {
            let name = fns.get(*idx).map(|f| f.name.as_str()).unwrap_or("?");
            out.push_str(&format!("<fn {name}>"));
        }
        Value::Array(cell) => {
            let ptr = Rc::as_ptr(cell) as *const ();
            if visiting.contains(&ptr) {
                out.push_str("<cycle>");
                return;
            }
            visiting.push(ptr);
            out.push('[');
            for (i, e) in cell.borrow().iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_val(e, fns, out, visiting, depth + 1);
            }
            out.push(']');
            visiting.pop();
        }
        Value::Struct(cell) => {
            let ptr = Rc::as_ptr(cell) as *const ();
            if visiting.contains(&ptr) {
                out.push_str("<cycle>");
                return;
            }
            visiting.push(ptr);
            let s = cell.borrow();
            out.push_str(&s.name);
            out.push_str(" { ");
            for (i, (name, e)) in s.fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(name);
                out.push_str(": ");
                fmt_val(e, fns, out, visiting, depth + 1);
            }
            out.push_str(" }");
            visiting.pop();
        }
    }
}

#[derive(Clone, Debug)]
pub struct TraceFrame {
    pub name: String,
    pub span: Span,
}

/// Enclosing-function return shape (for `?`). The checker guarantees the
/// error types line up, so `?` propagation here is identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetKind {
    Result,
    Option,
    Other,
}

fn ret_kind(ty: &Type) -> RetKind {
    match &ty.kind {
        TypeKind::Named { name, .. } if name == "Result" => RetKind::Result,
        TypeKind::Named { name, .. } if name == "Option" => RetKind::Option,
        _ => RetKind::Other,
    }
}

fn is_unit_ret(ty: &Type) -> bool {
    matches!(&ty.kind, TypeKind::Named { name, .. } if name == "unit")
}

#[derive(Clone, Debug)]
struct Slot {
    value: Value,
    mutable: bool,
}

/// Control-flow signal: normal value, early return, loop control, or a
/// Toto panic (never a Rust panic).
pub enum Signal {
    Return(Value),
    Break,
    Continue,
    Panic(Diag, Vec<TraceFrame>),
}

pub type Exec<T> = Result<T, Signal>;

fn panic(code: &'static str, message: String, span: Span, help: Option<String>) -> Signal {
    Signal::Panic(Diag::new(code, message, span, help), Vec::new())
}

pub struct Interp {
    fns: Vec<FnDecl>,
    fn_index: HashMap<String, usize>,
    structs: HashMap<String, StructDecl>,
    trace: Vec<TraceFrame>,
    call_depth: usize,
    pub output: Vec<u8>,
    /// Enclosing function return kinds (for `?`); pushed per call.
    ret_stack: Vec<RetKind>,
}

impl Interp {
    pub fn new(program: &Program) -> Self {
        let mut fns = Vec::new();
        let mut fn_index = HashMap::new();
        let mut structs = HashMap::new();
        for item in &program.items {
            match &item.kind {
                ItemKind::Fn(decl) => {
                    // Last definition wins (spec §3.1 leniency).
                    fn_index.insert(decl.name.clone(), fns.len());
                    fns.push(decl.clone());
                }
                ItemKind::Struct(decl) => {
                    structs.insert(decl.name.clone(), decl.clone());
                }
            }
        }
        Self {
            fns,
            fn_index,
            structs,
            trace: Vec::new(),
            call_depth: 0,
            output: Vec::new(),
            ret_stack: Vec::new(),
        }
    }

    /// Function table for builtin display of `<fn name>` values.
    pub fn fns(&self) -> &[FnDecl] {
        &self.fns
    }

    /// Dispatch a builtin call with this interpreter's function table,
    /// capturing printed output into its buffer.
    pub fn dispatch_builtin(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, (Diag, Vec<TraceFrame>)> {
        crate::builtins::call_builtin(name, args, span, &self.fns, &mut self.output)
    }

    pub fn run(&mut self) -> Result<i32, (Diag, Vec<TraceFrame>)> {
        let main = match self.fn_index.get("main") {
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
        let decl = self.fns[main].clone();
        if !decl.params.is_empty() {
            return Err((
                Diag::new(
                    "E0204",
                    "main takes no parameters".to_string(),
                    decl.name_span,
                    None,
                ),
                Vec::new(),
            ));
        }
        let value = self.call_fn(main, Vec::new(), decl.name_span)?;
        match value {
            Value::Int(n) if (0..=255).contains(&n) => Ok(n as i32),
            Value::Int(n) => Err((
                Diag::new(
                    "E0204",
                    format!("main returned {n}, which is not a valid exit code 0-255"),
                    decl.name_span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
            other => Err((
                Diag::new(
                    "E0204",
                    format!("main must return an int, not {}", other.type_name()),
                    decl.name_span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
        }
    }

    fn call_fn(
        &mut self,
        idx: usize,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, (Diag, Vec<TraceFrame>)> {
        let decl = self.fns[idx].clone();
        if args.len() != decl.params.len() {
            return Err((
                Diag::new(
                    "E0204",
                    format!(
                        "wrong number of arguments: `{}` takes {}, got {}",
                        decl.name,
                        decl.params.len(),
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
            name: decl.name.clone(),
            span,
        });
        self.ret_stack.push(ret_kind(&decl.ret));
        let mut frame = HashMap::new();
        for (param, arg) in decl.params.iter().zip(args) {
            frame.insert(
                param.name.clone(),
                Slot {
                    value: arg,
                    mutable: param.is_mut,
                },
            );
        }
        let mut scopes = vec![frame];
        let result = self.eval_block_value(&decl.body, &mut scopes);
        self.call_depth -= 1;
        self.ret_stack.pop();
        match result {
            Ok(v) => {
                if is_unit_ret(&decl.ret) {
                    self.trace.pop();
                    Ok(Value::Unit)
                } else if decl.body.tail.is_none() {
                    // Fell off the end: keep this frame for the trace.
                    Err((
                        Diag::new(
                            "E0212",
                            format!("function `{}` exited without returning a value", decl.name),
                            decl.body.span,
                            None,
                        ),
                        std::mem::take(&mut self.trace),
                    ))
                } else {
                    self.trace.pop();
                    Ok(v)
                }
            }
            Err(Signal::Return(v)) => {
                self.trace.pop();
                Ok(v)
            }
            Err(Signal::Panic(diag, carried)) => {
                // Merge: outer frames still live here plus any trace
                // carried from deeper (at most one side is non-empty,
                // since panics unwind without pushing new frames).
                let mut trace = std::mem::take(&mut self.trace);
                trace.extend(carried);
                Err((diag, trace))
            }
            Err(Signal::Break) | Err(Signal::Continue) => Err((
                Diag::new(
                    "E0204",
                    "loop control escaped its function (unreachable)".to_string(),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
        }
    }

    fn call_closure(
        &mut self,
        closure: &ClosureVal,
        args: Vec<Value>,
        span: Span,
        display: &str,
    ) -> Result<Value, (Diag, Vec<TraceFrame>)> {
        let ClosureCode::Ast { params, ret, body } = &closure.code else {
            // Unreachable: the tree-walker only creates Ast closures, and
            // values never cross to the MIR interpreter.
            return Err((
                Diag::new(
                    "E0204",
                    "internal error: MIR closure called by tree-walking interpreter".to_string(),
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
        self.ret_stack.push(ret_kind(ret));
        let mut frame = HashMap::new();
        for (name, value) in &closure.captured {
            frame.insert(
                name.clone(),
                Slot {
                    value: value.clone(),
                    mutable: false,
                },
            );
        }
        for (param, arg) in params.iter().zip(args) {
            frame.insert(
                param.name.clone(),
                Slot {
                    value: arg,
                    mutable: param.is_mut,
                },
            );
        }
        let mut scopes = vec![frame];
        let result = self.eval_block_value(body, &mut scopes);
        self.call_depth -= 1;
        self.ret_stack.pop();
        match result {
            Ok(v) => {
                if is_unit_ret(ret) {
                    self.trace.pop();
                    Ok(Value::Unit)
                } else if body.tail.is_none() {
                    // Fell off the end: keep this frame for the trace.
                    Err((
                        Diag::new(
                            "E0212",
                            "closure exited without returning a value".to_string(),
                            body.span,
                            None,
                        ),
                        std::mem::take(&mut self.trace),
                    ))
                } else {
                    self.trace.pop();
                    Ok(v)
                }
            }
            Err(Signal::Return(v)) => {
                self.trace.pop();
                Ok(v)
            }
            Err(Signal::Panic(diag, carried)) => {
                // Merge: outer frames still live here plus any trace
                // carried from deeper (at most one side is non-empty,
                // since panics unwind without pushing new frames).
                let mut trace = std::mem::take(&mut self.trace);
                trace.extend(carried);
                Err((diag, trace))
            }
            Err(Signal::Break) | Err(Signal::Continue) => Err((
                Diag::new(
                    "E0204",
                    "loop control escaped its function (unreachable)".to_string(),
                    span,
                    None,
                ),
                std::mem::take(&mut self.trace),
            )),
        }
    }

    fn lookup(&self, scopes: &[HashMap<String, Slot>], name: &str) -> Option<Value> {
        for scope in scopes.iter().rev() {
            if let Some(slot) = scope.get(name) {
                return Some(slot.value.clone());
            }
        }
        None
    }

    /// Evaluate a block: new scope, statements in order, tail or `()`.
    fn eval_block_value(
        &mut self,
        block: &Block,
        scopes: &mut Vec<HashMap<String, Slot>>,
    ) -> Exec<Value> {
        scopes.push(HashMap::new());
        let mut result = Ok(Value::Unit);
        for stmt in &block.stmts {
            match self.exec_stmt(stmt, scopes) {
                Ok(()) => {}
                Err(sig) => {
                    scopes.pop();
                    return Err(sig);
                }
            }
        }
        if let Some(tail) = &block.tail {
            result = self.eval_expr(tail, scopes);
        }
        scopes.pop();
        result
    }

    fn exec_stmt(&mut self, stmt: &Stmt, scopes: &mut Vec<HashMap<String, Slot>>) -> Exec<()> {
        match &stmt.node {
            StmtKind::Let {
                name,
                value,
                is_mut,
                ..
            } => {
                let v = self.eval_expr(value, scopes)?;
                let scope = scopes.last_mut().expect("scope stack underflow");
                scope.insert(
                    name.clone(),
                    Slot {
                        value: v,
                        mutable: *is_mut,
                    },
                );
                Ok(())
            }
            StmtKind::Assign { target, value } => {
                let v = self.eval_expr(value, scopes)?;
                self.assign(target, v, stmt.span, scopes)?;
                Ok(())
            }
            StmtKind::Expr(e) => {
                self.eval_expr(e, scopes)?;
                Ok(())
            }
            StmtKind::Return(e) => {
                let v = self.eval_expr(e, scopes)?;
                Err(Signal::Return(v))
            }
            StmtKind::Break => Err(Signal::Break),
            StmtKind::Continue => Err(Signal::Continue),
            StmtKind::While { cond, body } => {
                loop {
                    let c = self.eval_expr(cond, scopes)?;
                    match c {
                        Value::Bool(true) => {}
                        Value::Bool(false) => break,
                        other => {
                            return Err(panic(
                                "E0207",
                                format!("while condition must be bool, got {}", other.type_name()),
                                cond.span,
                                None,
                            ));
                        }
                    }
                    match self.eval_block_value(body, scopes) {
                        Ok(_) => {}
                        Err(Signal::Break) => break,
                        Err(Signal::Continue) => {}
                        Err(sig) => return Err(sig),
                    }
                }
                Ok(())
            }
            StmtKind::For {
                name, iter, body, ..
            } => {
                let it = self.eval_expr(iter, scopes)?;
                match it {
                    Value::Range(start, end) => {
                        let mut i = start;
                        while i < end {
                            scopes.push(HashMap::new());
                            scopes.last_mut().expect("scope underflow").insert(
                                name.clone(),
                                Slot {
                                    value: Value::Int(i),
                                    mutable: false,
                                },
                            );
                            let r = self.eval_block_value(body, scopes);
                            scopes.pop();
                            match r {
                                Ok(_) => {}
                                Err(Signal::Break) => break,
                                Err(Signal::Continue) => {}
                                Err(sig) => return Err(sig),
                            }
                            // Saturating increment: a range ending at
                            // i64::MAX terminates here instead of wrapping.
                            i = i.saturating_add(1);
                            if i == end {
                                break;
                            }
                        }
                        Ok(())
                    }
                    Value::Array(cell) => {
                        // Snapshot: the loop iterates the elements present
                        // when the loop starts, so push/pop inside the body
                        // cannot invalidate iteration or borrow state.
                        let snapshot = cell.borrow().clone();
                        for elem in snapshot {
                            scopes.push(HashMap::new());
                            scopes.last_mut().expect("scope underflow").insert(
                                name.clone(),
                                Slot {
                                    value: elem,
                                    mutable: false,
                                },
                            );
                            let r = self.eval_block_value(body, scopes);
                            scopes.pop();
                            match r {
                                Ok(_) => {}
                                Err(Signal::Break) => break,
                                Err(Signal::Continue) => {}
                                Err(sig) => return Err(sig),
                            }
                        }
                        Ok(())
                    }
                    other => Err(panic(
                        "E0207",
                        format!("cannot iterate over {}", other.type_name()),
                        iter.span,
                        Some("for iterates ranges (`0..n`) and arrays only".to_string()),
                    )),
                }
            }
            // Concurrent constructs never reach the sequential tree-walker
            // in production: `toto run` routes `uses_concurrency` programs
            // to the runtime executor. This arm is a routing tripwire.
            StmtKind::Go { .. } | StmtKind::Select { .. } => Err(panic(
                "E0215",
                "concurrent construct reached the sequential interpreter (internal routing error)"
                    .to_string(),
                stmt.span,
                Some("concurrent programs run on the runtime executor".to_string()),
            )),
        }
    }

    fn assign(
        &mut self,
        target: &AssignTarget,
        value: Value,
        span: Span,
        scopes: &mut Vec<HashMap<String, Slot>>,
    ) -> Exec<()> {
        match target {
            AssignTarget::Var(name, name_span) => {
                for scope in scopes.iter_mut().rev() {
                    if let Some(slot) = scope.get_mut(name) {
                        if !slot.mutable {
                            return Err(panic(
                                "E0204",
                                format!("cannot reassign immutable binding `{name}`"),
                                *name_span,
                                Some("declare it with `let mut` to reassign".to_string()),
                            ));
                        }
                        slot.value = value;
                        return Ok(());
                    }
                }
                Err(panic(
                    "E0208",
                    format!("undefined variable `{name}`"),
                    *name_span,
                    None,
                ))
            }
            AssignTarget::Field(base, field_name, field_span) => {
                let b = self.eval_expr(base, scopes)?;
                crate::ops::field_write(b, field_name, value, *field_span)
                    .map_err(|(d, t)| Signal::Panic(d, t))
            }
            AssignTarget::Index(base, index, _) => {
                let b = self.eval_expr(base, scopes)?;
                let i = self.eval_expr(index, scopes)?;
                crate::ops::index_write(b, i, value, span, index.span)
                    .map_err(|(d, t)| Signal::Panic(d, t))
            }
        }
    }
}

impl Interp {
    fn eval_expr(&mut self, expr: &Expr, scopes: &mut Vec<HashMap<String, Slot>>) -> Exec<Value> {
        match &expr.node {
            ExprKind::Int(v) => Ok(Value::Int(*v)),
            ExprKind::Float(v) => Ok(Value::Float(*v)),
            ExprKind::Str(s) => Ok(Value::Str(Rc::new(s.clone()))),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Unit => Ok(Value::Unit),
            ExprKind::Ok(inner) => Ok(Value::Ok(Box::new(self.eval_expr(inner, scopes)?))),
            ExprKind::Err(inner) => Ok(Value::Err(Box::new(self.eval_expr(inner, scopes)?))),
            ExprKind::Some(inner) => Ok(Value::Some(Box::new(self.eval_expr(inner, scopes)?))),
            ExprKind::None => Ok(Value::None),
            // Routing tripwire, as in `exec_stmt`: the sequential
            // tree-walker cannot execute channels.
            ExprKind::Send { .. } | ExprKind::Recv { .. } | ExprKind::ChanNew { .. } => Err(panic(
                "E0215",
                "concurrent construct reached the sequential interpreter (internal routing error)"
                    .to_string(),
                expr.span,
                Some("concurrent programs run on the runtime executor".to_string()),
            )),
            ExprKind::Question(inner) => {
                let v = self.eval_expr(inner, scopes)?;
                let enclosing = match self.ret_stack.last().copied().unwrap_or(RetKind::Other) {
                    RetKind::Result => crate::ops::FnRetKind::Result,
                    RetKind::Option => crate::ops::FnRetKind::Option,
                    RetKind::Other => crate::ops::FnRetKind::Other,
                };
                // The checker guarantees the error type matches the
                // enclosing return type, so propagation is identity and
                // the original error value is preserved as the cause.
                match crate::ops::propagate(v, enclosing, expr.span) {
                    Ok(unwrapped) => Ok(unwrapped),
                    Err(crate::ops::Propagate::Return(e)) => Err(Signal::Return(e)),
                    Err(crate::ops::Propagate::Bug(span)) => Err(panic(
                        "E0204",
                        "`?` outside a matching Result/Option function (unreachable in checked programs)".to_string(),
                        span,
                        None,
                    )),
                }
            }
            ExprKind::Var(name) => match self.lookup(scopes, name) {
                Some(v) => Ok(v),
                // Named functions are first-class values.
                None => match self.fn_index.get(name) {
                    Some(&idx) => Ok(Value::Func(idx)),
                    None => Err(panic(
                        "E0208",
                        format!("undefined variable `{name}`"),
                        expr.span,
                        None,
                    )),
                },
            },
            ExprKind::If {
                cond,
                then_b,
                else_b,
            } => {
                let c = self.eval_expr(cond, scopes)?;
                match c {
                    Value::Bool(true) => self.eval_block_value(then_b, scopes),
                    Value::Bool(false) => match else_b {
                        Some(b) => self.eval_block_value(b, scopes),
                        None => Ok(Value::Unit),
                    },
                    other => Err(panic(
                        "E0207",
                        format!("if condition must be bool, got {}", other.type_name()),
                        cond.span,
                        None,
                    )),
                }
            }
            ExprKind::Closure { params, ret, body } => {
                // Capture by value at creation (spec §5.7): copy every
                // visible binding. Scalars copy; arrays/structs share.
                // Innermost scope first so shadowed names resolve to the
                // innermost binding deterministically (HashMap order is
                // randomized per process and must not be observable).
                let mut captured = Vec::new();
                for scope in scopes.iter().rev() {
                    for (name, slot) in scope {
                        if !captured.iter().any(|(n, _)| n == name) {
                            captured.push((name.clone(), slot.value.clone()));
                        }
                    }
                }
                Ok(Value::Closure(Rc::new(ClosureVal {
                    code: ClosureCode::Ast {
                        params: params.clone(),
                        ret: ret.clone(),
                        body: body.clone(),
                    },
                    captured,
                })))
            }
            ExprKind::StructLit { name, fields } => {
                self.eval_struct_lit(name, fields, expr.span, scopes)
            }
            ExprKind::Array(elems) => {
                let mut values = Vec::with_capacity(elems.len());
                for e in elems {
                    values.push(self.eval_expr(e, scopes)?);
                }
                Ok(Value::Array(Rc::new(RefCell::new(values))))
            }
            ExprKind::Range(lo, hi) => {
                let l = self.eval_expr(lo, scopes)?;
                let h = self.eval_expr(hi, scopes)?;
                match (l, h) {
                    (Value::Int(a), Value::Int(b)) => Ok(Value::Range(a, b)),
                    (a, b) => Err(panic(
                        "E0203",
                        format!(
                            "range bounds must be int, got {} and {}",
                            a.type_name(),
                            b.type_name()
                        ),
                        expr.span,
                        None,
                    )),
                }
            }
            ExprKind::Unary { op, expr: inner } => {
                let v = self.eval_expr(inner, scopes)?;
                self.eval_unary(*op, v, inner.span, expr.span)
            }
            ExprKind::Binary { op, left, right } => {
                if matches!(op, BinOp::And) {
                    let l = self.eval_expr(left, scopes)?;
                    match l {
                        Value::Bool(false) => return Ok(Value::Bool(false)),
                        Value::Bool(true) => {
                            let r = self.eval_expr(right, scopes)?;
                            return match r {
                                Value::Bool(b) => Ok(Value::Bool(b)),
                                other => Err(panic(
                                    "E0203",
                                    format!("`&&` needs bools, got bool and {}", other.type_name()),
                                    right.span,
                                    None,
                                )),
                            };
                        }
                        other => {
                            return Err(panic(
                                "E0203",
                                format!("`&&` needs bools, got {}", other.type_name()),
                                left.span,
                                None,
                            ));
                        }
                    }
                }
                if matches!(op, BinOp::Or) {
                    let l = self.eval_expr(left, scopes)?;
                    match l {
                        Value::Bool(true) => return Ok(Value::Bool(true)),
                        Value::Bool(false) => {
                            let r = self.eval_expr(right, scopes)?;
                            return match r {
                                Value::Bool(b) => Ok(Value::Bool(b)),
                                other => Err(panic(
                                    "E0203",
                                    format!("`||` needs bools, got bool and {}", other.type_name()),
                                    right.span,
                                    None,
                                )),
                            };
                        }
                        other => {
                            return Err(panic(
                                "E0203",
                                format!("`||` needs bools, got {}", other.type_name()),
                                left.span,
                                None,
                            ));
                        }
                    }
                }
                let l = self.eval_expr(left, scopes)?;
                let r = self.eval_expr(right, scopes)?;
                self.eval_binary(*op, l, r, expr.span)
            }
            ExprKind::Call { callee, args } => self.eval_call(callee, args, expr.span, scopes),
            ExprKind::Field { base, name } => {
                let b = self.eval_expr(base, scopes)?;
                crate::ops::field_read(b, name, expr.span).map_err(|(d, t)| Signal::Panic(d, t))
            }
            ExprKind::Index { base, index } => {
                let b = self.eval_expr(base, scopes)?;
                let i = self.eval_expr(index, scopes)?;
                crate::ops::index_read(b, i, expr.span, index.span)
                    .map_err(|(d, t)| Signal::Panic(d, t))
            }
        }
    }

    fn eval_struct_lit(
        &mut self,
        name: &str,
        fields: &[(String, Expr, Span)],
        span: Span,
        scopes: &mut Vec<HashMap<String, Slot>>,
    ) -> Exec<Value> {
        let decl = match self.structs.get(name) {
            Some(d) => d.clone(),
            None => {
                return Err(panic(
                    "E0205",
                    format!("unknown struct `{name}`"),
                    span,
                    None,
                ));
            }
        };
        let mut seen = Vec::new();
        let mut values = Vec::new();
        for (fname, fexpr, fspan) in fields {
            if seen.contains(fname) {
                return Err(panic(
                    "E0205",
                    format!("duplicate field `{fname}` in `{name}` literal"),
                    *fspan,
                    None,
                ));
            }
            if !decl.fields.iter().any(|f| &f.name == fname) {
                return Err(panic(
                    "E0205",
                    format!("unknown field `{fname}` for struct `{name}`"),
                    *fspan,
                    None,
                ));
            }
            seen.push(fname.clone());
            values.push((fname.clone(), self.eval_expr(fexpr, scopes)?));
        }
        for field in &decl.fields {
            if !seen.contains(&field.name) {
                return Err(panic(
                    "E0205",
                    format!("missing field `{}` in `{name}` literal", field.name),
                    span,
                    None,
                ));
            }
        }
        // Canonical order: declaration order.
        let mut ordered = Vec::with_capacity(values.len());
        for field in &decl.fields {
            let v = values
                .iter()
                .find(|(n, _)| n == &field.name)
                .map(|(_, v)| v.clone())
                .expect("checked above");
            ordered.push((field.name.clone(), v));
        }
        Ok(Value::Struct(Rc::new(RefCell::new(StructVal {
            name: name.to_string(),
            fields: ordered,
        }))))
    }

    fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        scopes: &mut Vec<HashMap<String, Slot>>,
    ) -> Exec<Value> {
        // Evaluate arguments left to right (spec §5.1).
        let mut values = Vec::with_capacity(args.len());
        for a in args {
            values.push(self.eval_expr(a, scopes)?);
        }
        if let ExprKind::Var(name) = &callee.node {
            if let Some(v) = self.lookup(scopes, name) {
                let v = v.clone();
                return self.call_value(&v, values, span, name);
            }
            if let Some(&idx) = self.fn_index.get(name) {
                let target = Value::Func(idx);
                return self.call_value(&target, values, span, name);
            }
            if crate::builtins::is_builtin(name) {
                self.trace.push(TraceFrame {
                    name: format!("{name} (builtin)"),
                    span,
                });
                let result = self
                    .dispatch_builtin(name, values, span)
                    .map_err(|(d, t)| Signal::Panic(d, t));
                if result.is_ok() {
                    self.trace.pop();
                }
                return result;
            }
            return Err(panic(
                "E0208",
                format!("undefined variable `{name}`"),
                callee.span,
                None,
            ));
        }
        let target = self.eval_expr(callee, scopes)?;
        self.call_value(&target, values, span, "<value>")
    }

    fn call_value(
        &mut self,
        target: &Value,
        args: Vec<Value>,
        span: Span,
        display: &str,
    ) -> Exec<Value> {
        match target {
            Value::Func(idx) => self
                .call_fn(*idx, args, span)
                .map_err(|(d, t)| Signal::Panic(d, t)),
            Value::Closure(c) => {
                let c = c.clone();
                let name = display.to_string();
                self.call_closure(&c, args, span, &name)
                    .map_err(|(d, t)| Signal::Panic(d, t))
            }
            other => Err(panic(
                "E0204",
                format!("call of non-function value ({})", other.type_name()),
                span,
                None,
            )),
        }
    }

    fn eval_unary(&self, op: UnOp, v: Value, inner_span: Span, span: Span) -> Exec<Value> {
        let wrap = |r: crate::ops::OpResult<Value>| r.map_err(|(d, t)| Signal::Panic(d, t));
        match op {
            UnOp::Neg => wrap(crate::ops::neg_value(v, inner_span, span)),
            UnOp::Not => wrap(crate::ops::not_value(v, inner_span)),
        }
    }

    fn eval_binary(&mut self, op: BinOp, l: Value, r: Value, span: Span) -> Exec<Value> {
        // Value semantics live in `crate::ops`, shared with the MIR
        // interpreter; fallible ops convert to `Signal::Panic` here.
        let wrap = |r: crate::ops::OpResult<Value>| r.map_err(|(d, t)| Signal::Panic(d, t));
        match op {
            BinOp::Add => wrap(crate::ops::arith(l, r, span, "add", |a, b| {
                a.checked_add(b)
            })),
            BinOp::Sub => wrap(crate::ops::arith(l, r, span, "subtract", |a, b| {
                a.checked_sub(b)
            })),
            BinOp::Mul => wrap(crate::ops::arith(l, r, span, "multiply", |a, b| {
                a.checked_mul(b)
            })),
            BinOp::Div => wrap(crate::ops::div_values(l, r, span)),
            BinOp::Mod => wrap(crate::ops::mod_values(l, r, span)),
            BinOp::Range => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Range(a, b)),
                (a, b) => Err(panic(
                    "E0203",
                    format!(
                        "range bounds must be int, got {} and {}",
                        a.type_name(),
                        b.type_name()
                    ),
                    span,
                    None,
                )),
            },
            BinOp::Eq => wrap(crate::ops::equality(l, r, span, true)),
            BinOp::NotEq => wrap(crate::ops::equality(l, r, span, false)),
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                wrap(crate::ops::ordering(op, l, r, span))
            }
            BinOp::And | BinOp::Or => unreachable!("short-circuit handled in eval_expr"),
        }
    }
}
