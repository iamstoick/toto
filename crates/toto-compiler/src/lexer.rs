//! Lexer for the full v1 token set (spec §2). Errors are collected and
//! lexing continues; the lexer never panics on user input.

use crate::span::Span;
use crate::token::{Token, TokenKind, keyword};

/// A lexer error: code, message, span, optional suggestion.
#[derive(Clone, Debug)]
pub struct LexError {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
}

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    errors: Vec<LexError>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn span(&self, start: usize) -> Span {
        Span::new(start as u32, self.pos as u32)
    }

    fn emit(&mut self, start: usize, kind: TokenKind) {
        let span = self.span(start);
        self.tokens.push(Token { kind, span });
    }

    fn fail(&mut self, start: usize, code: &'static str, message: String, help: Option<String>) {
        let span = self.span(start);
        self.errors.push(LexError {
            code,
            message,
            span,
            help,
        });
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn bump(&mut self) {
        // Advance one Unicode scalar (source is UTF-8; positions stay on
        // character boundaries because we only stop at ASCII bytes).
        let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
        self.pos += ch.len_utf8();
    }

    pub fn lex(mut self) -> (Vec<Token>, Vec<LexError>) {
        while self.pos < self.bytes.len() {
            let start = self.pos;
            let b = self.bytes[self.pos];
            match b {
                b' ' | b'\t' | b'\r' | b'\n' => self.bump(),
                b'/' if self.peek2() == Some(b'/') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                b'(' => {
                    self.bump();
                    self.emit(start, TokenKind::LParen);
                }
                b')' => {
                    self.bump();
                    self.emit(start, TokenKind::RParen);
                }
                b'{' => {
                    self.bump();
                    self.emit(start, TokenKind::LBrace);
                }
                b'}' => {
                    self.bump();
                    self.emit(start, TokenKind::RBrace);
                }
                b'[' => {
                    self.bump();
                    self.emit(start, TokenKind::LBracket);
                }
                b']' => {
                    self.bump();
                    self.emit(start, TokenKind::RBracket);
                }
                b',' => {
                    self.bump();
                    self.emit(start, TokenKind::Comma);
                }
                b';' => {
                    self.bump();
                    self.emit(start, TokenKind::Semi);
                }
                b':' => {
                    self.bump();
                    self.emit(start, TokenKind::Colon);
                }
                b'+' => {
                    self.bump();
                    self.emit(start, TokenKind::Plus);
                }
                b'*' => {
                    self.bump();
                    self.emit(start, TokenKind::Star);
                }
                b'/' => {
                    self.bump();
                    self.emit(start, TokenKind::Slash);
                }
                b'%' => {
                    self.bump();
                    self.emit(start, TokenKind::Percent);
                }
                b'@' => {
                    self.bump();
                    self.emit(start, TokenKind::At);
                }
                b'?' => {
                    self.bump();
                    self.emit(start, TokenKind::Question);
                }
                b'.' => {
                    self.bump();
                    if self.peek() == Some(b'.') {
                        self.bump();
                        self.emit(start, TokenKind::DotDot);
                    } else {
                        self.emit(start, TokenKind::Dot);
                    }
                }
                b'=' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        self.emit(start, TokenKind::EqEq);
                    } else if self.peek() == Some(b'>') {
                        self.bump();
                        self.emit(start, TokenKind::FatArrow);
                    } else {
                        self.emit(start, TokenKind::Eq);
                    }
                }
                b'!' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        self.emit(start, TokenKind::BangEq);
                    } else {
                        self.emit(start, TokenKind::Bang);
                    }
                }
                b'<' => {
                    self.bump();
                    // Spec §2.3: `<` immediately followed by `-` lexes as
                    // one `<-` token; otherwise `<` stands alone.
                    if self.peek() == Some(b'=') {
                        self.bump();
                        self.emit(start, TokenKind::LtEq);
                    } else if self.peek() == Some(b'-') {
                        self.bump();
                        self.emit(start, TokenKind::LtMinus);
                    } else {
                        self.emit(start, TokenKind::Lt);
                    }
                }
                b'>' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        self.emit(start, TokenKind::GtEq);
                    } else {
                        self.emit(start, TokenKind::Gt);
                    }
                }
                b'-' => {
                    self.bump();
                    if self.peek() == Some(b'>') {
                        self.bump();
                        self.emit(start, TokenKind::Arrow);
                    } else {
                        self.emit(start, TokenKind::Minus);
                    }
                }
                b'&' => {
                    self.bump();
                    if self.peek() == Some(b'&') {
                        self.bump();
                        self.emit(start, TokenKind::AndAnd);
                    } else {
                        self.fail(
                            start,
                            "E0001",
                            "unexpected character `&`".to_string(),
                            Some("did you mean `&&`?".to_string()),
                        );
                    }
                }
                b'|' => {
                    self.bump();
                    if self.peek() == Some(b'|') {
                        self.bump();
                        self.emit(start, TokenKind::OrOr);
                    } else {
                        self.fail(
                            start,
                            "E0001",
                            "unexpected character `|`".to_string(),
                            Some("did you mean `||`?".to_string()),
                        );
                    }
                }
                b'"' => self.lex_string(start),
                b'\'' => {
                    self.bump();
                    self.fail(
                        start,
                        "E0001",
                        "unexpected character `'`".to_string(),
                        Some("strings use double quotes: `\"x\"`".to_string()),
                    );
                }
                b'#' => {
                    self.bump();
                    self.fail(
                        start,
                        "E0001",
                        "unexpected character `#`".to_string(),
                        Some("comments start with `//`".to_string()),
                    );
                }
                b'0'..=b'9' => self.lex_number(start),
                b'A'..=b'Z' | b'a'..=b'z' | b'_' => self.lex_word(start),
                _ => {
                    let ch = self.src[start..].chars().next().unwrap_or('\u{fffd}');
                    self.bump();
                    self.fail(start, "E0001", format!("unexpected character `{ch}`"), None);
                }
            }
        }
        let end = self.src.len() as u32;
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(end, end),
        });
        (self.tokens, self.errors)
    }

    fn lex_word(&mut self, start: usize) {
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        let word = &self.src[start..self.pos];
        let kind = keyword(word).unwrap_or_else(|| TokenKind::Ident(word.to_string()));
        self.emit(start, kind);
    }

    fn lex_number(&mut self, start: usize) {
        let hex =
            self.bytes[start] == b'0' && (self.peek2() == Some(b'x') || self.peek2() == Some(b'X'));
        if hex {
            self.bump();
            self.bump();
            let digits = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() || c == b'_' {
                    self.bump();
                } else {
                    break;
                }
            }
            let raw: String = self.src[digits..self.pos]
                .chars()
                .filter(|&c| c != '_')
                .collect();
            match i64::from_str_radix(&raw, 16) {
                Ok(v) => self.emit(start, TokenKind::Int(v)),
                Err(_) => {
                    self.fail(
                        start,
                        "E0004",
                        format!(
                            "integer literal `{}` out of range",
                            &self.src[start..self.pos]
                        ),
                        None,
                    );
                    self.emit(start, TokenKind::Int(0));
                }
            }
            return;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        // Fraction only when `.` is followed by a digit (spec §2.4: `1.` is
        // not a float, so `0..10` lexes cleanly).
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.bytes.get(self.pos + 1), Some(b'0'..=b'9')) {
            is_float = true;
            self.bump();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == b'_' {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let after_e = self.pos + 1;
            let mut p = after_e;
            if matches!(self.bytes.get(p), Some(b'+' | b'-')) {
                p += 1;
            }
            if matches!(self.bytes.get(p), Some(b'0'..=b'9')) {
                is_float = true;
                self.pos = p;
                self.bump();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == b'_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
            }
        }
        let raw: String = self.src[start..self.pos]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        if is_float {
            match raw.parse::<f64>() {
                Ok(v) => self.emit(start, TokenKind::Float(v)),
                Err(_) => {
                    self.fail(
                        start,
                        "E0004",
                        format!("invalid float literal `{raw}`"),
                        None,
                    );
                    self.emit(start, TokenKind::Float(0.0));
                }
            }
        } else {
            match raw.parse::<i64>() {
                Ok(v) => self.emit(start, TokenKind::Int(v)),
                Err(_) => {
                    self.fail(
                        start,
                        "E0004",
                        format!("integer literal `{raw}` out of range"),
                        None,
                    );
                    self.emit(start, TokenKind::Int(0));
                }
            }
        }
    }

    fn lex_string(&mut self, start: usize) {
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    self.fail(
                        start,
                        "E0002",
                        "unterminated string literal".to_string(),
                        None,
                    );
                    self.emit(start, TokenKind::Str(out));
                    return;
                }
                Some(b'"') => {
                    self.bump();
                    self.emit(start, TokenKind::Str(out));
                    return;
                }
                Some(b'\n') => {
                    self.fail(
                        start,
                        "E0002",
                        "unterminated string literal".to_string(),
                        Some("strings are single-line; close the quote first".to_string()),
                    );
                    self.emit(start, TokenKind::Str(out));
                    return;
                }
                Some(b'\\') => {
                    let esc_start = self.pos;
                    self.bump();
                    match self.peek() {
                        Some(b'n') => {
                            out.push('\n');
                            self.bump();
                        }
                        Some(b't') => {
                            out.push('\t');
                            self.bump();
                        }
                        Some(b'r') => {
                            out.push('\r');
                            self.bump();
                        }
                        Some(b'\\') => {
                            out.push('\\');
                            self.bump();
                        }
                        Some(b'"') => {
                            out.push('"');
                            self.bump();
                        }
                        _ => {
                            self.bump();
                            let bad = &self.src[esc_start..self.pos];
                            self.fail(
                                esc_start,
                                "E0003",
                                format!("unknown string escape `{bad}`"),
                                Some("valid escapes: \\n \\t \\r \\\\ \\\"".to_string()),
                            );
                        }
                    }
                }
                Some(_) => {
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    self.bump();
                }
            }
        }
    }
}

/// Lex a source file: tokens plus collected errors.
pub fn lex(src: &str) -> (Vec<Token>, Vec<LexError>) {
    Lexer::new(src).lex()
}
