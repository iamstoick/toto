//! Toto compiler: lexer, parser, diagnostics, tree-walking interpreter,
//! checker, and MIR (spec `docs/spec.md`). Concurrent execution lives in
//! the `toto-runtime` crate (Phase 3); this crate's interpreters are
//! sequential-only.

pub mod ast;
pub mod builtins;
pub mod check;
pub mod diag;
pub mod ffi;
pub mod fmt;
pub mod hir;
pub mod interp;
pub mod lexer;
pub mod mir;
pub mod mir_interp;
pub mod ops;
pub mod parser;
pub mod project;
pub mod span;
pub mod stdlib;
pub mod token;

use std::collections::HashMap;

use ast::Program;
use diag::{Diag, render_all};
use hir::CheckedProgram;
use interp::{Interp, TraceFrame};
use mir_interp::MirOutcome;

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
    let (program, checked) = match parse_and_check(src) {
        Ok((program, checked)) => (program, checked),
        Err(diags) => return Outcome::Static(diags),
    };
    match run_program(&program, &checked.externs) {
        (Ok(code), stdout) => Outcome::Success { code, stdout },
        (Err((diag, trace)), stdout) => Outcome::RuntimePanic {
            diag,
            trace,
            stdout,
        },
    }
}

/// Interpret an already-parsed program (single-file or merged project).
/// Never panics on user input: failures come back as values.
///
/// The tree-walking interpreter recurses on the host stack (one Rust
/// call chain per Toto call level). The 8,192-frame semantic limit
/// (spec §5.9) needs more host stack than the default 8 MiB in debug
/// builds, so interpretation runs on a dedicated large-stack thread.
/// A host panic here is an interpreter bug, never a user error: let it
/// propagate so tests fail loudly instead of hiding it.
pub fn run_program(
    program: &Program,
    externs: &HashMap<String, crate::hir::HExternFn>,
) -> MirOutcome {
    let program = program.clone();
    let externs = externs.clone();
    std::thread::Builder::new()
        .name("toto-interp".to_string())
        .stack_size(256 << 20)
        .spawn(move || {
            let mut interp = Interp::new(&program, externs);
            let result = interp.run();
            (result, interp.output)
        })
        .expect("failed to spawn interpreter thread")
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))
}

/// Frames shown per side when truncating a deep trace (spec §6).
const TRACE_HEAD: usize = 20;
const TRACE_TAIL: usize = 2;

/// Innermost-first visible frames plus the omission count, if truncated.
fn visible_frames(trace: &[TraceFrame]) -> (Vec<&TraceFrame>, Option<usize>) {
    if trace.len() <= TRACE_HEAD + TRACE_TAIL {
        (trace.iter().rev().collect(), None)
    } else {
        let mut v: Vec<&TraceFrame> = trace.iter().rev().take(TRACE_HEAD).collect();
        v.extend(trace.iter().take(TRACE_TAIL));
        (v, Some(trace.len() - TRACE_HEAD - TRACE_TAIL))
    }
}

/// Format a runtime panic per the spec §6 contract. Traces longer than
/// `TRACE_HEAD + TRACE_TAIL` frames are truncated to the innermost
/// `TRACE_HEAD` plus the outermost `TRACE_TAIL`, with an omission count.
pub fn render_panic(diag: &Diag, trace: &[TraceFrame], src: &str, filename: &str) -> String {
    let mut out = diag.render(src, filename, true);
    out.push_str("stack trace:\n");
    let (frames, omitted) = visible_frames(trace);
    for (i, frame) in frames.iter().enumerate() {
        if let Some(n) = omitted
            && i == TRACE_HEAD
        {
            out.push_str(&format!("  ... ({n} frames omitted) ...\n"));
        }
        let (line, col) = frame.span.line_col(src);
        out.push_str(&format!("  in {} at {filename}:{line}:{col}\n", frame.name));
    }
    out
}

/// Format a runtime panic for a multi-file project: the diagnostic and
/// each trace frame render against their own file's source. Attribution
/// is exact: function names are unique program-wide (merged checking
/// rejects duplicates with `E0311`), so the innermost frame's function
/// owns the panic site and each frame's function owns its line.
pub fn render_panic_multi(
    diag: &Diag,
    trace: &[TraceFrame],
    project: &crate::project::Project,
) -> String {
    let diag_file = trace
        .last()
        .map(|f| project.file_of_fn(&f.name))
        .unwrap_or(project.entry);
    let mut out = diag.render(
        &project.files[diag_file].src,
        &project.files[diag_file].path,
        true,
    );
    out.push_str("stack trace:\n");
    let (frames, omitted) = visible_frames(trace);
    for (i, frame) in frames.iter().enumerate() {
        if let Some(n) = omitted
            && i == TRACE_HEAD
        {
            out.push_str(&format!("  ... ({n} frames omitted) ...\n"));
        }
        let f = project.file_of_fn(&frame.name);
        let (line, col) = frame.span.line_col(&project.files[f].src);
        out.push_str(&format!(
            "  in {} at {}:{line}:{col}\n",
            frame.name, project.files[f].path
        ));
    }
    out
}

/// Format one file's static diagnostics against that file's source.
pub fn render_file_errors(diags: &[Diag], src: &str, filename: &str) -> String {
    render_errors(diags, src, filename)
}

/// Format static diagnostics (spec §9.1).
pub fn render_errors(diags: &[Diag], src: &str, filename: &str) -> String {
    render_all(diags, src, filename, false)
}
