//! Formatter gate (Phase 5, Stage 4): every corpus file that parses
//! must format idempotently (`fmt(fmt(x)) == fmt(x)`), and every
//! runnable single-file program must behave identically formatted as
//! unformatted.
//!
//! Panic goldens pin source locations, which formatting legitimately
//! moves, so status-1 files compare the exit code, stdout bytes, and
//! the diagnostic code + message (golden stderr first line), not the
//! full snippet text. Status-0 files compare exit + stdout exactly.
//! Exclusions mirror `sequential.rs`: `diag10_recursion_limit` (the
//! interpreter-only frame limit), concurrent programs (covered by the
//! runtime-side `fmt_equiv` test), and status-2 files (the formatter
//! refuses broken code, asserted below).

use std::path::{Path, PathBuf};

use toto_compiler::{Outcome, ast::Program, fmt, lexer, parse_and_check, parser};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
}

fn parse(src: &str) -> Option<Program> {
    let (tokens, lex_errors) = lexer::lex(src);
    if !lex_errors.is_empty() {
        return None;
    }
    let (program, parse_errors) = parser::parse(tokens, src);
    if !parse_errors.is_empty() {
        return None;
    }
    Some(program)
}

/// Golden stderr first line: `error[E0204]: message` for static
/// failures, `panic[E0204]: message` for runtime panics.
fn golden_head(stderr: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(stderr);
    let first = text.lines().next()?;
    let rest = first
        .strip_prefix("error[")
        .or_else(|| first.strip_prefix("panic["))?;
    let (code, message) = rest.split_once(']')?;
    Some((
        code.to_string(),
        message.strip_prefix(": ").unwrap_or(message).to_string(),
    ))
}

#[test]
fn fmt_idempotent_and_semantics_preserving() {
    let root = corpus_root();
    let mut areas: Vec<_> = std::fs::read_dir(&root)
        .expect("corpus root readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut failures = Vec::new();
    let mut formatted = 0usize;
    let mut refused = 0usize;
    let mut ran = 0usize;
    for area in areas {
        let mut files: Vec<_> = std::fs::read_dir(&area)
            .expect("area readable")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
            .collect();
        files.sort();
        for case in files {
            let name = case
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            let src = std::fs::read_to_string(&case).expect("read case");
            let Some(program) = parse(&src) else {
                // The formatter refuses broken code; every refusal
                // must be a static-failure golden (status 2).
                let status = std::fs::read_to_string(case.with_extension("status"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if status != "2" {
                    failures.push(format!("{name}: unparseable but status is {status}"));
                }
                refused += 1;
                continue;
            };
            let once = fmt::format_program(&program);
            let twice = parse(&once)
                .map(|p| fmt::format_program(&p))
                .unwrap_or_else(|| "<reparse failed>".to_string());
            if once != twice {
                failures.push(format!("{name}: not idempotent"));
                continue;
            }
            formatted += 1;
            if name == "diag10_recursion_limit.gzv" {
                continue;
            }
            // Semantics: single-file runnable programs only
            // (projects need the multi-file loader; concurrent ones
            // need the runtime executor — both covered elsewhere).
            let Ok((_p, checked)) = parse_and_check(&src) else {
                continue;
            };
            if checked.uses_concurrency {
                continue;
            }
            let status = std::fs::read_to_string(case.with_extension("status"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if status != "0" && status != "1" {
                continue;
            }
            let dir = case.parent().expect("parent").to_path_buf();
            let saved_cwd = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(&dir).expect("set case cwd");
            let outcome = toto_compiler::run_source(&once, &name);
            std::env::set_current_dir(&saved_cwd).expect("restore cwd");
            // Scratch files written by file-IO programs.
            for entry in std::fs::read_dir(&dir).expect("read case dir") {
                let path = entry.expect("entry").path();
                if path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with("_tmp.txt"))
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
            // `cli01_fileprog` writes outside the `_tmp` pattern.
            let _ = std::fs::remove_file(dir.join("cli01_tmp.txt"));
            match (status.as_str(), outcome) {
                ("0", Outcome::Success { code, stdout }) => {
                    let want_out = std::fs::read(case.with_extension("stdout")).unwrap_or_default();
                    if stdout != want_out {
                        failures.push(format!("{name}: formatted stdout differs"));
                    }
                    if code != 0 {
                        failures.push(format!("{name}: formatted exit {code}, want 0"));
                    }
                }
                ("1", Outcome::RuntimePanic { diag, stdout, .. }) => {
                    let want_out = std::fs::read(case.with_extension("stdout")).unwrap_or_default();
                    if stdout != want_out {
                        failures.push(format!("{name}: formatted panic-stdout differs"));
                    }
                    let want_err = std::fs::read(case.with_extension("stderr")).unwrap_or_default();
                    match golden_head(&want_err) {
                        Some((code, message)) if code == diag.code && message == diag.message => {}
                        other => failures.push(format!(
                            "{name}: formatted panic {:?} vs golden {other:?}",
                            (diag.code, diag.message)
                        )),
                    }
                }
                (s, o) => failures.push(format!(
                    "{name}: status {s} vs formatted outcome {}",
                    match o {
                        Outcome::Success { .. } => "ok",
                        Outcome::RuntimePanic { .. } => "panic",
                        Outcome::Static(_) => "static",
                    }
                )),
            }
            ran += 1;
        }
    }
    assert!(formatted > 100, "only {formatted} files formatted");
    assert!(refused > 0, "no broken files refused");
    assert!(
        failures.is_empty(),
        "{} fmt failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("fmt: {formatted} formatted, {refused} refused, {ran} executed");
}
