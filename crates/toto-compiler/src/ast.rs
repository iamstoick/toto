//! Abstract syntax tree with spans on every node (spec §3).

use crate::span::Span;

/// A node with its source span.
#[derive(Clone, Debug)]
pub struct Sp<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Sp<T> {
    pub fn new(node: T, span: Span) -> Self {
        Self { node, span }
    }
}

pub type Expr = Sp<ExprKind>;
pub type Stmt = Sp<StmtKind>;

#[derive(Clone, Debug)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Fn(FnDecl),
    Struct(StructDecl),
}

#[derive(Clone, Debug)]
pub struct Annotation {
    pub name: String,
    pub args: Vec<String>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub ret: Type,
    pub body: Box<Block>,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub name_span: Span,
    pub ty: Type,
    pub is_mut: bool,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<StructField>,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug)]
pub struct StructField {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeKind {
    Named {
        name: String,
        args: Vec<Type>,
    },
    /// Function types, provisional spelling `fn(T1, T2) -> R` (see `hir::Ty`).
    Fn {
        params: Vec<Type>,
        ret: Box<Type>,
    },
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Let {
        name: String,
        name_span: Span,
        ty: Option<Type>,
        is_mut: bool,
        value: Expr,
    },
    Assign {
        target: AssignTarget,
        value: Expr,
    },
    Expr(Expr),
    Return(Expr),
    Break,
    Continue,
    While {
        cond: Expr,
        body: Box<Block>,
    },
    For {
        name: String,
        name_span: Span,
        iter: Expr,
        body: Box<Block>,
    },
    /// `go <block>`: spawn a child task in the enclosing scope (Phase 3).
    /// Yields `unit`; completion is observed through scopes and channels.
    Go {
        body: Box<Block>,
    },
    /// `select` with guarded cases and an optional trailing `default`.
    Select {
        cases: Vec<SelectCase>,
        default: Option<Box<Block>>,
    },
}

/// One `select` case. Recv guards bind with `let` or discard; send
/// guards commit a value. Bodies run when the case is selected.
#[derive(Clone, Debug)]
pub enum SelectCase {
    Recv {
        /// `let v = <-ch` binding, or `None` for a discarding `<-ch`.
        name: Option<(String, Span)>,
        chan: Expr,
        body: Box<Block>,
    },
    Send {
        chan: Expr,
        value: Expr,
        body: Box<Block>,
    },
}

#[derive(Clone, Debug)]
pub enum AssignTarget {
    Var(String, Span),
    Field(Box<Expr>, String, Span),
    Index(Box<Expr>, Box<Expr>, Span),
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Unit,
    /// Unresolved name; also used as a recovery placeholder (never executed
    /// when static errors exist).
    Var(String),
    If {
        cond: Box<Expr>,
        then_b: Box<Block>,
        else_b: Option<Box<Block>>,
    },
    Closure {
        params: Vec<Param>,
        ret: Type,
        body: Box<Block>,
    },
    StructLit {
        name: String,
        fields: Vec<(String, Expr, Span)>,
    },
    Array(Vec<Expr>),
    Range(Box<Expr>, Box<Expr>),
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Ok(Box<Expr>),
    Err(Box<Expr>),
    Some(Box<Expr>),
    None,
    /// `ch <- v`: send (an expression yielding `unit` so it also fits
    /// select guards; normally used as a statement).
    Send {
        chan: Box<Expr>,
        value: Box<Expr>,
    },
    /// `<-ch`: receive, yielding the element value.
    Recv {
        chan: Box<Expr>,
    },
    /// `chan<T>()` / `chan<T>(n)`: channel construction.
    ChanNew {
        elem: Box<Type>,
        cap: Option<Box<Expr>>,
    },
    Question(Box<Expr>),
    Field {
        base: Box<Expr>,
        name: String,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Range,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}
