//! Fiber-aware race detector (Phase 4, Stage 3): schedule-sweep
//! stability classification for the concurrent corpus.
//!
//! Method: run each concurrent program under many fiber schedules —
//! the ONLY varied input is the scheduler `rng_seed` (fiber
//! tie-break / spawn / steal order), at M:N worker counts 2 and 4,
//! twice per schedule — and classify each run's outcome as (status,
//! stdout bytes, panic code). Programs with one class are stable;
//! more than one class means the outcome depends on the schedule:
//! a race.
//!
//! Why fiber-aware (not thread-aware):
//! - The sweep controls the fiber schedule only. At one worker the
//!   same seed reproduces the same outcome bit-for-bit
//!   (`seed_determinism`, racy seeds included): the outcome is a
//!   function of the fiber schedule, never of thread timing. At M:N
//!   a racy program can take either legal branch run-to-run under
//!   the SAME seed (verified by probe) — that run-to-run flip IS the
//!   race, and sampling it is what the detector does.
//! - Stable programs show the SAME single class at 2 and 4 workers:
//!   task migration across workers never counts as instability.
//! - conc10 (positive control) shows both legal classes across the
//!   sweep; conc03's two classes are designed nondeterminism
//!   (select tie-break, membership-bounded), not a race.
//!
//! Sensitivity note (inherent to sampling): a race whose minority
//! branch is rarer than ~1/64 could escape this sweep. The negative
//! controls are additionally covered by the 1,620-run hammer and the
//! exact goldens, which share the same legal-outcome sets.

use std::collections::BTreeSet;
use std::path::PathBuf;

use toto_compiler::parse_and_check;
use toto_jit::{Jit, run_jit};
use toto_runtime::RuntimeConfig;

const PROGRAMS: &[&str] = &[
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

const SEEDS: std::ops::RangeInclusive<u64> = 1..=16;
const WORKERS: [usize; 2] = [2, 4];

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/concurrent")
}

fn golden(name: &str, ext: &str) -> Vec<u8> {
    std::fs::read(corpus_dir().join(format!("{name}.{ext}"))).unwrap_or_default()
}

/// Outcome class: exit status ("0"/"1", golden convention), stdout
/// bytes, panic code ("" on success).
type OutcomeClass = (String, Vec<u8>, String);

fn class_of(outcome: &toto_runtime::RtOutcome) -> OutcomeClass {
    match &outcome.result {
        Ok(code) => (code.to_string(), outcome.stdout.clone(), String::new()),
        Err((diag, _)) => (
            "1".to_string(),
            outcome.stdout.clone(),
            diag.code.to_string(),
        ),
    }
}

fn sweep(
    program: &toto_compiler::ast::Program,
    checked: &toto_compiler::hir::CheckedProgram,
) -> BTreeSet<(String, Vec<u8>, String)> {
    let prog = toto_compiler::mir::lower(checked);
    let mut jit = Jit::new_task().expect("session");
    let session = jit.compile_task(&prog).expect("compiles");
    let dir = corpus_dir();
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    let mut classes = BTreeSet::new();
    for seed in SEEDS {
        for workers in WORKERS {
            let config = RuntimeConfig {
                gc_threshold_bytes: 1,
                rng_seed: seed,
                stack_bytes: 64 * 1024,
                collect_at_every_safepoint_for_test: true,
                workers,
                blocking_pool_threads: 2,
            };
            // Twice per schedule: catches run-to-run flips under the
            // same seed at M:N (the race manifesting as timing).
            for _ in 0..2 {
                classes.insert(class_of(&run_jit(program, checked, config, &session)));
            }
        }
    }
    std::env::set_current_dir(&saved).expect("restore cwd");
    classes
}

/// The detector: positive control flags, designed nondeterminism is
/// membership-bounded, everything else is a single golden-matching
/// class across all 32 schedules.
#[test]
fn detector_flags_conc10_and_only_conc10() {
    let mut failures = Vec::new();
    // Program, classes observed (printed for the CI log).
    let mut table: Vec<(&str, Vec<OutcomeClass>)> = Vec::new();
    for name in PROGRAMS {
        let src = std::fs::read_to_string(corpus_dir().join(format!("{name}.gzv")))
            .unwrap_or_else(|_| panic!("{name}: read case"));
        let (program, checked) = parse_and_check(&src)
            .map_err(|diags| format!("{name}: static errors: {diags:?}"))
            .expect("checked");
        assert!(checked.uses_concurrency, "{name}: not concurrent");
        let classes = sweep(&program, &checked);
        table.push((name, classes.iter().cloned().collect()));
        match *name {
            "conc10_select_send" => {
                // Positive control: the schedule alone decides between
                // clean completion and the E0216 deadlock.
                let has_ok =
                    classes.contains(&("0".to_string(), b"1\n2\n".to_vec(), String::new()));
                let has_deadlock = classes
                    .iter()
                    .any(|(status, _, code)| status == "1" && code == "E0216");
                if !has_ok || !has_deadlock || classes.len() != 2 {
                    failures.push(format!(
                        "{name}: positive control did not flag as racy: {classes:?}"
                    ));
                }
            }
            "conc03_select_two" => {
                // Designed nondeterminism: the tie-break picks a ready
                // branch; both are legal, nothing else is.
                let legal: BTreeSet<(String, Vec<u8>, String)> = [
                    ("0".to_string(), b"10\n".to_vec(), String::new()),
                    ("0".to_string(), b"20\n".to_vec(), String::new()),
                ]
                .into_iter()
                .collect();
                if classes != legal {
                    failures.push(format!("{name}: tie-break classes changed: {classes:?}"));
                }
            }
            _ => {
                if classes.len() != 1 {
                    failures.push(format!(
                        "{name}: RACY: {} classes: {classes:?}",
                        classes.len()
                    ));
                    continue;
                }
                // Stable: the single class must match the golden.
                let (status, stdout, code) = classes.iter().next().unwrap().clone();
                let want_status = String::from_utf8_lossy(&golden(name, "status"))
                    .trim()
                    .to_string();
                if status != want_status {
                    failures.push(format!("{name}: status {status} vs golden {want_status}"));
                    continue;
                }
                let want_out = golden(name, "stdout");
                if stdout != want_out {
                    failures.push(format!("{name}: stdout differs from golden"));
                    continue;
                }
                if status != "0" {
                    let want_err = String::from_utf8_lossy(&golden(name, "stderr")).into_owned();
                    let want_code = want_err
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim_start_matches("panic[")
                        .split(']')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if code != want_code {
                        failures.push(format!("{name}: panic {code} vs golden {want_code}"));
                    }
                }
            }
        }
    }
    println!("program,classes");
    for (name, classes) in &table {
        println!("{name},{}", classes.len());
    }
    assert_eq!(PROGRAMS.len(), 19, "concurrent census changed");
    assert!(
        failures.is_empty(),
        "detector findings:\n{}",
        failures.join("\n")
    );
}

/// Same seed, same fiber schedule, same outcome — ten repeats per
/// seed at ONE worker, on the RACY program and on racy seeds (1 and 2
/// deadlock at M:N; the mn seed completes): with no threads to race,
/// the fiber schedule alone determines the outcome. At M:N the same
/// seed can flip run-to-run — that flip is the race, not noise.
#[test]
fn seed_determinism_single_worker() {
    let dir = corpus_dir();
    let src = std::fs::read_to_string(dir.join("conc10_select_send.gzv")).expect("read case");
    let (program, checked) = parse_and_check(&src).expect("checked");
    let prog = toto_compiler::mir::lower(&checked);
    let mut jit = Jit::new_task().expect("session");
    let session = jit.compile_task(&prog).expect("compiles");
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    for seed in [1u64, 2, 0x9E37_79B9_7F4A_7C15] {
        let config = RuntimeConfig {
            gc_threshold_bytes: 1,
            rng_seed: seed,
            stack_bytes: 64 * 1024,
            collect_at_every_safepoint_for_test: true,
            workers: 1,
            blocking_pool_threads: 2,
        };
        let first = class_of(&run_jit(&program, &checked, config, &session));
        for _ in 0..9 {
            let again = class_of(&run_jit(&program, &checked, config, &session));
            assert_eq!(again, first, "seed {seed}: same fiber schedule diverged");
        }
    }
    std::env::set_current_dir(&saved).expect("restore cwd");
}
