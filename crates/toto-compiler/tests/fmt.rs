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
                } else {
                    // Tracked for the exact refused-set pin below.
                    failures.push(format!("refused:{name}"));
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
    // Exact pins (Phase 6 flag b): any corpus addition that parses,
    // refuses, or runs changes these, loudly. The 17 refused are all
    // lex/parse-level failures — E0001 (4: diag01, diag02, diag03,
    // lex05), E0101 (incl. the parse10/parse11 state/migrate
    // placement errors), E0102 (3: diag04,
    // parse02 + the missing-semi shape), E0103, E0104, E0106 (2:
    // diag12, parse09), E0107, E0154, E0156, E0158 — never
    // check-time (type) errors, which all parse and therefore
    // format.
    const EXPECTED_REFUSED: &[&str] = &[
        "diag01_single_quote.gzv",
        "diag02_hash_comment.gzv",
        "diag03_and_suggest.gzv",
        "diag04_missing_semi.gzv",
        "diag05_bad_assign.gzv",
        "diag06_bare_if.gzv",
        "diag11_unclosed_eof.gzv",
        "diag12_multierror3.gzv",
        "lex05_bad_char.gzv",
        "parse02_multierror.gzv",
        "parse05_bare_select.gzv",
        "parse06_reject_generics_extern.gzv",
        "parse07_reject_match_mod.gzv",
        "parse08_method_call.gzv",
        "parse09_suggest_eq_unclosed.gzv",
        "parse10_state_in_fn.gzv",
        "parse11_migrate_in_fn.gzv",
    ];
    let mut refused_names: Vec<String> = failures
        .iter()
        .filter_map(|f| f.strip_prefix("refused:"))
        .map(str::to_string)
        .collect();
    refused_names.sort();
    let mut expected: Vec<String> = EXPECTED_REFUSED.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(refused_names, expected, "refused set changed");
    assert_eq!(formatted, 143, "formatted count changed");
    assert_eq!(refused, 17, "refused count changed");
    assert_eq!(ran, 46, "executed count changed");
    assert!(
        failures.iter().all(|f| f.starts_with("refused:")),
        "{} fmt failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("fmt: {formatted} formatted, {refused} refused, {ran} executed");
}

// Phase 6 flag (a): `pub` drops on re-emit, and that is safe because
// the parser discards top-level `pub` before the AST exists
// (`parse_item`: "accepted on top-level items and has no effect
// (flat namespace)"). Nothing downstream — checker, MIR, runtime,
// JIT — can observe it, so a formatted tree checks exactly like the
// original. Pin: format every file of the `pub`-using transitive
// project, assert `pub` is gone from all outputs, and the formatted
// tree still loads and checks clean.
#[test]
fn fmt_drops_pub_without_changing_meaning() {
    use std::collections::HashMap;
    let root = corpus_root().join("project").join("proj02_transitive");
    let rels = ["main.gzv", "lib/a.gzv", "lib/b.gzv", "lib/shared.gzv"];
    let mut formatted: HashMap<String, String> = HashMap::new();
    let mut saw_pub = false;
    for rel in rels {
        let src = std::fs::read_to_string(root.join(rel)).expect("read project file");
        saw_pub |= src.lines().any(|l| l.starts_with("pub "));
        let program = parse(&src).unwrap_or_else(|| panic!("{rel} parses"));
        let once = toto_compiler::fmt::format_program(&program);
        assert!(
            !once.lines().any(|l| l.starts_with("pub ")),
            "{rel}: formatted output keeps `pub`"
        );
        formatted.insert(rel.to_string(), once);
    }
    assert!(saw_pub, "fixture lost its `pub` items");
    let entry = formatted.remove("main.gzv").expect("main formatted");
    let loaded = toto_compiler::project::load_and_check(
        "main.gzv",
        &entry,
        &|p| formatted.get(p).cloned(),
        &[],
    );
    assert!(
        loaded.is_ok(),
        "formatted pub-using project no longer checks"
    );
}
