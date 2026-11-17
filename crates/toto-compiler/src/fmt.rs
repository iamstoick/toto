//! Canonical formatter (Phase 5, Stage 4): parse and re-emit.
//!
//! The formatter is lossy by design (recorded in `docs/fmt.md`):
//! comments vanish, `pub` drops (it is meaningless in the flat
//! namespace), hex ints and redundant parens normalize away, and
//! long lines are never wrapped. What it guarantees: the output
//! reparses to the same AST (idempotency: `fmt(fmt(x)) == fmt(x)`)
//! and checked programs keep their semantics (the corpus test runs
//! formatted sources against the same goldens).
//!
//! Parenthesization is deliberately conservative: a redundant pair
//! of parens is AST-safe and stable, a missing pair is a miscompile,
//! so the emitter parenthesizes whenever the parse rule is not
//! obviously invertible.

use crate::ast::*;

// Tightness levels, mirroring `parser.rs` precedences.
const PREC_SEND: u8 = 5;
const PREC_OR: u8 = 10;
const PREC_AND: u8 = 20;
const PREC_EQ: u8 = 30;
const PREC_CMP: u8 = 40;
const PREC_RANGE: u8 = 45;
const PREC_ADD: u8 = 50;
const PREC_MUL: u8 = 60;
const PREC_UNARY: u8 = 70;
const PREC_POSTFIX: u8 = 80;
// `<-ch`: binds its operand tighter than any binary operator but is
// itself looser than one (see below).
const PREC_RECV: u8 = 4;

fn bin_prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => PREC_OR,
        BinOp::And => PREC_AND,
        BinOp::Eq | BinOp::NotEq => PREC_EQ,
        BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => PREC_CMP,
        BinOp::Range => PREC_RANGE,
        BinOp::Add | BinOp::Sub => PREC_ADD,
        BinOp::Mul | BinOp::Div | BinOp::Mod => PREC_MUL,
    }
}

fn bin_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Or => "||",
        BinOp::And => "&&",
        BinOp::Eq => "==",
        BinOp::NotEq => "!=",
        BinOp::Lt => "<",
        BinOp::LtEq => "<=",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        BinOp::Range => "..",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}

/// Format a whole program (one file; imports are not resolved).
pub fn format_program(prog: &Program) -> String {
    let mut f = F {
        out: String::new(),
        indent: 0,
    };
    for (i, item) in prog.items.iter().enumerate() {
        if i > 0 {
            f.out.push('\n');
        }
        f.emit_item(item);
    }
    f.out
}

struct F {
    out: String,
    indent: usize,
}

impl F {
    fn pad(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn line(&mut self, s: &str) {
        self.pad();
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn emit_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(d) => self.emit_fn(d),
            ItemKind::Struct(d) => self.emit_struct(d),
            ItemKind::Import(d) => {
                self.line(&format!("import \"{}\";", escape(&d.path)));
            }
            ItemKind::Extern(b) => self.emit_extern(b),
            ItemKind::State(d) => self.emit_state(d),
            ItemKind::Migrate(d) => self.emit_migrate(d),
        }
    }

    /// Format a `state { ... }` block (spec amendment, Phase 6):
    /// bindings in declaration order, initializers inline. `pub` is
    /// accepted by the parser but has no effect, so it is dropped
    /// here like everywhere else (see the `fmt` pub-drop rule).
    fn emit_state(&mut self, d: &StateDecl) {
        self.emit_annotations(&d.annotations);
        self.line("state {");
        self.indent += 1;
        for b in &d.bindings {
            let mut h = String::new();
            h.push_str(&b.name);
            h.push_str(": ");
            h.push_str(&emit_ty(&b.ty));
            match &b.init {
                Some(init) => {
                    h.push_str(" = ");
                    self.pad();
                    self.out.push_str(&h);
                    self.emit_expr(init, 0, false);
                    self.out.push_str(";\n");
                }
                None => {
                    h.push(';');
                    self.line(&h);
                }
            }
        }
        self.indent -= 1;
        self.line("}");
    }

    /// Format a `migrate fn ...` item: the `migrate` keyword plus an
    /// ordinary function rendering.
    fn emit_migrate(&mut self, d: &MigrateDecl) {
        self.emit_annotations(&d.annotations);
        // `emit_fn` renders the annotations itself; the migrate item
        // holds a clone, so render the body manually to avoid
        // duplicating them.
        let mut head = String::from("migrate fn ");
        head.push_str(&d.func.name);
        head.push('(');
        for (i, p) in d.func.params.iter().enumerate() {
            if i > 0 {
                head.push_str(", ");
            }
            head.push_str(&emit_param(p));
        }
        head.push_str(") -> ");
        head.push_str(&emit_ty(&d.func.ret));
        head.push_str(" {");
        self.line(&head);
        self.emit_block_body(&d.func.body);
        self.line("}");
    }

    fn emit_annotations(&mut self, anns: &[Annotation]) {
        for a in anns {
            let mut s = String::from("@");
            s.push_str(&a.name);
            if !a.args.is_empty() {
                s.push('(');
                for (i, arg) in a.args.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(&emit_annotation_arg(arg));
                }
                s.push(')');
            }
            self.line(&s);
        }
    }

    fn emit_fn(&mut self, d: &FnDecl) {
        self.emit_annotations(&d.annotations);
        let mut head = String::from("fn ");
        head.push_str(&d.name);
        head.push('(');
        for (i, p) in d.params.iter().enumerate() {
            if i > 0 {
                head.push_str(", ");
            }
            head.push_str(&emit_param(p));
        }
        head.push_str(") -> ");
        head.push_str(&emit_ty(&d.ret));
        head.push_str(" {");
        self.line(&head);
        self.emit_block_body(&d.body);
        self.line("}");
    }

    fn emit_struct(&mut self, d: &StructDecl) {
        self.emit_annotations(&d.annotations);
        let mut head = String::from("struct ");
        head.push_str(&d.name);
        head.push_str(" { ");
        for (i, f) in d.fields.iter().enumerate() {
            if i > 0 {
                head.push_str(", ");
            }
            head.push_str(&f.name);
            head.push_str(": ");
            head.push_str(&emit_ty(&f.ty));
        }
        head.push_str(" }");
        self.line(&head);
    }

    fn emit_extern(&mut self, b: &ExternBlock) {
        self.line(&format!("unsafe extern \"{}\" {{", escape(&b.abi)));
        self.indent += 1;
        for d in &b.decls {
            let mut s = String::from("fn ");
            s.push_str(&d.name);
            s.push('(');
            for (i, p) in d.params.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&p.name);
                s.push_str(": ");
                s.push_str(&emit_ty(&p.ty));
            }
            s.push_str(") -> ");
            s.push_str(&emit_ty(&d.ret));
            s.push(';');
            self.line(&s);
        }
        self.indent -= 1;
        self.line("}");
    }

    /// Block contents without the braces (the caller prints those).
    fn emit_block_body(&mut self, b: &Block) {
        self.indent += 1;
        for s in &b.stmts {
            self.emit_stmt(s);
        }
        if let Some(t) = &b.tail {
            self.pad();
            self.emit_expr(t, 0, false);
            self.out.push('\n');
        }
        self.indent -= 1;
    }

    fn emit_block(&mut self, b: &Block) {
        self.out.push_str("{\n");
        self.emit_block_body(b);
        self.pad();
        self.out.push('}');
    }

    fn emit_stmt(&mut self, s: &Stmt) {
        match &s.node {
            StmtKind::Let {
                name,
                name_span: _,
                ty,
                is_mut,
                value,
            } => {
                let mut h = String::from("let ");
                if *is_mut {
                    h.push_str("mut ");
                }
                h.push_str(name);
                if let Some(t) = ty {
                    h.push_str(": ");
                    h.push_str(&emit_ty(t));
                }
                h.push_str(" = ");
                self.pad();
                self.out.push_str(&h);
                self.emit_expr(value, 0, false);
                self.out.push_str(";\n");
            }
            StmtKind::Assign { target, value } => {
                self.pad();
                self.emit_assign_target(target);
                self.out.push_str(" = ");
                self.emit_expr(value, 0, false);
                self.out.push_str(";\n");
            }
            StmtKind::Expr(e) => {
                self.pad();
                let bare_if = matches!(&e.node, ExprKind::If { .. });
                self.emit_expr(e, 0, false);
                // A statement-position `if` never takes `;` (the
                // parser accepts it with or without one, and the AST
                // does not record which; canonical is bare).
                if !bare_if {
                    self.out.push(';');
                }
                self.out.push('\n');
            }
            StmtKind::Return(e) => {
                self.pad();
                self.out.push_str("return ");
                self.emit_expr(e, 0, false);
                self.out.push_str(";\n");
            }
            StmtKind::Break => self.line("break;"),
            StmtKind::Continue => self.line("continue;"),
            StmtKind::While { cond, body } => {
                self.pad();
                self.out.push_str("while ");
                self.emit_expr(cond, 0, true);
                self.out.push(' ');
                self.emit_block(body);
                self.out.push('\n');
            }
            StmtKind::For {
                name,
                name_span: _,
                iter,
                body,
            } => {
                self.pad();
                self.out.push_str("for ");
                self.out.push_str(name);
                self.out.push_str(" in ");
                self.emit_expr(iter, 0, true);
                self.out.push(' ');
                self.emit_block(body);
                self.out.push('\n');
            }
            StmtKind::Go { body } => {
                self.pad();
                self.out.push_str("go ");
                self.emit_block(body);
                self.out.push('\n');
            }
            StmtKind::Select { cases, default } => {
                self.line("select {");
                self.indent += 1;
                for c in cases {
                    match c {
                        SelectCase::Recv { name, chan, body } => {
                            self.pad();
                            if let Some((n, _)) = name {
                                self.out.push_str("let ");
                                self.out.push_str(n);
                                self.out.push_str(" = ");
                            }
                            self.out.push_str("<-");
                            self.emit_expr(chan, 0, false);
                            self.out.push(' ');
                            self.emit_block(body);
                            self.out.push('\n');
                        }
                        SelectCase::Send { chan, value, body } => {
                            self.pad();
                            self.emit_expr(chan, PREC_SEND + 1, false);
                            self.out.push_str(" <- ");
                            self.emit_expr(value, 0, false);
                            self.out.push(' ');
                            self.emit_block(body);
                            self.out.push('\n');
                        }
                    }
                }
                if let Some(d) = default {
                    self.pad();
                    self.out.push_str("default ");
                    self.emit_block(d);
                    self.out.push('\n');
                }
                self.indent -= 1;
                self.line("}");
            }
        }
    }

    fn emit_assign_target(&mut self, t: &AssignTarget) {
        match t {
            AssignTarget::Var(name, _) => self.out.push_str(name),
            AssignTarget::Field(base, name, _) => {
                self.emit_expr(base, PREC_POSTFIX, false);
                self.out.push('.');
                self.out.push_str(name);
            }
            AssignTarget::Index(base, index, _) => {
                self.emit_expr(base, PREC_POSTFIX, false);
                self.out.push('[');
                self.emit_expr(index, 0, false);
                self.out.push(']');
            }
        }
    }

    /// Emit an expression valid at `min_prec` in a `no_struct`
    /// context (see the module docs for why each paren exists).
    fn emit_expr(&mut self, e: &Expr, min_prec: u8, no_struct: bool) {
        let prec = expr_prec(&e.node);
        // An empty struct literal in a `no_struct` position (a
        // `while` condition, a `for` iterable) would reparse as a
        // control-flow body; parenthesize it. Field-carrying
        // literals are unambiguous (the `field :` lookahead).
        let need_ns_paren =
            no_struct && matches!(&e.node, ExprKind::StructLit { fields, .. } if fields.is_empty());
        if prec < min_prec || need_ns_paren {
            self.out.push('(');
            self.emit_expr_inner(e, no_struct);
            self.out.push(')');
        } else {
            self.emit_expr_inner(e, no_struct);
        }
    }

    fn emit_expr_inner(&mut self, e: &Expr, no_struct: bool) {
        match &e.node {
            ExprKind::Int(n) => self.out.push_str(&n.to_string()),
            ExprKind::Float(v) => self.out.push_str(&format!("{v:?}")),
            ExprKind::Str(s) => {
                self.out.push('"');
                self.out.push_str(&escape(s));
                self.out.push('"');
            }
            ExprKind::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            ExprKind::Unit => self.out.push_str("()"),
            ExprKind::Var(name) => self.out.push_str(name),
            ExprKind::None => self.out.push_str("None"),
            ExprKind::Ok(x) => {
                self.out.push_str("Ok(");
                self.emit_expr(x, 0, false);
                self.out.push(')');
            }
            ExprKind::Err(x) => {
                self.out.push_str("Err(");
                self.emit_expr(x, 0, false);
                self.out.push(')');
            }
            ExprKind::Some(x) => {
                self.out.push_str("Some(");
                self.emit_expr(x, 0, false);
                self.out.push(')');
            }
            ExprKind::Binary { op, left, right } => {
                let p = bin_prec(*op);
                self.emit_expr(left, p, no_struct);
                self.out.push(' ');
                self.out.push_str(bin_text(*op));
                self.out.push(' ');
                self.emit_expr(right, p + 1, no_struct);
            }
            ExprKind::Range(a, b) => {
                self.emit_expr(a, PREC_RANGE, no_struct);
                self.out.push_str("..");
                self.emit_expr(b, PREC_RANGE + 1, no_struct);
            }
            ExprKind::Unary { op, expr } => {
                self.out.push_str(match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                });
                self.emit_expr(expr, PREC_UNARY, no_struct);
            }
            ExprKind::Send { chan, value } => {
                // `chan` parsed at min-prec 6 and `value` greedily,
                // so neither child can be a bare `Send`: emit bare.
                self.emit_expr(chan, PREC_SEND + 1, no_struct);
                self.out.push_str(" <- ");
                self.emit_expr(value, PREC_SEND + 1, no_struct);
            }
            ExprKind::Recv { chan } => {
                self.out.push_str("<-");
                self.emit_recv_chan(chan, no_struct);
            }
            ExprKind::Question(x) => {
                self.emit_question_base(x, no_struct);
                self.out.push('?');
            }
            ExprKind::Call { callee, args } => {
                self.emit_callee(callee, no_struct);
                self.out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.emit_expr(a, 0, false);
                }
                self.out.push(')');
            }
            ExprKind::Field { base, name } => {
                self.emit_field_base(base, no_struct);
                self.out.push('.');
                self.out.push_str(name);
            }
            ExprKind::Index { base, index } => {
                self.emit_field_base(base, no_struct);
                self.out.push('[');
                self.emit_expr(index, 0, false);
                self.out.push(']');
            }
            ExprKind::If {
                cond,
                then_b,
                else_b,
            } => {
                self.out.push_str("if ");
                // `no_struct` propagates into conditions (a `while`
                // condition holding an `if`); `else if` branches
                // reset it, exactly like `parse_if`.
                self.emit_expr(cond, 0, no_struct);
                self.out.push(' ');
                self.emit_block(then_b);
                self.emit_else(else_b);
            }
            ExprKind::Closure { params, ret, body } => {
                self.out.push_str("fn(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&emit_param(p));
                }
                self.out.push_str(") -> ");
                self.out.push_str(&emit_ty(ret));
                self.out.push(' ');
                self.emit_block(body);
            }
            ExprKind::StructLit { name, fields, .. } => {
                self.out.push_str(name);
                self.out.push_str(" { ");
                for (i, (fname, value, _)) in fields.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(fname);
                    self.out.push_str(": ");
                    self.emit_expr(value, 0, false);
                }
                self.out.push_str(" }");
            }
            ExprKind::Array(elems) => {
                self.out.push('[');
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.emit_expr(e, 0, false);
                }
                self.out.push(']');
            }
            ExprKind::ChanNew { elem, cap } => {
                self.out.push_str("chan<");
                self.out.push_str(&emit_ty(elem));
                self.out.push_str(">(");
                if let Some(c) = cap {
                    self.emit_expr(c, 0, false);
                }
                self.out.push(')');
            }
        }
    }

    /// `<-` operand: binds tighter than any binary operator, so only
    /// a binary/`Send` child would escape — parenthesize those.
    fn emit_recv_chan(&mut self, chan: &Expr, no_struct: bool) {
        match &chan.node {
            ExprKind::Binary { .. } | ExprKind::Send { .. } => {
                self.out.push('(');
                self.emit_expr(chan, 0, no_struct);
                self.out.push(')');
            }
            _ => self.emit_expr(chan, 0, no_struct),
        }
    }

    /// `?` applies to a postfix operand; anything looser (binary,
    /// `Send`, unary, `<-`, `if`, closure) would reparse differently
    /// without parens.
    fn emit_question_base(&mut self, base: &Expr, no_struct: bool) {
        match &base.node {
            ExprKind::Binary { .. }
            | ExprKind::Range(..)
            | ExprKind::Send { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Recv { .. }
            | ExprKind::If { .. }
            | ExprKind::Closure { .. } => {
                self.out.push('(');
                self.emit_expr(base, 0, no_struct);
                self.out.push(')');
            }
            _ => self.emit_expr(base, 0, no_struct),
        }
    }

    /// Call callee: anything looser than postfix reparses
    /// differently, and a bare `a.b` would trip the v1 method-call
    /// rejection (`E0158`), so fields and `if`s parenthesize too.
    fn emit_callee(&mut self, callee: &Expr, no_struct: bool) {
        match &callee.node {
            ExprKind::Field { .. } | ExprKind::If { .. } => {
                self.out.push('(');
                self.emit_expr(callee, 0, no_struct);
                self.out.push(')');
            }
            _ => self.emit_expr(callee, PREC_POSTFIX, no_struct),
        }
    }

    /// `.`/`[` base: looser constructs reparse differently; `if` and
    /// closure bases reattach fine without parens.
    fn emit_field_base(&mut self, base: &Expr, no_struct: bool) {
        match &base.node {
            ExprKind::Binary { .. }
            | ExprKind::Range(..)
            | ExprKind::Send { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Recv { .. } => {
                self.out.push('(');
                self.emit_expr(base, 0, no_struct);
                self.out.push(')');
            }
            _ => self.emit_expr(base, PREC_POSTFIX, no_struct),
        }
    }

    /// `else` branch: an else holding exactly one trailing `if`
    /// prints as `else if` (same AST as the braced form, by the
    /// `parse_if` chain rule); anything else gets braces.
    fn emit_else(&mut self, else_b: &Option<Box<Block>>) {
        let Some(b) = else_b else {
            return;
        };
        if b.stmts.is_empty()
            && let Some(tail) = &b.tail
            && matches!(&tail.node, ExprKind::If { .. })
        {
            self.out.push_str(" else ");
            // `else if` branches parse with `no_struct = false`
            // (`parse_if` hardcodes it), so reset here.
            self.emit_expr(tail, 0, false);
        } else {
            self.out.push_str(" else ");
            self.emit_block(b);
        }
    }
}

/// Tightness of an emitted expression (see `bin_prec`).
fn expr_prec(node: &ExprKind) -> u8 {
    match node {
        ExprKind::Send { .. } => PREC_SEND,
        ExprKind::Binary { op, .. } => bin_prec(*op),
        ExprKind::Range(..) => PREC_RANGE,
        ExprKind::Unary { .. } => PREC_UNARY,
        ExprKind::Recv { .. } => PREC_RECV,
        _ => PREC_POSTFIX_OR_ATOM,
    }
}

/// Postfix-or-tighter: calls, fields, indices, `?`, and every
/// construct that reparses fine as a binary operand without parens
/// (`if`, closures, literals, constructors).
const PREC_POSTFIX_OR_ATOM: u8 = PREC_POSTFIX;

fn emit_ty(ty: &Type) -> String {
    match &ty.kind {
        TypeKind::Ptr(inner) => format!("*{}", emit_ty(inner)),
        TypeKind::Named { name, args } => {
            if args.is_empty() {
                name.clone()
            } else {
                let inner: Vec<String> = args.iter().map(emit_ty).collect();
                format!("{name}<{}>", inner.join(", "))
            }
        }
        TypeKind::Fn { params, ret } => {
            let inner: Vec<String> = params.iter().map(emit_ty).collect();
            format!("fn({}) -> {}", inner.join(", "), emit_ty(ret))
        }
    }
}

fn emit_param(p: &Param) -> String {
    format!(
        "{}{}: {}",
        if p.is_mut { "mut " } else { "" },
        p.name,
        emit_ty(&p.ty)
    )
}

/// One annotation argument: re-quote only when the text would not
/// parse back to the same argument (opaque strings are never
/// evaluated, so spelling is free as long as it reparses).
fn emit_annotation_arg(arg: &str) -> String {
    let bare = !arg.is_empty()
        && (arg == "true"
            || arg == "false"
            || arg.parse::<i64>().is_ok()
            || arg.parse::<f64>().is_ok()
            || (arg
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
                && arg.chars().all(|c| c.is_alphanumeric() || c == '_')));
    if bare {
        arg.to_string()
    } else {
        format!("\"{}\"", escape(arg))
    }
}

/// Re-escape a string literal body: exactly the lexer's five
/// escapes invert; everything else (including raw control bytes)
/// passes through verbatim so any accepted source round-trips.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}
