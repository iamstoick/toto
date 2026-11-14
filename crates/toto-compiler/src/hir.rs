//! HIR: typed, name-resolved form of the AST (Phase 2, Stage 2).
//! Same language-level shapes as the AST; every expression carries its
//! checked type and every variable use carries its resolution.

use std::collections::HashMap;
use std::fmt;

use crate::ast::{BinOp, UnOp};
use crate::span::Span;

/// A checked Toto type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    String,
    Bool,
    Unit,
    Range,
    Array(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    /// `chan<T>`: channel carrying values of type `T`.
    Chan(Box<Ty>),
    Struct(String),
    /// Function types. Spelling `fn(T1, T2) -> R` is PROVISIONAL (Phase 2
    /// report): the core subset has first-class functions (spec §4,
    /// `apply` in the corpus) but no anno­table function-type syntax, so a
    /// minimal spelling was chosen to keep `apply(f: F, …)` checkable.
    /// Confirm or rename — the spelling is isolated to the type parser.
    Fn(Vec<Ty>, Box<Ty>),
}

impl Ty {
    pub fn is_value_type(&self) -> bool {
        !matches!(self, Ty::Fn(_, _))
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::String => write!(f, "string"),
            Ty::Bool => write!(f, "bool"),
            Ty::Unit => write!(f, "unit"),
            Ty::Range => write!(f, "Range"),
            Ty::Array(t) => write!(f, "Array<{t}>"),
            Ty::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Ty::Result(t, e) => write!(f, "Result<{t}, {e}>"),
            Ty::Option(t) => write!(f, "Option<{t}>"),
            Ty::Chan(t) => write!(f, "chan<{t}>"),
            Ty::Struct(name) => write!(f, "{name}"),
            Ty::Fn(params, ret) => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}")
            }
        }
    }
}

/// What a variable use resolved to (scope structure settled by the checker).
/// Locals and parameters share one form: both live in lexical scopes and
/// resolve by name at runtime. The recorded depth lets MIR closure
/// conversion tell captures (bound outside) from body locals.
#[derive(Clone, Debug)]
pub enum VarRef {
    /// Lexical binding: block `let` or function parameter.
    Local {
        name: String,
        mutable: bool,
        depth: u32,
    },
    /// Top-level function (index into the program function table).
    GlobalFn { name: String, idx: usize },
}

#[derive(Clone, Debug)]
pub struct HProgram {
    pub items: Vec<HItem>,
}

#[derive(Clone, Debug)]
pub struct HItem {
    pub kind: HItemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum HItemKind {
    Fn(HFn),
    Struct(HStructDecl),
}

#[derive(Clone, Debug)]
pub struct HFn {
    pub name: String,
    pub params: Vec<(String, Ty, bool)>,
    pub ret: Ty,
    pub body: HBlock,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HStructDecl {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HBlock {
    pub stmts: Vec<HStmt>,
    pub tail: Option<HExpr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HStmt {
    pub kind: HStmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum HStmtKind {
    Let {
        name: String,
        ty: Ty,
        is_mut: bool,
        value: HExpr,
    },
    Assign {
        target: HAssignTarget,
        value: HExpr,
    },
    Expr(HExpr),
    Return(HExpr),
    Break,
    Continue,
    While {
        cond: HExpr,
        body: Box<HBlock>,
    },
    For {
        name: String,
        var_ty: Ty,
        iter: HExpr,
        body: Box<HBlock>,
    },
    /// `go <block>`: spawn a child task. Completion is observed through
    /// scopes and channels; the statement yields `unit`.
    Go {
        body: Box<HBlock>,
        /// Names bound outside the `go` body that it uses (immutable only;
        /// `mut` captures are rejected by the checker).
        captured: Vec<(String, Ty)>,
    },
    /// `select` with guarded cases and an optional trailing `default`.
    Select {
        cases: Vec<HSelectCase>,
        default: Option<Box<HBlock>>,
    },
}

/// One checked `select` case.
#[derive(Clone, Debug)]
pub enum HSelectCase {
    Recv {
        /// `let v = <-ch` binding, or `None` for a discarding `<-ch`.
        name: Option<(String, Span)>,
        chan: HExpr,
        body: Box<HBlock>,
    },
    Send {
        chan: HExpr,
        value: HExpr,
        body: Box<HBlock>,
    },
}

#[derive(Clone, Debug)]
pub enum HAssignTarget {
    /// Name targets carry their resolution: the depth tells MIR closure
    /// conversion whether the target is a capture or a body local.
    Var(String, VarRef, Span),
    Field(Box<HExpr>, String, Span),
    Index(Box<HExpr>, Box<HExpr>, Span),
}

#[derive(Clone, Debug)]
pub struct HExpr {
    pub ty: Ty,
    pub span: Span,
    pub kind: HExprKind,
}

#[derive(Clone, Debug)]
pub enum HExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Unit,
    Var(String, VarRef),
    If {
        cond: Box<HExpr>,
        then_b: Box<HBlock>,
        else_b: Option<Box<HBlock>>,
    },
    Closure {
        params: Vec<(String, Ty, bool)>,
        ret: Ty,
        body: Box<HBlock>,
        /// Names bound outside the closure that its body uses, with types.
        /// Computed from binding depths (see `check::captured_vars`).
        captured: Vec<(String, Ty)>,
    },
    StructLit {
        name: String,
        fields: Vec<(String, HExpr, Span)>,
    },
    Array(Vec<HExpr>),
    Range(Box<HExpr>, Box<HExpr>),
    Binary {
        op: BinOp,
        left: Box<HExpr>,
        right: Box<HExpr>,
    },
    Unary {
        op: UnOp,
        expr: Box<HExpr>,
    },
    Call {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
    },
    /// Direct call to a frozen builtin (never a first-class value).
    CallBuiltin {
        name: String,
        args: Vec<HExpr>,
    },
    Field {
        base: Box<HExpr>,
        name: String,
    },
    Index {
        base: Box<HExpr>,
        index: Box<HExpr>,
    },
    Ok(Box<HExpr>),
    Err(Box<HExpr>),
    Some(Box<HExpr>),
    None,
    /// `ch <- v`: send (an expression yielding `unit`).
    Send {
        chan: Box<HExpr>,
        value: Box<HExpr>,
    },
    /// `<-ch`: receive.
    Recv {
        chan: Box<HExpr>,
    },
    /// `chan<T>()` / `chan<T>(n)`: channel construction.
    ChanNew {
        elem_ty: Ty,
        cap: Option<Box<HExpr>>,
    },
    /// `operand?` with resolved types: `inner` is the unwrapped type,
    /// `fn_ret` the enclosing function's return type (conversion target).
    Question {
        operand: Box<HExpr>,
        inner: Ty,
        fn_ret: Ty,
    },
}

/// Function signature table shared by checker, MIR builder, and dumps.
#[derive(Clone, Debug)]
pub struct FnSig {
    pub name: String,
    pub params: Vec<(String, Ty, bool)>,
    pub ret: Ty,
    pub idx: usize,
}

#[derive(Clone, Debug)]
pub struct CheckedProgram {
    pub program: HProgram,
    pub fns: Vec<FnSig>,
    pub structs: HashMap<String, HStructDecl>,
    /// True when the program uses concurrency (`go`, channels, `select`).
    /// The CLI routes such programs to the runtime executor; sequential
    /// programs keep running on the tree-walking interpreter.
    pub uses_concurrency: bool,
}
