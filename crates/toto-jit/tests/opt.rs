//! Optimization gate (Phase 4, Stage 3): behavior is identical at
//! every `OptLevel`. For each runnable corpus program — gate
//! sequential via the `jit_probe` subprocess, task sequential and
//! concurrent via in-process `run_jit` — the outcome at `speed` and
//! `speed_and_size` must equal the outcome at `off` exactly (exit
//! code, stdout, rendered stderr). Off-correctness is owned by the
//! differential suites running in this same gate; this file owns
//! opt-invariance only. Tier-1 (`mir_conform`) needs no opt re-run:
//! it checks MIR shape, which a backend emission flag cannot alter.

use std::path::PathBuf;

use toto_compiler::{parse_and_check, render_panic};
use toto_jit::{Jit, OptLevel, run_jit};
use toto_runtime::RuntimeConfig;

const LEVELS: [OptLevel; 2] = [OptLevel::Speed, OptLevel::SpeedAndSize];

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

/// Gate-mode outcome of one probe run: exit code, stdout, stderr.
fn probe_out(probe: &str, case: &std::path::Path, level: OptLevel) -> (i32, Vec<u8>, Vec<u8>) {
    let mut cmd = std::process::Command::new(probe);
    if level != OptLevel::Off {
        let flag = match level {
            OptLevel::Speed => "speed",
            OptLevel::SpeedAndSize => "speed_and_size",
            OptLevel::Off => unreachable!(),
        };
        cmd.arg(format!("--opt-level={flag}"));
    }
    let out = cmd
        .arg(case)
        .current_dir(case.parent().expect("parent"))
        .output()
        .expect("probe runs");
    (out.status.code().unwrap_or(-999), out.stdout, out.stderr)
}

#[test]
fn opt_gate_sequential_equivalent() {
    let probe = env!("CARGO_BIN_EXE_jit_probe");
    let root = corpus_root();
    let mut areas: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("corpus root")
        .map(|e| e.expect("area entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut ran_ok = 0u32;
    let mut ran_panic = 0u32;
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
            let parsed = parse_and_check(&src);
            match parsed {
                Err(_) => {
                    if status != "2" {
                        failures.push(format!("{name}: static failure but status is {status}"));
                    }
                    continue;
                }
                Ok((_, checked)) => {
                    if checked.uses_concurrency {
                        continue;
                    }
                    if status != "0" && status != "1" {
                        failures.push(format!("{name}: checked but status is {status}"));
                        continue;
                    }
                }
            }
            let base = probe_out(probe, &case, OptLevel::Off);
            for level in LEVELS {
                let got = probe_out(probe, &case, level);
                if got != base {
                    failures.push(format!(
                        "{name}@{level:?}: outcome differs from off\n off: {:?}\n got: {:?}",
                        summarize(&base),
                        summarize(&got),
                    ));
                }
            }
            if status == "0" {
                ran_ok += 1;
            } else {
                ran_panic += 1;
            }
        }
    }
    // Exact census: the 42 status-0 (state01-05 joined in Phase 6;
    // diag16_ffi_array_sig joined in Wave 1) plus the 4 status-1
    // runnable sequential programs, same as the gate differential.
    assert_eq!(ran_ok, 42, "gate opt coverage (status-0) changed");
    assert_eq!(ran_panic, 4, "gate opt coverage (status-1) changed");
    // cli01 exercises file IO in its case directory; mirror the
    // differential's cleanup so no tmp file escapes the suite.
    let _ = std::fs::remove_file(corpus_root().join("interp").join("cli01_tmp.txt"));
    assert!(
        failures.is_empty(),
        "opt divergences:\n{}",
        failures.join("\n")
    );
}

fn summarize(out: &(i32, Vec<u8>, Vec<u8>)) -> (i32, usize, usize) {
    (out.0, out.1.len(), out.2.len())
}

fn pressure_config() -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 1,
        blocking_pool_threads: 2,
        // No coverage sink on opt test paths (Wave 8).
        coverage: None,
    }
}

/// In-process outcome rendering: code class, stdout, rendered stderr.
fn render(outcome: &toto_runtime::RtOutcome, src: &str, name: &str) -> (String, Vec<u8>, String) {
    match &outcome.result {
        Ok(code) => (code.to_string(), outcome.stdout.clone(), String::new()),
        Err((diag, trace)) => (
            "panic".to_string(),
            outcome.stdout.clone(),
            render_panic(diag, trace, src, name),
        ),
    }
}

fn jit_outcome(
    program: &toto_compiler::ast::Program,
    checked: &toto_compiler::hir::CheckedProgram,
    level: OptLevel,
) -> toto_runtime::RtOutcome {
    let mut jit = Jit::new_task_with_opt(level).expect("session");
    let prog = toto_compiler::mir::lower(checked);
    let session = jit.compile_task(&prog).expect("compiles");
    // No chdir: mirrors differential_task exactly (sequential cases
    // resolve file IO the same way there).
    run_jit(program, checked, pressure_config(), &session)
}

#[test]
fn opt_task_sequential_equivalent() {
    let root = corpus_root();
    let mut areas: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("corpus root")
        .map(|e| e.expect("area entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut ran = 0u32;
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
            let (program, checked) = match parse_and_check(&src) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if checked.uses_concurrency {
                continue;
            }
            // Interpreter parity at off is the differential's job;
            // here, every level must match off exactly.
            let base = render(&jit_outcome(&program, &checked, OptLevel::Off), &src, &name);
            for level in LEVELS {
                let got = render(&jit_outcome(&program, &checked, level), &src, &name);
                if got != base {
                    failures.push(format!("{name}@{level:?}: outcome differs from off"));
                }
            }
            ran += 1;
        }
    }
    // 42 status-0 + 4 status-1, same census as the task differential
    // (diag16_ffi_array_sig joined in Wave 1).
    assert_eq!(ran, 46, "task opt coverage changed");
    assert!(
        failures.is_empty(),
        "opt divergences:\n{}",
        failures.join("\n")
    );
}

const CONCURRENT: &[&str] = &[
    "conc01_pingpong",
    "conc02_buffered_close",
    "conc03_select_two",
    "conc04_select_default",
    "conc05_moved_struct",
    "conc06_fanout",
    "conc07_zero_values",
    "conc08_go_early_return",
    "conc09_nested_go",
    "conc10_select_send",
    "conc11_chan_of_chan",
    "conc18_fneq",
    "conc19_chan_eq",
    "conc_neg_child_panic",
    "conc_neg_deadlock",
    "conc_neg_double_close",
    "conc_neg_main_panic",
    "conc_neg_select_send_closed",
    "conc_neg_send_closed",
];

/// Outcome class: exit-code class, stdout bytes, panic code (if any).
fn class_of(outcome: &toto_runtime::RtOutcome) -> (String, Vec<u8>, String) {
    match &outcome.result {
        Ok(code) => (code.to_string(), outcome.stdout.clone(), String::new()),
        Err((diag, _)) => (
            "panic".to_string(),
            outcome.stdout.clone(),
            diag.code.to_string(),
        ),
    }
}

/// Legal outcome classes for the two racy programs (membership, not
/// equivalence: at M:N even the same seed can take either branch run
/// to run — verified by probe; both branches are semantically legal).
fn legal_racy(name: &str, class: &(String, Vec<u8>, String)) -> bool {
    match name {
        "conc03_select_two" => {
            (class.0 == "0" && class.1 == b"10\n") || (class.0 == "0" && class.1 == b"20\n")
        }
        "conc10_select_send" => {
            (class.0 == "0" && class.1 == b"1\n2\n" && class.2.is_empty())
                || (class.0 == "panic" && class.2 == "E0216")
        }
        _ => false,
    }
}

#[test]
fn opt_concurrent_equivalent() {
    let dir = corpus_root().join("concurrent");
    let mut failures = Vec::new();
    for name in CONCURRENT {
        let case = dir.join(format!("{name}.gzv"));
        let src = std::fs::read_to_string(&case).expect("read case");
        let (program, checked) = parse_and_check(&src)
            .map_err(|diags| format!("{name}: static errors: {diags:?}"))
            .expect("checked");
        assert!(checked.uses_concurrency, "{name}: not concurrent");
        // One session per level (compiled once, run under every
        // seed/worker below).
        let mut sessions = Vec::new();
        for level in [OptLevel::Off, OptLevel::Speed, OptLevel::SpeedAndSize] {
            let mut jit = Jit::new_task_with_opt(level).expect("session");
            let prog = toto_compiler::mir::lower(&checked);
            let session = jit.compile_task(&prog).expect("compiles");
            sessions.push((level, jit, session));
        }
        let racy = *name == "conc03_select_two" || *name == "conc10_select_send";
        let saved = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&dir).expect("set case cwd");
        for seed in 1u64..=4 {
            for workers in [2usize, 4] {
                let config = RuntimeConfig {
                    workers,
                    ..pressure_config()
                };
                let config = RuntimeConfig {
                    rng_seed: seed,
                    ..config
                };
                let base = class_of(&run_jit(&program, &checked, config.clone(), &sessions[0].2));
                if racy && !legal_racy(name, &base) {
                    failures.push(format!(
                        "{name}@off seed={seed} workers={workers}: illegal class"
                    ));
                }
                for (level, _, session) in sessions.iter().skip(1) {
                    let got = class_of(&run_jit(&program, &checked, config.clone(), session));
                    if racy {
                        if !legal_racy(name, &got) {
                            failures.push(format!(
                                "{name}@{level:?} seed={seed} workers={workers}: illegal class"
                            ));
                        }
                    } else if got != base {
                        failures.push(format!(
                            "{name}@{level:?} seed={seed} workers={workers}: class differs from off"
                        ));
                    }
                }
            }
        }
        std::env::set_current_dir(&saved).expect("restore cwd");
    }
    assert!(
        failures.is_empty(),
        "opt divergences:\n{}",
        failures.join("\n")
    );
}
