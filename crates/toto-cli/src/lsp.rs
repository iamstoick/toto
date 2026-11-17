//! Basic LSP server (Phase 5, Stage 4): hover, go-to-definition, and
//! diagnostics over stdio JSON-RPC.
//!
//! Scope: single open document with project-aware imports (the open
//! buffer overrides disk; `gzv.std` resolves embedded; anything else
//! reads from disk relative to the open file's directory). Hover
//! shows signatures and types; go-to-definition jumps within the
//! project; diagnostics publish per file on open/change.
//!
//! Deliberately dependency-free (no serde): a minimal JSON value
//! model covers exactly the messages below. Positions count Unicode
//! scalar values, not UTF-16 code units (noted in `docs/lsp.md`).
//! Verification is headless/protocol-only (no real editor run); the
//! CLI test suite drives the binary over piped stdio.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use toto_compiler::hir::Ty;

// ---------------------------------------------------------------------------
// Minimal JSON.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum J {
    Null,
    Bool(bool),
    Num(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            J::Num(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }
}

fn write_json(out: &mut String, v: &J) {
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(&n.to_string()),
        J::Str(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        out.push_str(&format!("\\u{:04x}", c as u32));
                    }
                    _ => out.push(c),
                }
            }
            out.push('"');
        }
        J::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        J::Obj(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, &J::Str(k.clone()));
                out.push(':');
                write_json(out, val);
            }
            out.push('}');
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

fn parse_json(text: &str) -> Option<J> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.pos == p.bytes.len() {
        Some(v)
    } else {
        None
    }
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn lit(&mut self, s: &str) -> bool {
        if self.bytes[self.pos..].starts_with(s.as_bytes()) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<J> {
        self.ws();
        let b = *self.bytes.get(self.pos)?;
        match b {
            b'n' => self.lit("null").then_some(J::Null),
            b't' => self.lit("true").then_some(J::Bool(true)),
            b'f' => self.lit("false").then_some(J::Bool(false)),
            b'"' => self.string().map(J::Str),
            b'[' => self.array().map(J::Arr),
            b'{' => self.object().map(J::Obj),
            b'-' | b'0'..=b'9' => self.number().map(J::Num),
            _ => None,
        }
    }

    fn string(&mut self) -> Option<String> {
        // Assumes the opening quote.
        self.pos += 1;
        let mut out = String::new();
        loop {
            let b = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.bytes.get(self.pos)?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?)
                                .ok()?;
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            self.pos += 4;
                            out.push(char::from_u32(cp)?);
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Raw UTF-8 bytes (multi-byte chars included):
                    // decode one scalar from the current position.
                    let rest = std::str::from_utf8(self.bytes.get(self.pos - 1..)?).ok()?;
                    let ch = rest.chars().next()?;
                    self.pos = self.pos - 1 + ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    fn array(&mut self) -> Option<Vec<J>> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.bytes.get(self.pos) == Some(&b']') {
                self.pos += 1;
                return Some(items);
            }
            items.push(self.value()?);
            self.ws();
            match self.bytes.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Some(items);
                }
                _ => return None,
            }
        }
    }

    fn object(&mut self) -> Option<Vec<(String, J)>> {
        self.pos += 1;
        let mut pairs = Vec::new();
        loop {
            self.ws();
            if self.bytes.get(self.pos) == Some(&b'}') {
                self.pos += 1;
                return Some(pairs);
            }
            self.ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return None;
            }
            let key = self.string()?;
            self.ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return None;
            }
            self.pos += 1;
            let val = self.value()?;
            pairs.push((key, val));
            self.ws();
            match self.bytes.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Some(pairs);
                }
                _ => return None,
            }
        }
    }

    fn number(&mut self) -> Option<i64> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        std::str::from_utf8(self.bytes.get(start..self.pos)?)
            .ok()?
            .parse()
            .ok()
    }
}

// ---------------------------------------------------------------------------
// Positions (Unicode-scalar columns; see module docs).
// ---------------------------------------------------------------------------

fn offset_to_position(text: &str, off: usize) -> (u64, u64) {
    let off = off.min(text.len());
    let mut line = 0u64;
    let mut col = 0u64;
    for (i, ch) in text.char_indices() {
        if i >= off {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn position_to_offset(text: &str, line: u64, character: u64) -> usize {
    let mut cur_line = 0u64;
    let mut col = 0u64;
    for (i, ch) in text.char_indices() {
        if cur_line == line && col == character {
            return i;
        }
        if ch == '\n' {
            cur_line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    text.len()
}

fn range_json(text: &str, start: u32, end: u32) -> J {
    let (sl, sc) = offset_to_position(text, start as usize);
    let (el, ec) = offset_to_position(text, end as usize);
    J::Obj(vec![
        ("start".to_string(), pos_json(sl, sc)),
        ("end".to_string(), pos_json(el, ec)),
    ])
}

fn pos_json(line: u64, character: u64) -> J {
    J::Obj(vec![
        ("line".to_string(), J::Num(line as i64)),
        ("character".to_string(), J::Num(character as i64)),
    ])
}

// ---------------------------------------------------------------------------
// Framing + server loop.
// ---------------------------------------------------------------------------

fn read_message(input: &mut impl BufRead) -> Option<J> {
    // A malformed frame is skipped, not fatal: only a true stdin
    // EOF ends the session. (Without this, one bad byte silently
    // kills the server and the client sees an EOF.)
    loop {
        let mut len: Option<usize> = None;
        loop {
            let mut line = String::new();
            if input.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                len = rest.trim().parse().ok();
            }
        }
        let len = match len {
            Some(len) => len,
            None => continue,
        };
        let mut buf = vec![0u8; len];
        input.read_exact(&mut buf).ok()?;
        if let Some(v) = parse_json(std::str::from_utf8(&buf).ok()?) {
            return Some(v);
        }
    }
}

fn send_message(out: &mut impl Write, v: &J) {
    let mut body = String::new();
    write_json(&mut body, v);
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("lsp stdout write");
    out.flush().expect("lsp stdout flush");
}

fn error_response(id: &J, code: i64, message: &str) -> J {
    J::Obj(vec![
        ("jsonrpc".to_string(), J::Str("2.0".to_string())),
        ("id".to_string(), id.clone()),
        (
            "error".to_string(),
            J::Obj(vec![
                ("code".to_string(), J::Num(code)),
                ("message".to_string(), J::Str(message.to_string())),
            ]),
        ),
    ])
}

fn result_response(id: &J, result: J) -> J {
    J::Obj(vec![
        ("jsonrpc".to_string(), J::Str("2.0".to_string())),
        ("id".to_string(), id.clone()),
        ("result".to_string(), result),
    ])
}

fn notification(method: &str, params: J) -> J {
    J::Obj(vec![
        ("jsonrpc".to_string(), J::Str("2.0".to_string())),
        ("method".to_string(), J::Str(method.to_string())),
        ("params".to_string(), params),
    ])
}

/// Serve one LSP session on stdin/stdout; returns the process exit
/// code (`exit` notification: 0 after `shutdown`, else 1).
pub fn serve() -> std::process::ExitCode {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut docs: HashMap<String, OpenDoc> = HashMap::new();
    let mut shutdown = false;
    loop {
        let Some(msg) = read_message(&mut input) else {
            return std::process::ExitCode::from(0);
        };
        let method = msg
            .get("method")
            .and_then(J::as_str)
            .unwrap_or("")
            .to_string();
        let id = msg.get("id").cloned().unwrap_or(J::Null);
        let params = msg.get("params").cloned().unwrap_or(J::Null);
        let has_id = !matches!(id, J::Null);
        match method.as_str() {
            "initialize" => {
                let caps = J::Obj(vec![
                    ("hoverProvider".to_string(), J::Bool(true)),
                    ("definitionProvider".to_string(), J::Bool(true)),
                    (
                        "textDocumentSync".to_string(),
                        J::Obj(vec![
                            ("openClose".to_string(), J::Bool(true)),
                            ("change".to_string(), J::Num(1)),
                        ]),
                    ),
                ]);
                send_message(
                    &mut out,
                    &result_response(
                        &id,
                        J::Obj(vec![
                            ("capabilities".to_string(), caps),
                            (
                                "serverInfo".to_string(),
                                J::Obj(vec![
                                    ("name".to_string(), J::Str("toto-lsp".to_string())),
                                    (
                                        "version".to_string(),
                                        J::Str(env!("CARGO_PKG_VERSION").to_string()),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                );
            }
            "initialized" => {}
            "shutdown" => {
                shutdown = true;
                if has_id {
                    send_message(&mut out, &result_response(&id, J::Null));
                }
            }
            "exit" => {
                return std::process::ExitCode::from(u8::from(!shutdown));
            }
            "textDocument/didOpen" => {
                if let Some(doc) = params.get("textDocument") {
                    let uri = doc.get("uri").and_then(J::as_str).unwrap_or("").to_string();
                    let text = doc
                        .get("text")
                        .and_then(J::as_str)
                        .unwrap_or("")
                        .to_string();
                    let mut state = OpenDoc::new(uri, text);
                    for note in state.reload() {
                        send_message(&mut out, &note);
                    }
                    docs.insert(state.uri.clone(), state);
                }
            }
            "textDocument/didChange" => {
                if let Some(doc) = params.get("textDocument") {
                    let uri = doc.get("uri").and_then(J::as_str).unwrap_or("");
                    if let Some(state) = docs.get_mut(uri) {
                        if let Some(changes) = params.get("contentChanges") {
                            // Full-document sync: the last change
                            // carries the whole text.
                            if let J::Arr(items) = changes
                                && let Some(last) = items.last()
                                && let Some(text) = last.get("text").and_then(J::as_str)
                            {
                                state.text = text.to_string();
                            }
                        }
                        let notes = state.reload();
                        for note in notes {
                            send_message(&mut out, &note);
                        }
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(doc) = params.get("textDocument") {
                    let uri = doc.get("uri").and_then(J::as_str).unwrap_or("").to_string();
                    docs.remove(&uri);
                    // Clear stale squiggles.
                    send_message(
                        &mut out,
                        &notification(
                            "textDocument/publishDiagnostics",
                            J::Obj(vec![
                                ("uri".to_string(), J::Str(uri)),
                                ("diagnostics".to_string(), J::Arr(vec![])),
                            ]),
                        ),
                    );
                }
            }
            "textDocument/hover" => {
                let result = hover_at(&docs, &params);
                if has_id {
                    send_message(&mut out, &result_response(&id, result));
                }
            }
            "textDocument/definition" => {
                let result = definition_at(&docs, &params);
                if has_id {
                    send_message(&mut out, &result_response(&id, result));
                }
            }
            _ => {
                if has_id {
                    send_message(&mut out, &error_response(&id, -32601, "method not found"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Open documents + diagnostics.
// ---------------------------------------------------------------------------

struct OpenDoc {
    uri: String,
    path: std::path::PathBuf,
    text: String,
    project: Option<toto_compiler::project::Project>,
    checked: Option<toto_compiler::hir::CheckedProgram>,
    /// Per-file sources for cross-file jumps: (display path, text).
    file_srcs: Vec<(String, String)>,
}

fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(std::path::PathBuf::from(rest))
}

fn path_to_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

impl OpenDoc {
    fn new(uri: String, text: String) -> Self {
        let path = uri_to_path(&uri).unwrap_or_else(|| std::path::PathBuf::from(&uri));
        Self {
            uri,
            path,
            text,
            project: None,
            checked: None,
            file_srcs: Vec::new(),
        }
    }

    /// Re-resolve and re-check; returns one
    /// `publishDiagnostics` notification per file that has any.
    fn reload(&mut self) -> Vec<J> {
        self.project = None;
        self.checked = None;
        self.file_srcs.clear();
        let text = self.text.clone();
        let open_path = self.path.to_string_lossy().into_owned();
        let parent = self
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // The open buffer overrides disk; `gzv.std` resolves
        // embedded; anything else reads from disk (relative to the
        // open file's directory, like the CLI).
        let loaded = toto_compiler::project::load_and_check(
            &open_path,
            &text,
            &|p| {
                if same_file(p, &open_path) {
                    return Some(text.clone());
                }
                if let Some(src) = toto_compiler::stdlib::get(p) {
                    return Some(src.to_string());
                }
                std::fs::read_to_string(p)
                    .ok()
                    .or_else(|| std::fs::read_to_string(parent.join(p)).ok())
            },
            &[],
        );
        let mut notes = Vec::new();
        match loaded {
            Ok((project, checked)) => {
                for f in &project.files {
                    self.file_srcs.push((f.path.clone(), f.src.clone()));
                }
                self.checked = Some(checked);
                self.project = Some(project);
                // Clean slate for every known file.
                for (path, _) in &self.file_srcs.clone() {
                    notes.push(self.diagnostics_note(path, &[]));
                }
            }
            Err(failed) => {
                for f in &failed.files {
                    self.file_srcs.push((f.path.clone(), f.src.clone()));
                }
                let mut by_file: HashMap<usize, Vec<toto_compiler::diag::Diag>> = HashMap::new();
                for fd in &failed.errors {
                    by_file.entry(fd.file).or_default().extend(fd.diags.clone());
                }
                for (idx, (path, _)) in self.file_srcs.clone().into_iter().enumerate() {
                    let empty = Vec::new();
                    let diags = by_file.get(&idx).unwrap_or(&empty);
                    notes.push(self.diagnostics_note(&path, diags));
                }
            }
        }
        notes
    }

    fn diagnostics_note(&self, file_path: &str, diags: &[toto_compiler::diag::Diag]) -> J {
        let src = self
            .file_srcs
            .iter()
            .find(|(p, _)| p == file_path)
            .map(|(_, s)| s.clone())
            .unwrap_or_default();
        let items: Vec<J> = diags
            .iter()
            .map(|d| {
                J::Obj(vec![
                    (
                        "range".to_string(),
                        range_json(&src, d.span.start, d.span.end),
                    ),
                    (
                        "severity".to_string(),
                        J::Num(if d.code.starts_with('E') { 1 } else { 2 }),
                    ),
                    ("code".to_string(), J::Str(d.code.to_string())),
                    ("message".to_string(), J::Str(d.message.clone())),
                ])
            })
            .collect();
        notification(
            "textDocument/publishDiagnostics",
            J::Obj(vec![
                ("uri".to_string(), J::Str(file_uri(&self.path, file_path))),
                ("diagnostics".to_string(), J::Arr(items)),
            ]),
        )
    }
}

/// The loader hands the reader resolved paths that may or may not
/// match the open path textually (relative vs absolute, `./`
/// prefixes); compare by suffix after normalizing separators.
fn same_file(candidate: &str, open: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_start_matches("./").to_string();
    let (a, b) = (norm(candidate), norm(open));
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// Display paths are relative when under the working directory;
/// re-anchor them at the open file for `file://` URIs.
fn file_uri(open_path: &std::path::Path, display: &str) -> String {
    let p = std::path::Path::new(display);
    if p.is_absolute() {
        return path_to_uri(p);
    }
    if std::fs::metadata(display).is_ok()
        && let Ok(abs) = std::path::absolute(display)
    {
        return path_to_uri(&abs);
    }
    match open_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => path_to_uri(&parent.join(display)),
        _ => path_to_uri(p),
    }
}

// ---------------------------------------------------------------------------
// Name resolution: hover + go-to-definition.
// ---------------------------------------------------------------------------

use toto_compiler::ast::{
    AssignTarget, Block, Expr, ExprKind, ItemKind, Param, Program, Stmt, StmtKind,
};
use toto_compiler::hir::{HBlock, HExpr, HExprKind};
use toto_compiler::span::Span;

/// A resolved name: what it is and where it is defined.
enum Target {
    /// Local binding or parameter: definition span in the open file.
    Local(Span),
    /// Top-level function: file display path + name span.
    Func(String, Span),
    /// Struct declaration: file display path + name span.
    Struct(String, Span),
    /// Struct field: file display path + field span.
    Field(String, Span),
    /// Foreign function: signature text (extern blocks have no
    /// per-name spans worth jumping to beyond the block).
    Extern(String, Span),
    /// Declared-state binding (spec amendment, Phase 6): file
    /// display path + binding name span.
    State(String, Span),
    /// Frozen builtin: static documentation.
    Builtin,
}

/// Hover text plus an optional definition target.
struct Resolved {
    hover: String,
    target: Option<Target>,
}

/// Entry: resolve the name at `(line, character)` in the open
/// document. Positions outside any known name return `None`.
/// Resolution context: the open document plus the enclosing
/// top-level function (for HIR type lookups; names are unique
/// program-wide, so the HIR function matches by name).
struct Cx<'a> {
    doc: &'a OpenDoc,
    enclosing: Option<String>,
}

fn resolve_at(doc: &OpenDoc, line: u64, character: u64) -> Option<Resolved> {
    let off = position_to_offset(&doc.text, line, character);
    let program = parse_buffer(&doc.text)?;
    let item = program
        .items
        .iter()
        .find(|i| span_contains(&item_span(i), off))?;
    let mut cx = Cx {
        doc,
        enclosing: None,
    };
    let mut scopes: Vec<HashMap<String, Span>> = Vec::new();
    match &item.kind {
        ItemKind::Fn(decl) => {
            if span_contains(&fn_name_span(decl), off) {
                let sig = fn_sig_text(&decl.name, &decl.params, &decl.ret);
                return Some(Resolved {
                    hover: sig,
                    target: Some(Target::Func(doc_display(doc), fn_name_span(decl))),
                });
            }
            cx.enclosing = Some(decl.name.clone());
            push_params(&mut scopes, &decl.params);
            if let Some(r) = resolve_block(&program, &decl.body, off, &mut scopes, &cx) {
                return Some(r);
            }
        }
        ItemKind::Struct(decl) => {
            if span_contains(&decl.name_span, off) {
                return Some(Resolved {
                    hover: struct_sig_text(cx.doc, &decl.name),
                    target: Some(Target::Struct(doc_display(cx.doc), decl.name_span)),
                });
            }
            for f in &decl.fields {
                if span_contains(&f.span, off) {
                    return Some(Resolved {
                        hover: format!("{}: {}", f.name, ty_text(&f.ty)),
                        target: Some(Target::Field(doc_display(cx.doc), f.span)),
                    });
                }
            }
        }
        ItemKind::Extern(block) => {
            for d in &block.decls {
                if span_contains(&d.name_span, off) {
                    let sig = format!(
                        "fn {}({}) -> {}",
                        d.name,
                        d.params
                            .iter()
                            .map(|p| format!("{}: {}", p.name, ty_text(&p.ty)))
                            .collect::<Vec<_>>()
                            .join(", "),
                        ty_text(&d.ret)
                    );
                    return Some(Resolved {
                        hover: format!("unsafe extern (C)\n{sig}"),
                        target: Some(Target::Extern(doc_display(cx.doc), d.name_span)),
                    });
                }
            }
        }
        ItemKind::Import(_) => {}
        ItemKind::State(decl) => {
            for b in &decl.bindings {
                if span_contains(&b.name_span, off) {
                    return Some(Resolved {
                        hover: format!("state {}: {}", b.name, ty_text(&b.ty)),
                        target: Some(Target::State(doc_display(cx.doc), b.name_span)),
                    });
                }
            }
        }
        ItemKind::Migrate(decl) => {
            if span_contains(&decl.func.name_span, off) {
                let sig = fn_sig_text(&decl.func.name, &decl.func.params, &decl.func.ret);
                return Some(Resolved {
                    hover: format!("migrate {sig}"),
                    target: Some(Target::Func(doc_display(cx.doc), fn_name_span(&decl.func))),
                });
            }
        }
    }
    None
}

/// Display path of the open file as the project knows it.
fn doc_display(doc: &OpenDoc) -> String {
    doc.file_srcs
        .first()
        .map(|(p, _)| p.clone())
        .unwrap_or_else(|| doc.path.to_string_lossy().into_owned())
}

fn span_contains(span: &Span, off: usize) -> bool {
    (span.start as usize) <= off && off < (span.end as usize)
}

fn parse_buffer(text: &str) -> Option<Program> {
    let (tokens, lex_errors) = toto_compiler::lexer::lex(text);
    if !lex_errors.is_empty() {
        return None;
    }
    let (program, parse_errors) = toto_compiler::parser::parse(tokens, text);
    if !parse_errors.is_empty() {
        return None;
    }
    Some(program)
}

/// An item's own span bounds the hover search to that item.
fn item_span(item: &toto_compiler::ast::Item) -> Span {
    item.span
}

fn fn_name_span(decl: &toto_compiler::ast::FnDecl) -> Span {
    decl.name_span
}

fn push_params(scopes: &mut Vec<HashMap<String, Span>>, params: &[Param]) {
    let mut map = HashMap::new();
    for p in params {
        map.insert(p.name.clone(), p.name_span);
    }
    scopes.push(map);
}

fn lookup(scopes: &[HashMap<String, Span>], name: &str) -> Option<Span> {
    for scope in scopes.iter().rev() {
        if let Some(span) = scope.get(name) {
            return Some(*span);
        }
    }
    None
}

/// Walk a block with lexical scopes; resolves the name at `off`.
fn resolve_block(
    program: &Program,
    block: &Block,
    off: usize,
    scopes: &mut Vec<HashMap<String, Span>>,
    cx: &Cx,
) -> Option<Resolved> {
    scopes.push(HashMap::new());
    for stmt in &block.stmts {
        if let Some(r) = resolve_stmt(program, stmt, off, scopes, cx) {
            scopes.pop();
            return Some(r);
        }
    }
    if let Some(tail) = &block.tail
        && let Some(r) = resolve_expr(program, tail, off, scopes, cx)
    {
        scopes.pop();
        return Some(r);
    }
    scopes.pop();
    None
}

fn resolve_stmt(
    program: &Program,
    stmt: &Stmt,
    off: usize,
    scopes: &mut Vec<HashMap<String, Span>>,
    cx: &Cx,
) -> Option<Resolved> {
    match &stmt.node {
        StmtKind::Let {
            name,
            name_span,
            ty,
            is_mut: _,
            value,
        } => {
            if span_contains(name_span, off) {
                let hover = match ty {
                    Some(t) => format!("let {}: {}", name, ty_text(t)),
                    None => format!("let {name}"),
                };
                return Some(Resolved {
                    hover,
                    target: Some(Target::Local(*name_span)),
                });
            }
            // The value sees the OUTER scope (no self-reference),
            // then the binding enters scope for later statements.
            if let Some(r) = resolve_expr(program, value, off, scopes, cx) {
                return Some(r);
            }
            if let Some(scope) = scopes.last_mut() {
                scope.insert(name.clone(), *name_span);
            }
            None
        }
        StmtKind::Assign { target, value } => {
            resolve_assign_target(program, target, off, scopes, cx)
                .or_else(|| resolve_expr(program, value, off, scopes, cx))
        }
        StmtKind::Expr(e) | StmtKind::Return(e) => resolve_expr(program, e, off, scopes, cx),
        StmtKind::Break | StmtKind::Continue => None,
        StmtKind::While { cond, body } => resolve_expr(program, cond, off, scopes, cx)
            .or_else(|| resolve_block(program, body, off, scopes, cx)),
        StmtKind::For {
            name,
            name_span,
            iter,
            body,
        } => {
            if span_contains(name_span, off) {
                return Some(Resolved {
                    hover: format!("for {name}"),
                    target: Some(Target::Local(*name_span)),
                });
            }
            if let Some(r) = resolve_expr(program, iter, off, scopes, cx) {
                return Some(r);
            }
            scopes.push(HashMap::new());
            scopes
                .last_mut()
                .expect("pushed")
                .insert(name.clone(), *name_span);
            let r = resolve_block(program, body, off, scopes, cx);
            scopes.pop();
            r
        }
        StmtKind::Go { body } => resolve_block(program, body, off, scopes, cx),
        StmtKind::Select { cases, default } => {
            for case in cases {
                match case {
                    toto_compiler::ast::SelectCase::Recv { name, chan, body } => {
                        if let Some(r) = resolve_expr(program, chan, off, scopes, cx) {
                            return Some(r);
                        }
                        scopes.push(HashMap::new());
                        if let Some((n, span)) = name {
                            scopes.last_mut().expect("pushed").insert(n.clone(), *span);
                            if span_contains(span, off) {
                                let r = Resolved {
                                    hover: format!("let {n}"),
                                    target: Some(Target::Local(*span)),
                                };
                                scopes.pop();
                                return Some(r);
                            }
                        }
                        let r = resolve_block(program, body, off, scopes, cx);
                        scopes.pop();
                        if r.is_some() {
                            return r;
                        }
                    }
                    toto_compiler::ast::SelectCase::Send { chan, value, body } => {
                        if let Some(r) = resolve_expr(program, chan, off, scopes, cx)
                            .or_else(|| resolve_expr(program, value, off, scopes, cx))
                        {
                            return Some(r);
                        }
                        if let Some(r) = resolve_block(program, body, off, scopes, cx) {
                            return Some(r);
                        }
                    }
                }
            }
            if let Some(d) = default {
                return resolve_block(program, d, off, scopes, cx);
            }
            None
        }
    }
}

fn resolve_assign_target(
    program: &Program,
    target: &AssignTarget,
    off: usize,
    scopes: &mut Vec<HashMap<String, Span>>,
    cx: &Cx,
) -> Option<Resolved> {
    match target {
        AssignTarget::Var(name, span) => resolve_var(program, name, *span, off, scopes, cx),
        AssignTarget::Field(base, name, _) => resolve_expr(program, base, off, scopes, cx)
            .or_else(|| resolve_field(program, base, name, off, scopes, cx)),
        AssignTarget::Index(base, index, _) => resolve_expr(program, base, off, scopes, cx)
            .or_else(|| resolve_expr(program, index, off, scopes, cx)),
    }
}

/// Resolve one expression, innermost first.
fn resolve_expr(
    program: &Program,
    expr: &Expr,
    off: usize,
    scopes: &mut Vec<HashMap<String, Span>>,
    cx: &Cx,
) -> Option<Resolved> {
    match &expr.node {
        ExprKind::Var(name) => resolve_var(program, name, expr.span, off, scopes, cx),
        ExprKind::Call { callee, args } => {
            for a in args {
                if span_contains(&a.span, off)
                    && let Some(r) = resolve_expr(program, a, off, scopes, cx)
                {
                    return Some(r);
                }
            }
            resolve_expr(program, callee, off, scopes, cx)
        }
        ExprKind::Field { base, name } => resolve_expr(program, base, off, scopes, cx)
            .or_else(|| resolve_field(program, base, name, off, scopes, cx)),
        ExprKind::Index { base, index } => resolve_expr(program, base, off, scopes, cx)
            .or_else(|| resolve_expr(program, index, off, scopes, cx)),
        ExprKind::StructLit { name, fields, .. } => {
            if expr.span.start as usize <= off
                && off < (expr.span.start as usize + name.len())
                && let Some(r) = resolve_global(program, name, cx)
            {
                return Some(r);
            }
            for (_, value, _) in fields {
                if span_contains(&value.span, off)
                    && let Some(r) = resolve_expr(program, value, off, scopes, cx)
                {
                    return Some(r);
                }
            }
            None
        }
        ExprKind::Binary { left, right, .. } => resolve_expr(program, left, off, scopes, cx)
            .or_else(|| resolve_expr(program, right, off, scopes, cx)),
        ExprKind::Range(a, b) => resolve_expr(program, a, off, scopes, cx)
            .or_else(|| resolve_expr(program, b, off, scopes, cx)),
        ExprKind::Unary { expr: inner, .. } => resolve_expr(program, inner, off, scopes, cx),
        ExprKind::Send { chan, value } => resolve_expr(program, chan, off, scopes, cx)
            .or_else(|| resolve_expr(program, value, off, scopes, cx)),
        ExprKind::Recv { chan } => resolve_expr(program, chan, off, scopes, cx),
        ExprKind::Question(inner) => resolve_expr(program, inner, off, scopes, cx),
        ExprKind::If {
            cond,
            then_b,
            else_b,
        } => resolve_expr(program, cond, off, scopes, cx)
            .or_else(|| resolve_block(program, then_b, off, scopes, cx))
            .or_else(|| {
                else_b
                    .as_ref()
                    .and_then(|b| resolve_block(program, b, off, scopes, cx))
            }),
        ExprKind::Closure { params, body, .. } => {
            push_params(scopes, params);
            for p in params {
                if span_contains(&p.name_span, off) {
                    let r = Resolved {
                        hover: format!("{}: {}", p.name, ty_text(&p.ty)),
                        target: Some(Target::Local(p.name_span)),
                    };
                    scopes.pop();
                    return Some(r);
                }
            }
            let r = resolve_block(program, body, off, scopes, cx);
            scopes.pop();
            r
        }
        ExprKind::Array(elems) => {
            for e in elems {
                if span_contains(&e.span, off)
                    && let Some(r) = resolve_expr(program, e, off, scopes, cx)
                {
                    return Some(r);
                }
            }
            None
        }
        ExprKind::ChanNew { .. }
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Unit
        | ExprKind::None => None,
        ExprKind::Ok(x) | ExprKind::Err(x) | ExprKind::Some(x) => {
            resolve_expr(program, x, off, scopes, cx)
        }
    }
}

/// One variable occurrence: binding span from scope, or a global.
fn resolve_var(
    program: &Program,
    name: &str,
    span: Span,
    off: usize,
    scopes: &[HashMap<String, Span>],
    cx: &Cx,
) -> Option<Resolved> {
    if !span_contains(&span, off) {
        return None;
    }
    if let Some(def) = lookup(scopes, name) {
        let ty = local_type_text(cx, off).unwrap_or_else(|| String::from("unknown"));
        return Some(Resolved {
            hover: format!("{name}: {ty}"),
            target: Some(Target::Local(def)),
        });
    }
    resolve_global(program, name, cx)
}

/// Top-level names: current file first, then imported project files,
/// then builtins.
fn resolve_global(program: &Program, name: &str, cx: &Cx) -> Option<Resolved> {
    for item in &program.items {
        match &item.kind {
            ItemKind::Fn(d) if d.name == name => {
                return Some(Resolved {
                    hover: fn_sig_text(&d.name, &d.params, &d.ret),
                    target: Some(Target::Func(doc_display(cx.doc), fn_name_span(d))),
                });
            }
            ItemKind::Struct(d) if d.name == name => {
                return Some(Resolved {
                    hover: struct_sig_text(cx.doc, &d.name),
                    target: Some(Target::Struct(doc_display(cx.doc), d.name_span)),
                });
            }
            ItemKind::Extern(b) => {
                for d in &b.decls {
                    if d.name == name {
                        let sig = format!(
                            "fn {}({}) -> {}",
                            d.name,
                            d.params
                                .iter()
                                .map(|p| format!("{}: {}", p.name, ty_text(&p.ty)))
                                .collect::<Vec<_>>()
                                .join(", "),
                            ty_text(&d.ret)
                        );
                        return Some(Resolved {
                            hover: format!("unsafe extern (C)\n{sig}"),
                            target: Some(Target::Extern(doc_display(cx.doc), d.name_span)),
                        });
                    }
                }
            }
            ItemKind::State(decl) => {
                for b in &decl.bindings {
                    if b.name == name {
                        return Some(Resolved {
                            hover: format!("state {}: {}", b.name, ty_text(&b.ty)),
                            target: Some(Target::State(doc_display(cx.doc), b.name_span)),
                        });
                    }
                }
            }
            ItemKind::Migrate(decl) if decl.func.name == name => {
                let sig = fn_sig_text(&decl.func.name, &decl.func.params, &decl.func.ret);
                return Some(Resolved {
                    hover: format!("migrate {sig}"),
                    target: Some(Target::Func(doc_display(cx.doc), fn_name_span(&decl.func))),
                });
            }
            _ => {}
        }
    }
    // Imported project files (parsed from the cached sources).
    for (path, src) in &cx.doc.file_srcs {
        if *path == doc_display(cx.doc) {
            continue;
        }
        let other = parse_buffer(src)?;
        for item in &other.items {
            match &item.kind {
                ItemKind::Fn(d) if d.name == name => {
                    return Some(Resolved {
                        hover: fn_sig_text(&d.name, &d.params, &d.ret),
                        target: Some(Target::Func(path.clone(), fn_name_span(d))),
                    });
                }
                ItemKind::Struct(d) if d.name == name => {
                    return Some(Resolved {
                        hover: format!("struct {}", d.name),
                        target: Some(Target::Struct(path.clone(), d.name_span)),
                    });
                }
                ItemKind::State(decl) => {
                    for b in &decl.bindings {
                        if b.name == name {
                            return Some(Resolved {
                                hover: format!("state {}: {}", b.name, ty_text(&b.ty)),
                                target: Some(Target::State(path.clone(), b.name_span)),
                            });
                        }
                    }
                }
                ItemKind::Migrate(decl) if decl.func.name == name => {
                    let sig = fn_sig_text(&decl.func.name, &decl.func.params, &decl.func.ret);
                    return Some(Resolved {
                        hover: format!("migrate {sig}"),
                        target: Some(Target::Func(path.clone(), fn_name_span(&decl.func))),
                    });
                }
                _ => {}
            }
        }
    }
    builtin_doc(name).map(|text| Resolved {
        hover: text.to_string(),
        target: Some(Target::Builtin),
    })
}

/// `base.field`: the field's declared type and definition span.
fn resolve_field(
    _program: &Program,
    base: &Expr,
    name: &str,
    off: usize,
    _scopes: &mut Vec<HashMap<String, Span>>,
    cx: &Cx,
) -> Option<Resolved> {
    // Only the field name itself resolves here; the base was tried
    // by the caller. The name span is re-derived as
    // `base.end + 1 .. + len` — the parser takes `.name` directly in
    // canonical code; anything else misses cleanly.
    let start = base.span.end as usize + 1;
    if off < start || off >= start + name.len() {
        return None;
    }
    let base_ty = local_type_text(cx, base.span.start as usize)?;
    // A struct-typed base displays as its bare name (`Ty::Struct`);
    // anything else is not a field access.
    let struct_name = base_ty
        .strip_prefix("struct ")
        .and_then(|s| s.split([' ', '{']).next())
        .unwrap_or(&base_ty);
    if !struct_name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let checked = cx.doc.checked.as_ref()?;
    let decl = checked.structs.get(struct_name)?;
    let (_, field_ty) = decl.fields.iter().find(|(n, _)| n == name)?;
    for (path, src) in &cx.doc.file_srcs {
        let other = parse_buffer(src)?;
        for item in &other.items {
            if let ItemKind::Struct(d) = &item.kind
                && d.name == struct_name
            {
                for f in &d.fields {
                    if f.name == name {
                        return Some(Resolved {
                            hover: format!("{struct_name}.{name}: {field_ty}"),
                            target: Some(Target::Field(path.clone(), f.span)),
                        });
                    }
                }
            }
        }
    }
    None
}

fn fn_sig_text(name: &str, params: &[Param], ret: &toto_compiler::ast::Type) -> String {
    format!(
        "fn {name}({}) -> {}",
        params
            .iter()
            .map(|p| format!(
                "{}{}: {}",
                if p.is_mut { "mut " } else { "" },
                p.name,
                ty_text(&p.ty)
            ))
            .collect::<Vec<_>>()
            .join(", "),
        ty_text(ret)
    )
}

fn struct_sig_text(doc: &OpenDoc, name: &str) -> String {
    if let Some(checked) = &doc.checked
        && let Some(decl) = checked.structs.get(name)
    {
        let fields: Vec<String> = decl
            .fields
            .iter()
            .map(|(n, t)| format!("{n}: {t}"))
            .collect();
        return format!("struct {name} {{ {} }}", fields.join(", "));
    }
    format!("struct {name}")
}

/// Canonical surface spelling of an AST type (mirrors the type
/// parser: `name`, `name<args>`, `*T`, `fn(params) -> ret`).
fn ty_text(ty: &toto_compiler::ast::Type) -> String {
    match &ty.kind {
        toto_compiler::ast::TypeKind::Ptr(inner) => format!("*{}", ty_text(inner)),
        toto_compiler::ast::TypeKind::Named { name, args } => {
            if args.is_empty() {
                name.clone()
            } else {
                let inner: Vec<String> = args.iter().map(ty_text).collect();
                format!("{name}<{}>", inner.join(", "))
            }
        }
        toto_compiler::ast::TypeKind::Fn { params, ret } => {
            let inner: Vec<String> = params.iter().map(ty_text).collect();
            format!("fn({}) -> {}", inner.join(", "), ty_text(ret))
        }
    }
}

/// Hover type for a local use: the innermost checked expression at
/// the use offset carries the exact type (names are unique
/// program-wide, so the HIR function matches by enclosing name).
fn local_type_text(cx: &Cx, off: usize) -> Option<String> {
    let checked = cx.doc.checked.as_ref()?;
    let enclosing = cx.enclosing.as_ref()?;
    for item in &checked.program.items {
        if let toto_compiler::hir::HItemKind::Fn(hfn) = &item.kind
            && &hfn.name == enclosing
        {
            return hir_type_at(&hfn.body, off).map(|t| t.to_string());
        }
    }
    None
}

/// Innermost checked expression type containing `off`.
fn hir_type_at(block: &HBlock, off: usize) -> Option<Ty> {
    for stmt in &block.stmts {
        if let Some(t) = hstmt_type_at(stmt, off) {
            return Some(t);
        }
    }
    if let Some(tail) = &block.tail {
        return hexpr_type_at(tail, off);
    }
    None
}

fn hstmt_type_at(stmt: &toto_compiler::hir::HStmt, off: usize) -> Option<Ty> {
    use toto_compiler::hir::HStmtKind;
    match &stmt.kind {
        HStmtKind::Let { value, .. }
        | HStmtKind::Assign { value, .. }
        | HStmtKind::Expr(value)
        | HStmtKind::Return(value) => hexpr_type_at(value, off),
        HStmtKind::While { cond, body } => {
            hexpr_type_at(cond, off).or_else(|| hir_type_at(body, off))
        }
        HStmtKind::For { iter, body, .. } => {
            hexpr_type_at(iter, off).or_else(|| hir_type_at(body, off))
        }
        HStmtKind::Go { body, .. } => hir_type_at(body, off),
        HStmtKind::Select { cases, default } => {
            for case in cases {
                let found = match case {
                    toto_compiler::hir::HSelectCase::Recv { chan, body, .. } => {
                        hexpr_type_at(chan, off).or_else(|| hir_type_at(body, off))
                    }
                    toto_compiler::hir::HSelectCase::Send { chan, value, body } => {
                        hexpr_type_at(chan, off)
                            .or_else(|| hexpr_type_at(value, off))
                            .or_else(|| hir_type_at(body, off))
                    }
                };
                if found.is_some() {
                    return found;
                }
            }
            default.as_ref().and_then(|b| hir_type_at(b, off))
        }
        HStmtKind::Break | HStmtKind::Continue => None,
    }
}

fn hexpr_type_at(expr: &HExpr, off: usize) -> Option<Ty> {
    if !span_contains(&expr.span, off) {
        return None;
    }
    // Innermost first: try children before reporting this node.
    let child = match &expr.kind {
        HExprKind::Call { callee, args } => hexpr_type_at(callee, off).or_else(|| {
            args.iter().find_map(|a| {
                if span_contains(&a.span, off) {
                    hexpr_type_at(a, off)
                } else {
                    None
                }
            })
        }),
        HExprKind::CallBuiltin { args, .. } | HExprKind::Array(args) => args.iter().find_map(|a| {
            if span_contains(&a.span, off) {
                hexpr_type_at(a, off)
            } else {
                None
            }
        }),
        HExprKind::Field { base, .. }
        | HExprKind::Unary { expr: base, .. }
        | HExprKind::Recv { chan: base }
        | HExprKind::Question { operand: base, .. } => hexpr_type_at(base, off),
        HExprKind::CallForeign { args, .. } => args.iter().find_map(|a| {
            if span_contains(&a.span, off) {
                hexpr_type_at(a, off)
            } else {
                None
            }
        }),
        HExprKind::ChanNew { cap, .. } => cap.as_ref().and_then(|c| {
            span_contains(&c.span, off)
                .then(|| hexpr_type_at(c, off))
                .flatten()
        }),
        HExprKind::Index { base, index } => {
            hexpr_type_at(base, off).or_else(|| hexpr_type_at(index, off))
        }
        HExprKind::Binary { left, right, .. }
        | HExprKind::Range(left, right)
        | HExprKind::Send {
            chan: left,
            value: right,
        } => hexpr_type_at(left, off).or_else(|| hexpr_type_at(right, off)),
        HExprKind::If {
            cond,
            then_b,
            else_b,
        } => hexpr_type_at(cond, off)
            .or_else(|| hir_type_at(then_b, off))
            .or_else(|| else_b.as_ref().and_then(|b| hir_type_at(b, off))),
        HExprKind::Closure { body, .. } => hir_type_at(body, off),
        HExprKind::StructLit { fields, .. } => fields.iter().find_map(|(_, v, _)| {
            if span_contains(&v.span, off) {
                hexpr_type_at(v, off)
            } else {
                None
            }
        }),
        HExprKind::Ok(x) | HExprKind::Err(x) | HExprKind::Some(x) => hexpr_type_at(x, off),
        _ => None,
    };
    child.or(Some(expr.ty.clone()))
}

/// `textDocument/hover`: Markdown signature/type, or null outside
/// any known name.
fn hover_at(docs: &HashMap<String, OpenDoc>, params: &J) -> J {
    let (uri, line, character) = doc_position(params);
    docs.get(&uri)
        .and_then(|d| resolve_at(d, line, character))
        .map(|r| {
            J::Obj(vec![(
                "contents".to_string(),
                J::Obj(vec![
                    ("kind".to_string(), J::Str("markdown".to_string())),
                    (
                        "value".to_string(),
                        J::Str(format!("```toto\n{}\n```", r.hover)),
                    ),
                ]),
            )])
        })
        .unwrap_or(J::Null)
}

/// `textDocument/definition`: jump target, or null (builtins have no
/// local definition to jump to).
fn definition_at(docs: &HashMap<String, OpenDoc>, params: &J) -> J {
    let (uri, line, character) = doc_position(params);
    let Some(doc) = docs.get(&uri) else {
        return J::Null;
    };
    let Some(resolved) = resolve_at(doc, line, character) else {
        return J::Null;
    };
    let Some(target) = resolved.target else {
        return J::Null;
    };
    let (path, span) = match target {
        Target::Local(span) => return location(&doc.uri, &doc.text, span),
        Target::Func(path, span) | Target::Struct(path, span) | Target::Field(path, span) => {
            (path, span)
        }
        Target::State(path, span) => (path, span),
        Target::Extern(path, span) => (path, span),
        Target::Builtin => return J::Null,
    };
    let src = doc
        .file_srcs
        .iter()
        .find(|(p, _)| p == &path)
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    location(&file_uri(&doc.path, &path), &src, span)
}

fn doc_position(params: &J) -> (String, u64, u64) {
    let uri = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(J::as_str)
        .unwrap_or("")
        .to_string();
    let pos = params.get("position").cloned().unwrap_or(J::Null);
    let line = pos.get("line").and_then(J::as_u64).unwrap_or(0);
    let character = pos.get("character").and_then(J::as_u64).unwrap_or(0);
    (uri, line, character)
}

fn location(uri: &str, text: &str, span: Span) -> J {
    J::Obj(vec![
        ("uri".to_string(), J::Str(uri.to_string())),
        ("range".to_string(), range_json(text, span.start, span.end)),
    ])
}

/// Frozen builtin name to its one-line signature documentation.
/// Display-only (signatures live in `check::check_builtin`); the key
/// set must stay exactly `BUILTIN_NAMES`.
fn builtin_doc(name: &str) -> Option<&'static str> {
    Some(match name {
        "print" => "print(v) — print a value without a trailing newline",
        "println" => "println(v) — print a value with a trailing newline",
        "chan" => "chan<T>(n?) — make an unbounded (or capacity-n) channel",
        "close" => "close(ch) — close a channel; receivers drain then see closed",
        "closed" => "closed(ch) — true once a channel is closed and drained",
        "len" => "len(x) — chars in a string, elements in an array",
        "split" => "split(s, sep) — split a string on a separator",
        "trim" => "trim(s) — strip surrounding whitespace",
        "upper" => "upper(s) — ASCII uppercase copy",
        "lower" => "lower(s) — ASCII lowercase copy",
        "contains" => "contains(s, sub) — substring test",
        "to_string" => "to_string(v) — render any value as a string",
        "parse_int" => "parse_int(s) — Result<int, string>: strict whole-string parse",
        "to_int" => "to_int(f) — truncate a float toward zero",
        "to_float" => "to_float(n) — widen an int to float",
        "is_nan" => "is_nan(f) — IEEE NaN test",
        "abs" => "abs(n) — absolute value (checked: overflows on int min)",
        "min" => "min(a, b) — smaller of two ints",
        "max" => "max(a, b) — larger of two ints",
        "sqrt" => "sqrt(f) — square root (NaN for negatives)",
        "floor" => "floor(f) — round down to an integer-valued float",
        "ceil" => "ceil(f) — round up to an integer-valued float",
        "push" => "push(a, v) — append to an array (shares until push)",
        "pop" => "pop(a) — remove and return the last element (panics when empty)",
        "read_file" => "read_file(path) — Result<string, string>: whole file as text",
        "write_file" => "write_file(path, contents) — replace a file",
        "file_exists" => "file_exists(path) — presence probe",
        _ => return None,
    })
}
