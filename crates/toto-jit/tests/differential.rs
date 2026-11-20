//! Interpreter-vs-JIT differential (gate mode, status-0 sequential
//! programs): each case runs through the tree-walking interpreter
//! (in-process) and the gate JIT (subprocess isolation: a diverging
//! JIT bug fails one case, never the suite), comparing exit code and
//! stdout bytes exactly. Status-1 cases go through task mode (full
//! traces); concurrent programs need the scheduler; static failures
//! never reach an executor (check parity asserted); `diag10` is
//! interpreter-only (spec §5.9), skipped like everywhere else.

use std::path::PathBuf;

use toto_compiler::{Outcome, parse_and_check, run_source};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

#[test]
fn differential_gate_sequential() {
    let probe = env!("CARGO_BIN_EXE_jit_probe");
    let root = corpus_root();
    let mut areas: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("corpus root")
        .map(|e| e.expect("area entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut ran = 0u32;
    let mut skipped_concurrent = 0u32;
    let mut skipped_panic = 0u32;
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
            let parsed = parse_and_check(&src);
            match parsed {
                Err(_) => {
                    if status != "2" {
                        failures.push(format!("{name}: static failure but status is {status}"));
                    }
                    static_failures += 1;
                    continue;
                }
                Ok((_, checked)) => {
                    if checked.uses_concurrency {
                        skipped_concurrent += 1;
                        continue;
                    }
                    if status != "0" {
                        // Status-1 goes through task mode (full traces).
                        if status != "1" {
                            failures.push(format!("{name}: checked but status is {status}"));
                        }
                        skipped_panic += 1;
                        continue;
                    }
                    // Tree-walker (in-process, case cwd for file IO).
                    let saved_cwd = std::env::current_dir().expect("cwd");
                    std::env::set_current_dir(&dir).expect("set case cwd");
                    let interp = run_source(&src, &name);
                    std::env::set_current_dir(&saved_cwd).expect("restore cwd");
                    let (want_code, want_out) = match interp {
                        Outcome::Success { code, stdout } => (code, stdout),
                        other => {
                            failures.push(format!("{name}: interpreter failed: {other:?}"));
                            continue;
                        }
                    };
                    // Gate JIT (subprocess, case cwd).
                    let out = std::process::Command::new(probe)
                        .arg(&case)
                        .current_dir(&dir)
                        .output()
                        .expect("probe runs");
                    if out.status.code().is_none() {
                        failures.push(format!("{name}: probe killed by signal"));
                        continue;
                    }
                    let got_code = out.status.code().unwrap_or(-999);
                    if got_code != want_code {
                        failures.push(format!(
                            "{name}: exit {got_code} vs interpreter {want_code}\nstderr:\n{}",
                            String::from_utf8_lossy(&out.stderr)
                        ));
                        continue;
                    }
                    if out.stdout != want_out {
                        failures.push(format!("{name}: stdout differs"));
                        continue;
                    }
                    ran += 1;
                }
            }
        }
    }
    let _ = std::fs::remove_file(corpus_root().join("interp").join("cli01_tmp.txt"));
    // Exact census (160 cases: 42 status-0 + 4 status-1 runnable
    // (diag16_ffi_array_sig joined in Wave 1), 22 concurrent
    // (conc19_chan_eq joined in Phase 5; state06-07 joined in
    // Phase 6; conc20_net_hammer joined in Wave 1), 91 static
    // (type37_index_range + diag13-15 FFI joined in Phase 5;
    // 20 Phase-6 type_state/type_migrate + parse10-11 joined in
    // Phase 6; diag17_ffi_nonbyte_array joined in Wave 1), diag10
    // excluded).
    assert_eq!(ran, 42, "gate differential runnable count changed");
    assert_eq!(skipped_concurrent, 22, "concurrent skip count changed");
    assert_eq!(skipped_panic, 4, "status-1 skip count changed");
    assert_eq!(static_failures, 91, "static negative count changed");
    assert!(
        failures.is_empty(),
        "{} differential divergence(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
