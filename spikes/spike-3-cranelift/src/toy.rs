//! Toy IR: hand-constructed Rust data structures (no parser).
//!
//! Everything is i64 (ints untagged; heap handles travel as u64 bit
//! patterns in i64 values). Functions take `ctx` first, then `params`
//! args. Root discipline: `Alloc*` returns UNROOTED handles (Spike 1
//! raw-alloc rule); `Keep` publishes a value to the shadow stack and
//! yields it; callees push, callers and the top-level test reset the
//! shadow afterwards. A value never published is collectable even
//! while sitting in a JIT SSA value — the missed-root control.

pub type FuncId = u32;

#[derive(Clone, Debug)]
pub struct ToyFunc {
    pub name: String,
    pub params: u32,
    pub body: Expr,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Lit(i64),
    Arg(u32),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    /// Unsigned division. Traps on a zero divisor (trap test only).
    Div(Box<Expr>, Box<Expr>),
    /// 1 when equal, else 0.
    Eq(Box<Expr>, Box<Expr>),
    /// 1 when left < right (signed), else 0.
    Lt(Box<Expr>, Box<Expr>),
    IfElse {
        cond: Box<Expr>,
        then_: Box<Expr>,
        else_: Box<Expr>,
    },
    Call(FuncId, Vec<Expr>),
    /// Evaluate `body` `times` times; value is the last result
    /// (0 when `times <= 0`). `Index` is valid only directly inside
    /// `body` and yields the 0-based iteration.
    Loop {
        times: Box<Expr>,
        body: Box<Expr>,
    },
    Index,
    /// Evaluate each in order; value is the last.
    Seq(Vec<Expr>),
    /// Allocate an int cell. Unrooted.
    AllocInt(Box<Expr>),
    /// Allocate a pair of the two (encoded-handle) values. Unrooted.
    AllocPair(Box<Expr>, Box<Expr>),
    /// Read pair field `idx` (0/1). Yields the encoded field handle.
    ReadField(Box<Expr>, u32),
    /// Overwrite pair field; yields 1/0 for stale-or-not.
    WriteField(Box<Expr>, u32, Box<Expr>),
    /// Int cell value, or `default` when the handle is dead.
    GetIntOr(Box<Expr>, i64),
    /// Bind a name for the body. Values computed once and used
    /// across a safepoint (e.g. a keeper read after a GC) need this;
    /// `Seq` alone cannot reference earlier results.
    Let(u32, Box<Expr>, Box<Expr>),
    /// Reference a `Let`-bound name.
    Var(u32),
    /// Publish a value to the shadow stack; yields it.
    Keep(Box<Expr>),
    /// Explicit safepoint: collect now; yields live object count.
    GcCollect,
}

/// Constructor names mirror the operators they build (`add` builds
/// `Add`, etc.). Deliberately *not* the `std::ops` traits: these take
/// and return toy `Expr`, not `Self` arithmetic.
#[allow(clippy::should_implement_trait)]
impl Expr {
    pub fn lit(v: i64) -> Self {
        Expr::Lit(v)
    }
    pub fn arg(i: u32) -> Self {
        Expr::Arg(i)
    }
    pub fn add(a: Expr, b: Expr) -> Self {
        Expr::Add(Box::new(a), Expr::wrap(b))
    }
    pub fn sub(a: Expr, b: Expr) -> Self {
        Expr::Sub(Box::new(a), Expr::wrap(b))
    }
    pub fn mul(a: Expr, b: Expr) -> Self {
        Expr::Mul(Box::new(a), Expr::wrap(b))
    }
    pub fn div(a: Expr, b: Expr) -> Self {
        Expr::Div(Box::new(a), Expr::wrap(b))
    }
    pub fn eq(a: Expr, b: Expr) -> Self {
        Expr::Eq(Box::new(a), Expr::wrap(b))
    }
    pub fn lt(a: Expr, b: Expr) -> Self {
        Expr::Lt(Box::new(a), Expr::wrap(b))
    }
    pub fn call(id: FuncId, args: Vec<Expr>) -> Self {
        Expr::Call(id, args)
    }

    pub fn let_(name: u32, val: Expr, body: Expr) -> Self {
        Expr::Let(name, Box::new(val), Expr::wrap(body))
    }

    pub fn var(name: u32) -> Self {
        Expr::Var(name)
    }
    fn wrap(e: Expr) -> Box<Expr> {
        Box::new(e)
    }
}
