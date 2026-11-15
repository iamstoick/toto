//! Interpreter-vs-JIT differential (task mode, sequential programs):
//! every runnable case runs through the tree-walking interpreter and
//! the scheduler-driven JIT (default config AND forced-GC pressure),
//! comparing exit code, stdout bytes, and fully rendered panics
//! (traces included) exactly. Concurrent programs have their own
//! harness (goldens + determinism); static failures never reach an
//! executor (check parity asserted); `diag10` is interpreter-only
//! (spec §5.9), skipped like everywhere else.

use std::path::PathBuf;

use toto_compiler::ast::Program;
use toto_compiler::hir::CheckedProgram;
use toto_compiler::{Outcome, parse_and_check, render_panic, run_source};
use toto_jit::{Jit, run_jit};
use toto_runtime::{RtOutcome, RuntimeConfig};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn jit_outcome(
    program: &Program,
    checked: &CheckedProgram,
    force_gc: bool,
) -> Result<RtOutcome, String> {
    let mut jit = Jit::new_task().map_err(|e| e.to_string())?;
    let prog = toto_compiler::mir::lower(checked);
    let session = jit.compile_task(&prog).map_err(|e| e.to_string())?;
    let mut config = RuntimeConfig::default();
    if force_gc {
        config.gc_threshold_bytes = 1;
        config.collect_at_every_safepoint_for_test = true;
    }
    Ok(run_jit(program, checked, config, &session))
}

fn class_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Success { .. } => "ok",
        Outcome::RuntimePanic { .. } => "panic",
        Outcome::Static(_) => "static",
    }
}

fn compare(
    name: &str,
    tag: &str,
    src: &str,
    interp: &Outcome,
    jit: &RtOutcome,
    failures: &mut Vec<String>,
) {
    match (interp, &jit.result) {
        (Outcome::Success { code: a, stdout: oa }, Ok(b)) => {
            if a != b {
                failures.push(format!("{name}{tag}: exit {b} vs interpreter {a}"));
            } else if oa != &jit.stdout {
                failures.push(format!("{name}{tag}: stdout differs"));
            }
        }
        (
            Outcome::RuntimePanic {
                diag: ad,
                trace: at,
                stdout: oa,
            },
            Err((bd, bt)),
        ) => {
            let want = render_panic(ad, at, src, name);
            let got = render_panic(bd, bt, src, name);
            if want != got {
                failures.push(format!(
                    "{name}{tag}: stderr differs:\n got:\n{got}\n want:\n{want}"
                ));
            }
            if oa != &jit.stdout {
                failures.push(format!("{name}{tag}: panic-stdout differs"));
            }
        }
        (a, b) => failures.push(format!(
            "{name}{tag}: outcome class differs (interp {}, jit {})",
            class_name(a),
            if b.is_ok() { "ok" } else { "panic" }
        )),
    }
}

#[test]
fn differential_task_sequential() {
    let root = corpus_root();
    let mut areas: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("corpus root")
        .map(|e| e.expect("area entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut ran = 0u32;
    let mut skipped_concurrent = 0u32;
    let mut static_failures = 0u32;
    let mut failures = Vec::new();
    for area in areas {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&area)
            .expect("area dir")
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
            if name == "diag10_recursion_limit.gzv" {
                continue;
            }
            let src = std::fs::read_to_string(&case).expect("read case");
            let status = std::fs::read_to_string(case.with_extension("status"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let dir = case.parent().expect("parent").to_path_buf();
            let saved_cwd = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(&dir).expect("set case cwd");
            let parsed = parse_and_check(&src);
            let interp = parsed
                .as_ref()
                .ok()
                .map(|_| run_source(&src, &name));
            std::env::set_current_dir(&saved_cwd).expect("restore cwd");
            match parsed {
                Err(_) => {
                    if status != "2" {
                        failures.push(format!("{name}: static failure but status is {status}"));
                    }
                    static_failures += 1;
                    continue;
                }
                Ok((program, checked)) => {
                    if checked.uses_concurrency {
                        skipped_concurrent += 1;
                        continue;
                    }
                    if status != "0" && status != "1" {
                        failures.push(format!("{name}: checked but status is {status}"));
                        continue;
                    }
                    let interp = interp.expect("interpreter ran on checked code");
                    for (force_gc, tag) in [(false, ""), (true, " [forced GC]")] {
                        match jit_outcome(&program, &checked, force_gc) {
                            Ok(jit) => compare(&name, tag, &src, &interp, &jit, &mut failures),
                            Err(e) => failures.push(format!("{name}{tag}: task JIT refused: {e}")),
                        }
                    }
                    ran += 1;
                }
            }
        }
    }
    let _ = std::fs::remove_file(corpus_root().join("interp").join("cli01_tmp.txt"));
    // Exact census (123 cases: 40 runnable + 18 concurrent + 64
    // static + diag10 excluded).
    assert_eq!(ran, 40, "task differential runnable count changed");
    assert_eq!(skipped_concurrent, 18, "concurrent skip count changed");
    assert_eq!(static_failures, 64, "static negative count changed");
    assert!(
        failures.is_empty(),
        "{} differential divergence(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
