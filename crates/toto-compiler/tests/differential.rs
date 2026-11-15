//! Differential harness (Phase 2, Stage 2): every positive corpus
//! program runs through BOTH the tree-walking interpreter (the semantics
//! reference) and the MIR interpreter, requiring identical exit codes,
//! stdout, and panic rendering. Any divergence is a bug.
//!
//! File-IO programs run with the case directory as cwd (like `toto run`
//! from the corpus runner); scratch `_tmp.txt` files are cleaned up.

use std::path::{Path, PathBuf};

use toto_compiler::{Outcome, check, lexer, mir_interp, parser, render_panic, run_source};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
}

fn positive_cases() -> Vec<PathBuf> {
    let root = corpus_root();
    let mut areas: Vec<_> = std::fs::read_dir(&root)
        .expect("corpus root readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut out = Vec::new();
    for area in areas {
        let mut files: Vec<_> = std::fs::read_dir(&area)
            .expect("area readable")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
            .collect();
        files.sort();
        for case in files {
            let status_path = case.with_extension("status");
            let status = std::fs::read_to_string(&status_path)
                .unwrap_or_default()
                .trim()
                .to_string();
            if status == "0" {
                out.push(case);
            }
        }
    }
    out
}

fn cleanup_scratch(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read case dir") {
        let path = entry.expect("entry").path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with("_tmp.txt"))
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[test]
fn differential_tree_vs_mir() {
    let cases = positive_cases();
    assert!(!cases.is_empty(), "no positive corpus cases found");
    let saved_cwd = std::env::current_dir().expect("read cwd");
    let mut failures = Vec::new();
    for case in &cases {
        let dir = case.parent().expect("parent dir").to_path_buf();
        let src = std::fs::read_to_string(case).expect("read case");
        std::env::set_current_dir(&dir).expect("set case cwd");
        // Lex+parse+check first: concurrent programs never reach the
        // sequential interpreters (routing tripwire E0215) and are
        // covered exactly by the runtime executor's deterministic
        // goldens instead.
        let (tokens, lex_errors) = lexer::lex(&src);
        assert!(lex_errors.is_empty(), "{}: lex errors", case.display());
        let (program, parse_errors) = parser::parse(tokens, &src);
        assert!(parse_errors.is_empty(), "{}: parse errors", case.display());
        let (checked, check_errors) = check::check(&program);
        assert!(
            check_errors.is_empty(),
            "{}: check errors: {check_errors:?}",
            case.display()
        );
        let checked = checked.expect("checked program");
        if checked.uses_concurrency {
            continue;
        }
        // Tree-walker via the real `toto run` pipeline (lex+parse+check+run).
        let tree = run_source(&src, "case.gzv");
        // MIR via lex+parse+check+lower+run.
        let (mir_result, mir_out) = mir_interp::run_mir(&program, &checked);
        // Compare as rendered text: exit code, stdout, panic rendering.
        let (tree_render, mir_render) = match tree {
            Outcome::Success { code, stdout } => {
                let a = format!("exit={code}\nstdout={}\n", String::from_utf8_lossy(&stdout));
                let (code2, out2) = match mir_result {
                    Ok(c) => (c, mir_out),
                    Err((d, t)) => {
                        failures.push(format!(
                            "{}: tree ok but MIR panicked:\n{}",
                            case.display(),
                            render_panic(&d, &t, &src, "case.gzv")
                        ));
                        cleanup_scratch(&dir);
                        continue;
                    }
                };
                let b = format!("exit={code2}\nstdout={}\n", String::from_utf8_lossy(&out2));
                (a, b)
            }
            Outcome::Static(diags) => {
                failures.push(format!(
                    "{}: positive case failed static checks: {diags:?}",
                    case.display()
                ));
                cleanup_scratch(&dir);
                continue;
            }
            Outcome::RuntimePanic {
                diag,
                trace,
                stdout,
            } => {
                let a = format!(
                    "panic:\n{}stdout={}\n",
                    render_panic(&diag, &trace, &src, "case.gzv"),
                    String::from_utf8_lossy(&stdout)
                );
                let b = match mir_result {
                    Ok(code) => format!("MIR exited {code} while tree panicked"),
                    Err((d, t)) => format!(
                        "panic:\n{}stdout={}\n",
                        render_panic(&d, &t, &src, "case.gzv"),
                        String::from_utf8_lossy(&mir_out)
                    ),
                };
                (a, b)
            }
        };
        if tree_render != mir_render {
            failures.push(format!(
                "--- {}\n tree:\n{tree_render}\n mir:\n{mir_render}",
                case.display()
            ));
        }
        cleanup_scratch(&dir);
    }
    std::env::set_current_dir(&saved_cwd).expect("restore cwd");
    assert!(
        failures.is_empty(),
        "{} differential case(s) diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
