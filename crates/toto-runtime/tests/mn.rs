//! Concurrent corpus under the M:N scheduler (Stage 2): the same 19
//! programs (conc19_chan_eq joined in Phase 5) run with 4 workers and the forced-collection hook (every
//! safepoint rendezvouses all workers — maximum barrier pressure).
//!
//! Interleavings are genuinely nondeterministic under M:N, so:
//! - race-free single-printer programs assert EXACT goldens;
//! - `conc03_select_two` (seeded RNG tie-break) asserts membership in
//!   the two legal outputs;
//! - `conc10_select_send` has a latent program race (if the select
//!   takes `b` first, the trailing `<-b` deadlocks): the outcome must
//!   be one of the two legal ones — exact output or exact E0216;
//! - negatives assert exact status/stdout/stderr goldens.
//!
//! A single `#[test]` per class: cases chdir into their directory, and
//! parallel test threads in one binary must not race the
//! process-global cwd.

use std::path::{Path, PathBuf};

use toto_compiler::{parse_and_check, render_panic};
use toto_runtime::{RuntimeConfig, run_checked};

fn concurrent_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
        .join("concurrent")
}

fn mn_config() -> RuntimeConfig {
    RuntimeConfig {
        // Forced collection at every safepoint on 4 workers: the
        // rendezvous barrier is exercised on every program.
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 4,
        blocking_pool_threads: 2,
        // No coverage sink on multi-worker paths (Wave 8).
        coverage: None,
    }
}

struct CaseOut {
    status: String,
    stdout: Vec<u8>,
    stderr: String,
}

fn run_case(name: &str) -> CaseOut {
    let case = concurrent_dir().join(format!("{name}.gzv"));
    let src = std::fs::read_to_string(&case).expect("read case");
    let dir = case.parent().expect("parent").to_path_buf();
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    let parsed = parse_and_check(&src);
    std::env::set_current_dir(&saved).expect("restore cwd");
    let (program, checked) = parsed
        .map_err(|diags| format!("{name}: static errors: {diags:?}"))
        .expect("checked");
    assert!(checked.uses_concurrency, "{name}: not concurrent");
    let outcome = run_checked(&program, &checked, mn_config());
    assert!(outcome.stats.tasks_spawned >= 1, "{name}: no tasks spawned");
    assert!(
        outcome.stats.collections >= 1,
        "{name}: no collections under M:N pressure"
    );
    let (status, stderr) = match &outcome.result {
        Ok(_) => ("0".to_string(), String::new()),
        Err((diag, trace)) => (
            "1".to_string(),
            render_panic(diag, trace, &src, &format!("{name}.gzv")),
        ),
    };
    CaseOut {
        status,
        stdout: outcome.stdout,
        stderr,
    }
}

fn golden(name: &str, ext: &str) -> Vec<u8> {
    std::fs::read(concurrent_dir().join(format!("{name}.{ext}"))).unwrap_or_default()
}

fn golden_str(name: &str, ext: &str) -> String {
    String::from_utf8_lossy(&golden(name, ext)).into_owned()
}

#[test]
fn mn_exact_race_free_programs() {
    // Single-printer, no schedule-sensitive choices: byte-identical
    // under any interleaving.
    for name in [
        "conc01_pingpong",
        "conc02_buffered_close",
        "conc04_select_default",
        "conc05_moved_struct",
        "conc06_fanout",
        "conc07_zero_values",
        "conc08_go_early_return",
        "conc09_nested_go",
        "conc11_chan_of_chan",
        "conc19_chan_eq",
    ] {
        let out = run_case(name);
        assert_eq!(
            out.status,
            golden_str(name, "status").trim(),
            "{name}: status"
        );
        assert_eq!(out.stdout, golden(name, "stdout"), "{name}: stdout");
        if out.status == "0" {
            assert!(
                golden(name, "stderr").is_empty(),
                "{name}: unexpected stderr golden"
            );
        } else {
            assert_eq!(out.stderr, golden_str(name, "stderr"), "{name}: stderr");
        }
    }
}

#[test]
fn mn_select_tiebreak_membership() {
    // Both cases ready: the seeded RNG breaks the tie. Under M:N the
    // shared RNG is drawn in schedule order, so either value is legal.
    let out = run_case("conc03_select_two");
    assert_eq!(out.status, "0");
    assert!(
        out.stdout == b"10\n".to_vec() || out.stdout == b"20\n".to_vec(),
        "conc03: illegal output {:?}",
        out.stdout
    );
}

#[test]
fn mn_racy_select_legal_outcomes() {
    // Genuine program race (not a scheduler bug): if the select takes
    // `b` first, the trailing `<-b` has no sender left and the run
    // deadlocks with E0216. Either outcome is legal.
    let out = run_case("conc10_select_send");
    if out.status == "0" {
        assert_eq!(out.stdout, b"1\n2\n".to_vec());
    } else {
        assert_eq!(out.status, "1");
        assert!(out.stdout.is_empty());
        assert!(
            out.stderr.contains("panic[E0216]"),
            "conc10: non-deadlock failure is illegal: {}",
            out.stderr
        );
    }
}

#[test]
fn mn_stress_unbuffered_pairing() {
    // Pairing hammer for the M:N lost-wakeup races (both directions):
    // 8 senders rendezvous with one receiver on a single unbuffered
    // channel, 40 rounds. Every round must observe the exact sum
    // 0+..+7 = 28 regardless of interleaving; any lost pairing strands
    // a task and the run deadlocks (E0216), so status 0 plus the `ok`
    // marker is the invariant. Runs at 1/2/4/8 workers, plus one pass
    // with a collection at every safepoint (maximum rendezvous-barrier
    // pressure on the pairing paths).
    let src = r#"fn main() -> int {
    let ch = chan<int>();
    let mut round = 0;
    while round < 40 {
        for k in 0..8 {
            go {
                ch <- k
            }
        }
        let mut got = 0;
        let mut n = 0;
        while n < 8 {
            got = got + <-ch;
            n = n + 1;
        }
        if got != 28 {
            println("MISMATCH");
            return 1;
        }
        round = round + 1;
    }
    println("ok");
    return 0;
}"#;
    let (program, checked) = parse_and_check(src).expect("stress program checks");
    assert!(checked.uses_concurrency, "stress program is concurrent");
    for workers in [1usize, 2, 4, 8] {
        let cfg = RuntimeConfig {
            workers,
            ..mn_config()
        };
        // Default GC threshold here (not every safepoint): this test
        // hammers pairing interleavings, not the barrier.
        let cfg = RuntimeConfig {
            collect_at_every_safepoint_for_test: false,
            ..cfg
        };
        let outcome = run_checked(&program, &checked, cfg);
        assert!(
            outcome.result.is_ok(),
            "stress pairing w={workers}: {}",
            outcome
                .result
                .as_ref()
                .err()
                .map(|(d, _)| format!("{} {}", d.code, d.message))
                .unwrap_or_default()
        );
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "ok\n",
            "stress pairing w={workers}: stdout"
        );
    }
    let outcome = run_checked(&program, &checked, mn_config());
    assert!(
        outcome.result.is_ok(),
        "stress pairing w=4 forced-gc: {}",
        outcome
            .result
            .as_ref()
            .err()
            .map(|(d, _)| format!("{} {}", d.code, d.message))
            .unwrap_or_default()
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout),
        "ok\n",
        "stress pairing w=4 forced-gc: stdout"
    );
}

#[test]
fn mn_negatives_exact() {
    for name in [
        "conc_neg_send_closed",
        "conc_neg_double_close",
        "conc_neg_deadlock",
        "conc_neg_select_send_closed",
        "conc_neg_child_panic",
        "conc_neg_main_panic",
        "conc18_fneq",
    ] {
        let out = run_case(name);
        assert_eq!(out.status, "1", "{name}: status");
        assert_eq!(out.stdout, golden(name, "stdout"), "{name}: stdout");
        assert_eq!(out.stderr, golden_str(name, "stderr"), "{name}: stderr");
    }
}
