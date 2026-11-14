//! Toto compiler: lexer, parser, diagnostics, tree-walking interpreter.
//! Phase 1 delivers the core subset (spec `docs/spec.md`); the checker
//! and IR arrive in Phase 2. The runtime crate stays empty until Phase 3.

pub mod ast;
pub mod builtins;
pub mod diag;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;

use diag::{Diag, render_all};
use interp::{Interp, TraceFrame};

/// What running a source file produces.
pub enum Outcome {
    /// `main` returned an exit code, with captured stdout.
    Success { code: i32, stdout: Vec<u8> },
    /// Static (lexer/parser) diagnostics; nothing executed.
    Static(Vec<Diag>),
    /// A runtime panic with its call trace and any stdout so far.
    RuntimePanic {
        diag: Diag,
        trace: Vec<TraceFrame>,
        stdout: Vec<u8>,
    },
}

/// Lex, parse, and interpret a source file. Never panics on user input:
/// every failure surfaces as `Static` or `RuntimePanic`.
pub fn run_source(src: &str, _filename: &str) -> Outcome {
    let (tokens, lex_errors) = lexer::lex(src);
    if !lex_errors.is_empty() {
        let diags = lex_errors
            .into_iter()
            .map(|e| Diag::new(e.code, e.message, e.span, e.help))
            .collect();
        return Outcome::Static(diags);
    }
    let (program, parse_errors) = parser::parse(tokens, src);
    if !parse_errors.is_empty() {
        return Outcome::Static(parse_errors);
    }
    let mut interp = Interp::new(&program);
    match interp.run() {
        Ok(code) => Outcome::Success {
            code,
            stdout: std::mem::take(&mut interp.output),
        },
        Err((diag, trace)) => Outcome::RuntimePanic {
            diag,
            trace,
            stdout: std::mem::take(&mut interp.output),
        },
    }
}

/// Format a runtime panic per the spec §6 contract.
pub fn render_panic(diag: &Diag, trace: &[TraceFrame], src: &str, filename: &str) -> String {
    let mut out = diag.render(src, filename, true);
    out.push_str("stack trace:\n");
    for frame in trace.iter().rev() {
        let (line, col) = frame.span.line_col(src);
        out.push_str(&format!("  in {} at {filename}:{line}:{col}\n", frame.name));
    }
    out
}

/// Format static diagnostics (spec §9.1).
pub fn render_errors(diags: &[Diag], src: &str, filename: &str) -> String {
    render_all(diags, src, filename, false)
}
