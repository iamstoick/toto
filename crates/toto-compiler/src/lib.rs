//! Toto compiler: lexer, parser, diagnostics, tree-walking interpreter,
//! checker, and MIR (spec `docs/spec.md`). Concurrent execution lives in
//! the `toto-runtime` crate (Phase 3); this crate's interpreters are
//! sequential-only.

pub mod ast;
pub mod builtins;
pub mod check;
pub mod diag;
pub mod hir;
pub mod interp;
pub mod lexer;
pub mod mir;
pub mod mir_interp;
pub mod ops;
pub mod parser;
pub mod span;
pub mod token;

use ast::Program;
use diag::{Diag, render_all};
use hir::CheckedProgram;
use interp::{Interp, TraceFrame};

/// What running a source file produces.
#[derive(Debug)]
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

/// Parse and check a source file, returning the AST plus typed HIR.
/// Static failures (lexer, parser, checker) come back as diagnostics;
/// the caller decides how to execute (tree-walker or runtime executor).
pub fn parse_and_check(src: &str) -> Result<(Program, CheckedProgram), Vec<Diag>> {
    let (tokens, lex_errors) = lexer::lex(src);
    if !lex_errors.is_empty() {
        let diags = lex_errors
            .into_iter()
            .map(|e| Diag::new(e.code, e.message, e.span, e.help))
            .collect();
        return Err(diags);
    }
    let (program, parse_errors) = parser::parse(tokens, src);
    if !parse_errors.is_empty() {
        return Err(parse_errors);
    }
    let (checked, check_errors) = check::check(&program);
    match checked {
        Some(checked) => {
            debug_assert!(check_errors.is_empty());
            Ok((program, checked))
        }
        None => Err(check_errors),
    }
}

/// Lex, parse, and interpret a source file. Never panics on user input:
/// every failure surfaces as `Static` or `RuntimePanic`.
///
/// Sequential programs only: concurrent programs (those using `go`,
/// channels, `select`, or `close`) are rejected by the routing tripwire
/// (`E0215`) — run them through `toto run`, which routes to the runtime
/// executor.
pub fn run_source(src: &str, _filename: &str) -> Outcome {
    let program = match parse_and_check(src) {
        Ok((program, _)) => program,
        Err(diags) => return Outcome::Static(diags),
    };
    // The tree-walking interpreter recurses on the host stack (one Rust
    // call chain per Toto call level). The 8,192-frame semantic limit
    // (spec §5.9) needs more host stack than the default 8 MiB in debug
    // builds, so interpretation runs on a dedicated large-stack thread.
    // A host panic here is an interpreter bug, never a user error: let it
    // propagate so tests fail loudly instead of hiding it.
    let (run_result, output) = std::thread::Builder::new()
        .name("toto-interp".to_string())
        .stack_size(256 << 20)
        .spawn(move || {
            let mut interp = Interp::new(&program);
            let result = interp.run();
            (result, interp.output)
        })
        .expect("failed to spawn interpreter thread")
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
    match run_result {
        Ok(code) => Outcome::Success {
            code,
            stdout: output,
        },
        Err((diag, trace)) => Outcome::RuntimePanic {
            diag,
            trace,
            stdout: output,
        },
    }
}

/// Frames shown per side when truncating a deep trace (spec §6).
const TRACE_HEAD: usize = 20;
const TRACE_TAIL: usize = 2;

/// Format a runtime panic per the spec §6 contract. Traces longer than
/// `TRACE_HEAD + TRACE_TAIL` frames are truncated to the innermost
/// `TRACE_HEAD` plus the outermost `TRACE_TAIL`, with an omission count.
pub fn render_panic(diag: &Diag, trace: &[TraceFrame], src: &str, filename: &str) -> String {
    let mut out = diag.render(src, filename, true);
    out.push_str("stack trace:\n");
    let frames: Vec<&TraceFrame> = if trace.len() <= TRACE_HEAD + TRACE_TAIL {
        trace.iter().rev().collect()
    } else {
        let mut v: Vec<&TraceFrame> = trace.iter().rev().take(TRACE_HEAD).collect();
        v.extend(trace.iter().take(TRACE_TAIL));
        v
    };
    for (i, frame) in frames.iter().enumerate() {
        if trace.len() > TRACE_HEAD + TRACE_TAIL && i == TRACE_HEAD {
            out.push_str(&format!(
                "  ... ({} frames omitted) ...\n",
                trace.len() - TRACE_HEAD - TRACE_TAIL
            ));
        }
        let (line, col) = frame.span.line_col(src);
        out.push_str(&format!("  in {} at {filename}:{line}:{col}\n", frame.name));
    }
    out
}

/// Format static diagnostics (spec §9.1).
pub fn render_errors(diags: &[Diag], src: &str, filename: &str) -> String {
    render_all(diags, src, filename, false)
}
