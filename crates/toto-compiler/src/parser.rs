//! Pratt parser for the core subset (spec §3). Out-of-subset constructs
//! are rejected with phase-naming diagnostics; recovery synchronizes at
//! statement boundaries so one file can report many errors.

use crate::ast::*;
use crate::diag::Diag;
use crate::span::Span;
use crate::token::{Token, TokenKind};

type PResult<T> = Result<T, ()>;

/// What one statement parse produces: a statement, or a block tail.
enum StmtOut {
    Stmt(Stmt),
    Tail(Expr),
}

const MAX_ERRORS: usize = 16;
const MAX_DEPTH: u32 = 500;

pub struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    errors: Vec<Diag>,
    depth: u32,
    loop_depth: u32,
    src: &'a str,
}

impl<'a> Parser<'a> {
    pub fn new(toks: Vec<Token>, src: &'a str) -> Self {
        Self {
            toks,
            pos: 0,
            errors: Vec::new(),
            depth: 0,
            loop_depth: 0,
            src,
        }
    }

    fn peek(&self) -> &Token {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn eof_span(&self) -> Span {
        self.toks.last().map(|t| t.span).unwrap_or(Span::new(0, 0))
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn error(&mut self, code: &'static str, message: String, span: Span, help: Option<String>) {
        if self.errors.len() >= MAX_ERRORS {
            return;
        }
        self.errors.push(Diag {
            code,
            message,
            span,
            help,
        });
        if self.errors.len() == MAX_ERRORS {
            self.errors.push(Diag {
                code: "E0109",
                message: "too many errors; stopping".to_string(),
                span: self.eof_span(),
                help: None,
            });
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// Skip to a statement boundary for recovery. Returns when positioned
    /// at `;` (consumed), `}`/EOF (not consumed), or an item start.
    fn sync_stmt(&mut self) {
        while !self.at_eof() {
            match self.peek().kind {
                TokenKind::Semi => {
                    self.bump();
                    return;
                }
                TokenKind::RBrace => return,
                TokenKind::Fn | TokenKind::Struct | TokenKind::At => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Skip a balanced `{...}` (the opening brace is consumed here).
    fn skip_balanced(&mut self) {
        let mut depth = 0u32;
        loop {
            if self.at_eof() {
                return;
            }
            match self.bump().kind {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }

    fn expect(&mut self, want: fn(&TokenKind) -> bool, what: &str) -> PResult<Token> {
        if want(&self.peek().kind) {
            Ok(self.bump())
        } else {
            let span = self.peek().span;
            self.error(
                "E0101",
                format!("expected {what}, found {}", self.peek().kind.name()),
                span,
                None,
            );
            Err(())
        }
    }

    fn expect_semi(&mut self, after: &str) -> PResult<()> {
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.bump();
            Ok(())
        } else {
            let span = self.peek().span;
            self.error(
                "E0102",
                format!("expected `;` {after}, found {}", self.peek().kind.name()),
                span,
                Some("add `;` at the end of the statement".to_string()),
            );
            Err(())
        }
    }

    fn expect_close(&mut self, open: Span, close: &TokenKind, what: &str) -> PResult<Token> {
        if std::mem::discriminant(&self.peek().kind) == std::mem::discriminant(close) {
            Ok(self.bump())
        } else {
            let span = self.peek().span;
            let (line, col) = open.line_col(self.src);
            self.error(
                "E0103",
                format!("unclosed `{what}` opened at {line}:{col}"),
                span,
                Some(format!("add the closing `{}`", close_name(close))),
            );
            Err(())
        }
    }

    /// Skip to the next item start for top-level recovery.
    fn sync_item(&mut self) {
        while !self.at_eof() {
            match self.peek().kind {
                TokenKind::Fn | TokenKind::Struct | TokenKind::At => return,
                _ => {
                    self.bump();
                }
            }
        }
    }
}

fn close_name(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::RParen => ")",
        TokenKind::RBracket => "]",
        TokenKind::RBrace => "}",
        _ => "?",
    }
}

/// Parse a token stream into a program, collecting all diagnostics.
pub fn parse(toks: Vec<Token>, src: &str) -> (Program, Vec<Diag>) {
    let mut p = Parser::new(toks, src);
    let mut items = Vec::new();
    while !p.at_eof() {
        match p.parse_item() {
            Ok(item) => items.push(item),
            Err(()) => p.sync_item(),
        }
    }
    (Program { items }, p.errors)
}

impl Parser<'_> {
    fn parse_item(&mut self) -> PResult<Item> {
        let start = self.peek().span;
        let mut annotations = Vec::new();
        while matches!(self.peek().kind, TokenKind::At) {
            annotations.push(self.parse_annotation()?);
        }
        match self.peek().kind.clone() {
            TokenKind::Fn => {
                let mut decl = self.parse_fn()?;
                decl.annotations = annotations;
                let span = start.merge(self.prev_span());
                Ok(Item {
                    kind: ItemKind::Fn(decl),
                    span,
                })
            }
            TokenKind::Struct => {
                let mut decl = self.parse_struct()?;
                decl.annotations = annotations;
                let span = start.merge(self.prev_span());
                Ok(Item {
                    kind: ItemKind::Struct(decl),
                    span,
                })
            }
            _ => {
                let span = self.peek().span;
                self.error(
                    "E0101",
                    format!(
                        "expected `fn` or `struct` item, found {}",
                        self.peek().kind.name()
                    ),
                    span,
                    None,
                );
                Err(())
            }
        }
    }

    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1).min(self.toks.len() - 1)].span
    }

    fn parse_annotation(&mut self) -> PResult<Annotation> {
        let at = self.bump().span;
        let name = match self.peek().kind.clone() {
            TokenKind::Ident(n) => {
                self.bump();
                n
            }
            _ => {
                let span = self.peek().span;
                self.error(
                    "E0101",
                    format!("expected annotation name, found {}", self.peek().kind.name()),
                    span,
                    None,
                );
                return Err(());
            }
        };
        let mut args = Vec::new();
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.bump();
            loop {
                if matches!(self.peek().kind, TokenKind::RParen) {
                    self.bump();
                    break;
                }
                if self.at_eof() {
                    self.error(
                        "E0103",
                        "unclosed `(` in annotation".to_string(),
                        self.peek().span,
                        Some("add the closing `)`".to_string()),
                    );
                    return Err(());
                }
                // Arguments are literals or names; store their text.
                let tok = self.bump();
                match &tok.kind {
                    TokenKind::Int(v) => args.push(v.to_string()),
                    TokenKind::Float(v) => args.push(v.to_string()),
                    TokenKind::Str(s) | TokenKind::Ident(s) => args.push(s.clone()),
                    TokenKind::True => args.push("true".to_string()),
                    TokenKind::False => args.push("false".to_string()),
                    TokenKind::Comma => continue,
                    _ => {
                        self.error(
                            "E0101",
                            format!("expected annotation argument, found {}", tok.kind.name()),
                            tok.span,
                            None,
                        );
                        return Err(());
                    }
                }
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                }
            }
        }
        Ok(Annotation {
            name,
            args,
            span: at.merge(self.prev_span()),
        })
    }

    fn parse_ident(&mut self, what: &str) -> PResult<(String, Span)> {
        match self.peek().kind.clone() {
            TokenKind::Ident(n) => {
                let t = self.bump();
                Ok((n, t.span))
            }
            _ => {
                let span = self.peek().span;
                self.error(
                    "E0101",
                    format!("expected {what}, found {}", self.peek().kind.name()),
                    span,
                    None,
                );
                Err(())
            }
        }
    }

    fn check_no_generics(&mut self, what: &str) -> PResult<()> {
        if matches!(self.peek().kind, TokenKind::Lt) {
            let span = self.peek().span;
            self.error(
                "E0154",
                format!("user-defined generics are not supported in v1 ({what})"),
                span,
                Some("remove the type parameters".to_string()),
            );
            // Recover: skip to the parameter list or body.
            while !self.at_eof() {
                match self.peek().kind {
                    TokenKind::LParen | TokenKind::LBrace | TokenKind::Semi => break,
                    _ => {
                        self.bump();
                    }
                }
            }
            return Err(());
        }
        Ok(())
    }

    fn parse_fn(&mut self) -> PResult<FnDecl> {
        self.bump(); // fn
        let (name, name_span) = self.parse_ident("function name")?;
        if self.check_no_generics("generic functions").is_err() {
            return Err(());
        }
        let open = self.expect(|k| matches!(k, TokenKind::LParen), "`(`")?.span;
        let mut params = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                let is_mut = if matches!(self.peek().kind, TokenKind::Mut) {
                    self.bump();
                    true
                } else {
                    false
                };
                let (pname, pname_span) = self.parse_ident("parameter name")?;
                self.expect(|k| matches!(k, TokenKind::Colon), "`:`")?;
                let ty = self.parse_type()?;
                params.push(Param {
                    name: pname,
                    name_span: pname_span,
                    ty,
                    is_mut,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                    continue;
                }
                break;
            }
        }
        self.expect_close(open, &TokenKind::RParen, "(")?;
        // Explicit return type required (spec §3).
        if !matches!(self.peek().kind, TokenKind::Arrow) {
            let span = self.peek().span;
            self.error(
                "E0101",
                "functions require an explicit return type".to_string(),
                span,
                Some("add `-> Type` before the body, e.g. `-> int`".to_string()),
            );
            return Err(());
        }
        self.bump();
        let ret = self.parse_type()?;
        let body = self.parse_block()?;
        let body = Box::new(body);
        Ok(FnDecl {
            name,
            name_span,
            params,
            ret,
            body,
            annotations: Vec::new(),
        })
    }

    fn parse_closure(&mut self) -> PResult<Expr> {
        let start = self.bump().span; // fn
        let open = self.expect(|k| matches!(k, TokenKind::LParen), "`(`")?.span;
        let mut params = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                let is_mut = if matches!(self.peek().kind, TokenKind::Mut) {
                    self.bump();
                    true
                } else {
                    false
                };
                let (pname, pname_span) = self.parse_ident("parameter name")?;
                self.expect(|k| matches!(k, TokenKind::Colon), "`:`")?;
                let ty = self.parse_type()?;
                params.push(Param {
                    name: pname,
                    name_span: pname_span,
                    ty,
                    is_mut,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                    continue;
                }
                break;
            }
        }
        self.expect_close(open, &TokenKind::RParen, "(")?;
        self.expect(|k| matches!(k, TokenKind::Arrow), "`->`")?;
        let ret = self.parse_type()?;
        let body = self.parse_block()?;
        let span = start.merge(body.span);
        let body = Box::new(body);
        Ok(Sp::new(
            ExprKind::Closure { params, ret, body },
            span,
        ))
    }

    fn parse_struct(&mut self) -> PResult<StructDecl> {
        self.bump(); // struct
        let (name, name_span) = self.parse_ident("struct name")?;
        if self.check_no_generics("generic structs").is_err() {
            return Err(());
        }
        let open = self.expect(|k| matches!(k, TokenKind::LBrace), "`{`")?.span;
        let mut fields = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBrace) {
            loop {
                let (fname, fspan) = self.parse_ident("field name")?;
                self.expect(|k| matches!(k, TokenKind::Colon), "`:`")?;
                let ty = self.parse_type()?;
                fields.push(StructField {
                    name: fname,
                    ty,
                    span: fspan,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::RBrace) {
                        break;
                    }
                    continue;
                }
                break;
            }
        }
        self.expect_close(open, &TokenKind::RBrace, "{")?;
        Ok(StructDecl {
            name,
            name_span,
            fields,
            annotations: Vec::new(),
        })
    }

    fn parse_type(&mut self) -> PResult<Type> {
        let start = self.peek().span;
        let name = match self.peek().kind.clone() {
            TokenKind::Ident(n) => {
                self.bump();
                n
            }
            TokenKind::IntTy
            | TokenKind::FloatTy
            | TokenKind::StringTy
            | TokenKind::BoolTy
            | TokenKind::UnitTy
            | TokenKind::ArrayTy
            | TokenKind::MapTy
            | TokenKind::RangeTy
            | TokenKind::ResultTy
            | TokenKind::OptionTy => {
                let t = self.bump();
                t.kind.name().trim_matches('`').to_string()
            }
            _ => {
                self.error(
                    "E0101",
                    format!("expected type, found {}", self.peek().kind.name()),
                    start,
                    None,
                );
                return Err(());
            }
        };
        let mut args = Vec::new();
        if matches!(self.peek().kind, TokenKind::Lt) {
            self.bump();
            loop {
                args.push(self.parse_type()?);
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    continue;
                }
                break;
            }
            if !matches!(self.peek().kind, TokenKind::Gt) {
                let span = self.peek().span;
                self.error(
                    "E0101",
                    format!("expected `>`, found {}", self.peek().kind.name()),
                    span,
                    None,
                );
                return Err(());
            }
            self.bump();
        }
        Ok(Type {
            name,
            args,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_block(&mut self) -> PResult<Block> {
        let open = self.expect(|k| matches!(k, TokenKind::LBrace), "`{`")?.span;
        let mut stmts = Vec::new();
        loop {
            if self.at_eof() {
                let (line, col) = open.line_col(self.src);
                self.error(
                    "E0103",
                    format!("unclosed `{{` opened at {line}:{col}"),
                    self.peek().span,
                    Some("add the closing `}`".to_string()),
                );
                return Err(());
            }
            if matches!(self.peek().kind, TokenKind::RBrace) {
                let close = self.bump();
                return Ok(Block {
                    stmts,
                    tail: None,
                    span: open.merge(close.span),
                });
            }
            match self.parse_stmt() {
                Ok(StmtOut::Stmt(stmt)) => stmts.push(stmt),
                Ok(StmtOut::Tail(tail)) => {
                    if !matches!(self.peek().kind, TokenKind::RBrace) {
                        let span = self.peek().span;
                        self.error(
                            "E0102",
                            format!(
                                "expected `;` or `}}`, found {}",
                                self.peek().kind.name()
                            ),
                            span,
                            Some("add `;` to make this a statement".to_string()),
                        );
                        self.sync_stmt();
                        continue;
                    }
                    let close = self.bump();
                    return Ok(Block {
                        stmts,
                        tail: Some(tail),
                        span: open.merge(close.span),
                    });
                }
                Err(()) => self.sync_stmt(),
            }
        }
    }

    /// Parse one statement: a full statement, or a block tail expression.
    /// A trailing `if` (with or without `else`) with no `;` is a statement
    /// unless it is immediately followed by `}`, when it is the tail.
    fn parse_stmt(&mut self) -> PResult<StmtOut> {
        let start = self.peek().span;
        match self.peek().kind.clone() {
            TokenKind::Let => {
                self.bump();
                let is_mut = if matches!(self.peek().kind, TokenKind::Mut) {
                    self.bump();
                    true
                } else {
                    false
                };
                let (name, name_span) = self.parse_ident("binding name")?;
                let ty = if matches!(self.peek().kind, TokenKind::Colon) {
                    self.bump();
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(|k| matches!(k, TokenKind::Eq), "`=`")?;
                let value = self.parse_expr(true, false)?;
                self.expect_semi("after `let` statement")?;
                Ok(StmtOut::Stmt(Sp::new(
                    StmtKind::Let {
                        name,
                        name_span,
                        ty,
                        is_mut,
                        value,
                    },
                    start.merge(self.prev_span()),
                )))
            }
            TokenKind::Return => {
                self.bump();
                let value = self.parse_expr(true, false)?;
                self.expect_semi("after `return`")?;
                Ok(StmtOut::Stmt(Sp::new(
                    StmtKind::Return(value),
                    start.merge(self.prev_span()),
                )))
            }
            TokenKind::Break => {
                self.bump();
                if self.loop_depth == 0 {
                    self.error(
                        "E0105",
                        "`break` outside of a loop".to_string(),
                        start.merge(self.prev_span()),
                        None,
                    );
                    return Err(());
                }
                self.expect_semi("after `break`")?;
                Ok(StmtOut::Stmt(Sp::new(
                    StmtKind::Break,
                    start.merge(self.prev_span()),
                )))
            }
            TokenKind::Continue => {
                self.bump();
                if self.loop_depth == 0 {
                    self.error(
                        "E0105",
                        "`continue` outside of a loop".to_string(),
                        start.merge(self.prev_span()),
                        None,
                    );
                    return Err(());
                }
                self.expect_semi("after `continue`")?;
                Ok(StmtOut::Stmt(Sp::new(
                    StmtKind::Continue,
                    start.merge(self.prev_span()),
                )))
            }
            TokenKind::While => {
                self.bump();
                let cond = self.parse_expr(false, false)?;
                self.loop_depth += 1;
                let body = self.parse_block();
                self.loop_depth -= 1;
                let body = Box::new(body?);
                let span = start.merge(body.span);
                Ok(StmtOut::Stmt(Sp::new(StmtKind::While { cond, body }, span)))
            }
            TokenKind::For => {
                self.bump();
                let (name, name_span) = self.parse_ident("loop variable")?;
                self.expect(|k| matches!(k, TokenKind::In), "`in`")?;
                let iter = self.parse_expr(false, false)?;
                self.loop_depth += 1;
                let body = self.parse_block();
                self.loop_depth -= 1;
                let body = Box::new(body?);
                let span = start.merge(body.span);
                Ok(StmtOut::Stmt(Sp::new(
                    StmtKind::For {
                        name,
                        name_span,
                        iter,
                        body,
                    },
                    span,
                )))
            }
            TokenKind::Go => {
                self.bump();
                self.error(
                    "E0151",
                    "`go` blocks execute in Phase 3".to_string(),
                    start.merge(self.prev_span()),
                    None,
                );
                if matches!(self.peek().kind, TokenKind::LBrace) {
                    self.bump();
                    self.skip_balanced();
                }
                Err(())
            }
            TokenKind::Match => {
                self.bump();
                self.error(
                    "E0156",
                    "`match` pattern matching is a Phase 2 concern".to_string(),
                    start.merge(self.prev_span()),
                    None,
                );
                if matches!(self.peek().kind, TokenKind::LBrace) {
                    self.bump();
                    self.skip_balanced();
                } else {
                    self.sync_stmt();
                }
                Err(())
            }
            TokenKind::Import | TokenKind::Mod | TokenKind::Use | TokenKind::Pub => {
                let t = self.bump();
                self.error(
                    "E0157",
                    format!("{} modules and imports arrive in Phase 2", t.kind.name()),
                    start.merge(t.span),
                    None,
                );
                self.sync_stmt();
                Err(())
            }
            TokenKind::Unsafe | TokenKind::Extern => {
                let t = self.bump();
                self.error(
                    "E0155",
                    format!("{} FFI blocks execute in Phase 5", t.kind.name()),
                    start.merge(t.span),
                    None,
                );
                if matches!(self.peek().kind, TokenKind::LBrace) {
                    self.bump();
                    self.skip_balanced();
                } else {
                    self.sync_stmt();
                }
                Err(())
            }
            TokenKind::Trait
            | TokenKind::Impl
            | TokenKind::Const
            | TokenKind::Static
            | TokenKind::As
            | TokenKind::SelfKw
            | TokenKind::Defer
            | TokenKind::Close
            | TokenKind::Select => {
                let t = self.bump();
                self.error(
                    "E0159",
                    format!("{} is not in the v1 core subset", t.kind.name()),
                    start.merge(t.span),
                    None,
                );
                self.sync_stmt();
                Err(())
            }
            TokenKind::Loop => {
                let t = self.bump();
                self.error(
                    "E0159",
                    "`loop` is not in the v1 core subset".to_string(),
                    start.merge(t.span),
                    Some("use `while true { ... }`".to_string()),
                );
                self.sync_stmt();
                Err(())
            }
            TokenKind::RBrace | TokenKind::Eof => {
                self.error(
                    "E0101",
                    format!("expected statement, found {}", self.peek().kind.name()),
                    self.peek().span,
                    None,
                );
                Err(())
            }
            _ => {
                // Expression statement or assignment. Parse an expression,
                // stopping before a top-level `=`.
                let expr = self.parse_expr(true, true)?;
                if matches!(self.peek().kind, TokenKind::Eq) {
                    self.bump();
                    let value = self.parse_expr(true, false)?;
                    self.expect_semi("after assignment")?;
                    let target = match expr.node {
                        ExprKind::Var(name) => AssignTarget::Var(name, expr.span),
                        ExprKind::Field { base, name } => {
                            let span = expr.span;
                            AssignTarget::Field(base, name, span)
                        }
                        ExprKind::Index { base, index } => {
                            let span = expr.span;
                            AssignTarget::Index(base, index, span)
                        }
                        _ => {
                            self.error(
                                "E0104",
                                "invalid assignment target".to_string(),
                                expr.span,
                                Some(
                                    "assign to a variable, field, or index".to_string(),
                                ),
                            );
                            return Err(());
                        }
                    };
                    Ok(StmtOut::Stmt(Sp::new(
                        StmtKind::Assign {
                            target,
                            value,
                        },
                        start.merge(self.prev_span()),
                    )))
                } else if matches!(self.peek().kind, TokenKind::Semi) {
                    self.bump();
                    Ok(StmtOut::Stmt(Sp::new(
                        StmtKind::Expr(expr),
                        start.merge(self.prev_span()),
                    )))
                } else if matches!(expr.node, ExprKind::If { .. })
                    && !matches!(self.peek().kind, TokenKind::RBrace)
                {
                    // Statement-position `if` needs no `;`.
                    Ok(StmtOut::Stmt(Sp::new(
                        StmtKind::Expr(expr),
                        start.merge(self.prev_span()),
                    )))
                } else {
                    // No `;`: this is a block tail expression candidate.
                    Ok(StmtOut::Tail(expr))
                }
            }
        }
    }
}

impl Parser<'_> {
    /// Parse an expression. `allow_bare_if` permits `if` without `else`
    /// (statement position only, spec §3). `stmt_level` stops before a
    /// top-level `=` so the statement parser can treat it as assignment.
    fn parse_expr(&mut self, allow_bare_if: bool, stmt_level: bool) -> PResult<Expr> {
        if self.depth >= MAX_DEPTH {
            self.error(
                "E0108",
                "nesting too deep".to_string(),
                self.peek().span,
                None,
            );
            return Err(());
        }
        self.depth += 1;
        let result = self.parse_prec(allow_bare_if, stmt_level, 0);
        self.depth -= 1;
        result
    }

    fn parse_prec(
        &mut self,
        allow_bare_if: bool,
        stmt_level: bool,
        min_prec: u8,
    ) -> PResult<Expr> {
        let mut left = self.parse_unary(allow_bare_if)?;
        loop {
            let (op, prec) = match self.peek().kind.clone() {
                TokenKind::OrOr => (BinOp::Or, 10),
                TokenKind::AndAnd => (BinOp::And, 20),
                TokenKind::EqEq => (BinOp::Eq, 30),
                TokenKind::BangEq => (BinOp::NotEq, 30),
                TokenKind::Lt => (BinOp::Lt, 40),
                TokenKind::LtEq => (BinOp::LtEq, 40),
                TokenKind::Gt => (BinOp::Gt, 40),
                TokenKind::GtEq => (BinOp::GtEq, 40),
                TokenKind::DotDot => (BinOp::Range, 45),
                TokenKind::Plus => (BinOp::Add, 50),
                TokenKind::Minus => (BinOp::Sub, 50),
                TokenKind::Star => (BinOp::Mul, 60),
                TokenKind::Slash => (BinOp::Div, 60),
                TokenKind::Percent => (BinOp::Mod, 60),
                TokenKind::LtMinus => {
                    let span = self.peek().span;
                    self.bump();
                    self.error(
                        "E0152",
                        "channel send/receive executes in Phase 3".to_string(),
                        span,
                        None,
                    );
                    // Recover: parse a right-hand side, drop the construct.
                    let _ = self.parse_unary(false);
                    return Err(());
                }
                TokenKind::Eq => {
                    if stmt_level && min_prec == 0 {
                        break;
                    }
                    let span = self.bump().span;
                    self.error(
                        "E0106",
                        "unexpected `=` in expression".to_string(),
                        span,
                        Some(
                            "did you mean `==`? assignment is a statement and cannot appear inside an expression".to_string(),
                        ),
                    );
                    // Recover as `==` so parsing can continue.
                    let right = self.parse_prec(false, false, 31)?;
                    let span = left.span.merge(right.span);
                    left = Sp::new(
                        ExprKind::Binary {
                            op: BinOp::Eq,
                            left: Box::new(left),
                            right: Box::new(right),
                        },
                        span,
                    );
                    continue;
                }
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let right = self.parse_prec(false, false, prec + 1)?;
            let span = left.span.merge(right.span);
            left = Sp::new(
                ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            );
        }
        Ok(left)
    }

    fn parse_unary(&mut self, allow_bare_if: bool) -> PResult<Expr> {
        let start = self.peek().span;
        match self.peek().kind.clone() {
            TokenKind::Minus => {
                self.bump();
                let expr = self.parse_unary(false)?;
                let span = start.merge(expr.span);
                Ok(Sp::new(
                    ExprKind::Unary {
                        op: UnOp::Neg,
                        expr: Box::new(expr),
                    },
                    span,
                ))
            }
            TokenKind::Bang => {
                self.bump();
                let expr = self.parse_unary(false)?;
                let span = start.merge(expr.span);
                Ok(Sp::new(
                    ExprKind::Unary {
                        op: UnOp::Not,
                        expr: Box::new(expr),
                    },
                    span,
                ))
            }
            _ => self.parse_postfix(allow_bare_if),
        }
    }

    fn parse_postfix(&mut self, allow_bare_if: bool) -> PResult<Expr> {
        let mut expr = self.parse_primary(allow_bare_if)?;
        loop {
            match self.peek().kind.clone() {
                TokenKind::LParen => {
                    let open = self.bump().span;
                    let mut args = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr(false, false)?);
                            if matches!(self.peek().kind, TokenKind::Comma) {
                                self.bump();
                                if matches!(self.peek().kind, TokenKind::RParen) {
                                    break;
                                }
                                continue;
                            }
                            break;
                        }
                    }
                    self.expect_close(open, &TokenKind::RParen, "(")?;
                    let span = expr.span.merge(self.prev_span());
                    expr = Sp::new(
                        ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                        },
                        span,
                    );
                }
                TokenKind::Dot => {
                    self.bump();
                    let (name, name_span) = self.parse_ident("field name")?;
                    if matches!(self.peek().kind, TokenKind::LParen) {
                        // Method-call syntax is not v1 (spec §3.3, E0158).
                        // Recover: consume the balanced argument list.
                        self.skip_call_args();
                        self.error(
                            "E0158",
                            "method-call syntax is not in v1".to_string(),
                            expr.span.merge(name_span),
                            Some(
                                "Toto v1 has functions only; call a function with the value as an argument".to_string(),
                            ),
                        );
                        return Err(());
                    }
                    let span = expr.span.merge(name_span);
                    expr = Sp::new(
                        ExprKind::Field {
                            base: Box::new(expr),
                            name,
                        },
                        span,
                    );
                }
                TokenKind::LBracket => {
                    let open = self.bump().span;
                    let index = self.parse_expr(false, false)?;
                    self.expect_close(open, &TokenKind::RBracket, "[")?;
                    let span = expr.span.merge(self.prev_span());
                    expr = Sp::new(
                        ExprKind::Index {
                            base: Box::new(expr),
                            index: Box::new(index),
                        },
                        span,
                    );
                }
                TokenKind::Question => {
                    let span = self.bump().span;
                    self.error(
                        "E0153",
                        "`?` error propagation is specified now and executes in Phase 2"
                            .to_string(),
                        span,
                        None,
                    );
                    return Err(());
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Consume `(...)` with nesting (called after `(` was already bumped).
    fn skip_call_args(&mut self) {
        let mut depth = 1u32;
        while !self.at_eof() && depth > 0 {
            match self.bump().kind {
                TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
                TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                    depth -= 1;
                }
                _ => {}
            }
            if depth > 64 {
                break;
            }
        }
    }

    fn parse_primary(&mut self, allow_bare_if: bool) -> PResult<Expr> {
        let start = self.peek().span;
        match self.peek().kind.clone() {
            TokenKind::Int(v) => {
                self.bump();
                Ok(Sp::new(ExprKind::Int(v), start))
            }
            TokenKind::Float(v) => {
                self.bump();
                Ok(Sp::new(ExprKind::Float(v), start))
            }
            TokenKind::Str(s) => {
                self.bump();
                Ok(Sp::new(ExprKind::Str(s), start))
            }
            TokenKind::True => {
                self.bump();
                Ok(Sp::new(ExprKind::Bool(true), start))
            }
            TokenKind::False => {
                self.bump();
                Ok(Sp::new(ExprKind::Bool(false), start))
            }
            TokenKind::LParen => {
                self.bump();
                if matches!(self.peek().kind, TokenKind::RParen) {
                    let close = self.bump();
                    return Ok(Sp::new(ExprKind::Unit, start.merge(close.span)));
                }
                let inner = self.parse_expr(true, false)?;
                self.expect_close(start, &TokenKind::RParen, "(")?;
                let span = start.merge(self.prev_span());
                Ok(Sp::new(inner.node, span))
            }
            TokenKind::LBracket => {
                let open = self.bump().span;
                let mut elems = Vec::new();
                if !matches!(self.peek().kind, TokenKind::RBracket) {
                    loop {
                        elems.push(self.parse_expr(false, false)?);
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.bump();
                            if matches!(self.peek().kind, TokenKind::RBracket) {
                                break;
                            }
                            continue;
                        }
                        break;
                    }
                }
                self.expect_close(open, &TokenKind::RBracket, "[")?;
                Ok(Sp::new(
                    ExprKind::Array(elems),
                    open.merge(self.prev_span()),
                ))
            }
            TokenKind::If => self.parse_if(allow_bare_if),
            TokenKind::Fn => self.parse_closure(),
            TokenKind::Ident(name) => {
                self.bump();
                if matches!(self.peek().kind, TokenKind::LBrace) {
                    return self.parse_struct_lit(name, start);
                }
                Ok(Sp::new(ExprKind::Var(name), start))
            }
            TokenKind::Go => {
                self.bump();
                self.error(
                    "E0151",
                    "`go` blocks execute in Phase 3".to_string(),
                    start.merge(self.prev_span()),
                    None,
                );
                Err(())
            }
            TokenKind::LtMinus => {
                let t = self.bump();
                self.error(
                    "E0152",
                    "channel send/receive executes in Phase 3".to_string(),
                    t.span,
                    None,
                );
                Err(())
            }
            TokenKind::Match => {
                self.bump();
                self.error(
                    "E0156",
                    "`match` pattern matching is a Phase 2 concern".to_string(),
                    start.merge(self.prev_span()),
                    None,
                );
                Err(())
            }
            TokenKind::Unsafe | TokenKind::Extern => {
                let t = self.bump();
                self.error(
                    "E0155",
                    format!("{} FFI blocks execute in Phase 5", t.kind.name()),
                    start.merge(t.span),
                    None,
                );
                Err(())
            }
            TokenKind::Import | TokenKind::Mod | TokenKind::Use | TokenKind::Pub => {
                let t = self.bump();
                self.error(
                    "E0157",
                    format!("{} modules and imports arrive in Phase 2", t.kind.name()),
                    start.merge(t.span),
                    None,
                );
                Err(())
            }
            TokenKind::Trait
            | TokenKind::Impl
            | TokenKind::Const
            | TokenKind::Static
            | TokenKind::As
            | TokenKind::SelfKw
            | TokenKind::Defer
            | TokenKind::Close
            | TokenKind::Select
            | TokenKind::Loop => {
                let t = self.bump();
                self.error(
                    "E0159",
                    format!("{} is not in the v1 core subset", t.kind.name()),
                    start.merge(t.span),
                    None,
                );
                Err(())
            }
            _ => {
                let span = self.peek().span;
                self.error(
                    "E0101",
                    format!("expected expression, found {}", self.peek().kind.name()),
                    span,
                    None,
                );
                Err(())
            }
        }
    }

    fn parse_struct_lit(&mut self, name: String, start: Span) -> PResult<Expr> {
        let open = self.bump().span; // {
        let mut fields = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBrace) {
            loop {
                let (fname, fspan) = self.parse_ident("field name")?;
                self.expect(|k| matches!(k, TokenKind::Colon), "`:`")?;
                let value = self.parse_expr(false, false)?;
                fields.push((fname, value, fspan));
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::RBrace) {
                        break;
                    }
                    continue;
                }
                break;
            }
        }
        self.expect_close(open, &TokenKind::RBrace, "{")?;
        let span = start.merge(self.prev_span());
        Ok(Sp::new(
            ExprKind::StructLit { name, fields },
            span,
        ))
    }

    fn parse_if(&mut self, allow_bare_if: bool) -> PResult<Expr> {
        let start = self.bump().span; // if
        let cond = self.parse_expr(false, false)?;
        let then_b = Box::new(self.parse_block()?);
        let mut else_b: Option<Box<Block>> = None;
        if matches!(self.peek().kind, TokenKind::Else) {
            self.bump();
            if matches!(self.peek().kind, TokenKind::If) {
                let nested = self.parse_if(true)?;
                // An `else if` chain is an else containing one if-expr;
                // model it as a block with that if as its tail.
                let span = nested.span;
                else_b = Some(Box::new(Block {
                    stmts: Vec::new(),
                    tail: Some(nested),
                    span,
                }));
            } else {
                else_b = Some(Box::new(self.parse_block()?));
            }
        }
        let end = else_b.as_ref().map(|b| b.span).unwrap_or(then_b.span);
        let span = start.merge(end);
        if !has_final_else(&else_b) && !allow_bare_if {
            self.error(
                "E0107",
                "`if` used as an expression requires an `else` branch".to_string(),
                span,
                Some("add an `else` branch, or use `if` as a statement".to_string()),
            );
            return Err(());
        }
        Ok(Sp::new(
            ExprKind::If {
                cond: Box::new(cond),
                then_b,
                else_b,
            },
            span,
        ))
    }
}

/// Does this else-branch (or chain) end in a real `else`? An `else if`
/// chain defers to its innermost if; any other present `else` counts.
fn has_final_else(else_b: &Option<Box<Block>>) -> bool {
    match else_b {
        None => false,
        Some(block) => match &block.tail {
            Some(tail) => match &tail.node {
                ExprKind::If {
                    else_b: inner, ..
                } => has_final_else(inner),
                _ => true,
            },
            None => true,
        },
    }
}
