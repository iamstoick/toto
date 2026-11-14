//! Concurrent corpus through the runtime executor with GC pressure:
//! every `tests/corpus/concurrent/*` program runs under a tiny
//! allocation threshold (collect at almost every safepoint) with the
//! fixed deterministic seed, and must match its golden exactly.
//!
//! This is the end-to-end milestone: green threads + channels + heap
//! allocation + forced collection; live references survive, unreachable
//! objects are reclaimed; deterministic; exact goldens.

use std::path::PathBuf;

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

fn pressure_config() -> RuntimeConfig {
    RuntimeConfig {
        // TEST-ONLY forced collection: every allocation and switch
        // safepoint runs the collector in these tests, so even
        // allocation-free programs prove the GC machinery ran.
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        // Deterministic single-worker mode: exact goldens.
        workers: 1,
        blocking_pool_threads: 2,
    }
}

use std::path::Path;

fn run_case(name: &str) -> (String, Vec<u8>, Vec<u8>) {
    let case = concurrent_dir().join(format!("{name}.gzv"));
    let src = std::fs::read_to_string(&case).expect("read case");
    let dir = case.parent().expect("parent").to_path_buf();
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    let (program, checked) = parse_and_check(&src)
        .map_err(|diags| format!("{name}: static errors: {diags:?}"))
        .expect("checked");
    assert!(checked.uses_concurrency, "{name}: not concurrent");
    let outcome = run_checked(&program, &checked, pressure_config());
    std::env::set_current_dir(&saved).expect("restore cwd");
    let status = std::fs::read_to_string(concurrent_dir().join(format!("{name}.status")))
        .unwrap_or_default()
        .trim()
        .to_string();
    let want_out =
        std::fs::read(concurrent_dir().join(format!("{name}.stdout"))).unwrap_or_default();
    let want_err_raw =
        std::fs::read(concurrent_dir().join(format!("{name}.stderr"))).unwrap_or_default();
    let want_err = String::from_utf8_lossy(&want_err_raw).into_owned();
    let got_status = match &outcome.result {
        Ok(_) => "0".to_string(),
        Err(_) => "1".to_string(),
    };
    assert_eq!(got_status, status, "{name}: status");
    assert_eq!(outcome.stdout, want_out, "{name}: stdout");
    match &outcome.result {
        Ok(_) => assert!(want_err.is_empty(), "{name}: unexpected stderr golden"),
        Err((diag, trace)) => {
            let got_err = render_panic(diag, trace, &src, &format!("{name}.gzv"));
            assert_eq!(got_err, want_err, "{name}: stderr");
        }
    }
    // Stats prove the milestone ran: tasks spawned and collections done.
    assert!(outcome.stats.tasks_spawned >= 1, "{name}: no tasks spawned");
    assert!(
        outcome.stats.collections >= 1,
        "{name}: no collections under pressure"
    );
    (status, outcome.stdout, want_err_raw)
}

#[test]
fn concurrent_positives_exact_under_gc_pressure() {
    for name in [
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
    ] {
        run_case(name);
    }
}

#[test]
fn concurrent_negatives_exact_under_gc_pressure() {
    for name in [
        "conc_neg_send_closed",
        "conc_neg_double_close",
        "conc_neg_deadlock",
        "conc_neg_select_send_closed",
        "conc_neg_child_panic",
        "conc_neg_main_panic",
    ] {
        run_case(name);
    }
}

#[test]
fn concurrent_deterministic_across_runs() {
    // Same seed, same program, two full runs: byte-identical results.
    for name in ["conc03_select_two", "conc06_fanout", "conc10_select_send"] {
        let (s1, o1, e1) = run_case(name);
        let (s2, o2, e2) = run_case(name);
        assert_eq!((s1, o1, e1), (s2, o2, e2), "{name}: nondeterministic");
    }
}
