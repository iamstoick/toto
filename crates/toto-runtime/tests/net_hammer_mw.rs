//! Multi-worker poller proof (Wave 1 fix-forward): the `conc20`
//! hammer must complete at 2, 3, and 4 workers across seeds —
//! completion itself is the no-lost-wakeup / no-starvation
//! evidence. A watchdog converts a stall into a loud failure
//! naming the (workers, seed) cell instead of hanging the suite.
//!
//! The golden is exact even multi-worker: the hammer prints one
//! line of pure arithmetic. A stack-budget assertion guards the
//! ARM fix: no task may touch the guard page on any cell.

use std::path::{Path, PathBuf};
use std::time::Duration;

use toto_compiler::parse_and_check;
use toto_runtime::{RtOutcome, RuntimeConfig, run_checked};

fn case_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
        .join("concurrent")
        .join("conc20_net_hammer.gzv")
}

fn run_cell(workers: usize, seed: u64) -> RtOutcome {
    let case = case_path();
    let src = std::fs::read_to_string(&case).expect("read hammer");
    let (program, checked) = parse_and_check(&src)
        .map_err(|diags| format!("static errors: {diags:?}"))
        .expect("checked");
    assert!(checked.uses_concurrency, "hammer is concurrent");
    let config = RuntimeConfig {
        gc_threshold_bytes: 1 << 20,
        rng_seed: seed,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: false,
        workers,
        blocking_pool_threads: 2,
    };
    // The watchdog: a lost wakeup parks every worker forever, so an
    // unbounded join would hang the suite. Timeout converts the
    // stall into a failure naming the cell. (A timed-out run's
    // thread is detached; the process exits normally at test end.)
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = run_checked(&program, &checked, config);
        let _ = tx.send(outcome);
    });
    rx.recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| {
            panic!("net hammer stalled: workers={workers} seed={seed} (lost wakeup?)")
        })
}

fn check_cell(workers: usize, seed: u64) {
    let outcome = run_cell(workers, seed);
    assert!(
        outcome.result.is_ok(),
        "workers={workers} seed={seed}: {:?}",
        outcome.result
    );
    assert_eq!(
        outcome.stdout, b"Ok(HAMMER-OK clients=4 rounds=25 bytes=1600 served=1600)\n",
        "workers={workers} seed={seed}: stdout"
    );
    assert!(
        outcome.stats.tasks_spawned >= 9,
        "workers={workers} seed={seed}: too few tasks"
    );
    // Spec-fixed 64 KiB stacks (ADR-002/ADR-008): the high-water
    // must stay strictly inside the budget on every cell. The ARM
    // fix measured 60,584 bytes on x86_64; this bound (one guard
    // page of margin) catches any regression on any platform.
    assert!(
        outcome.stats.high_water_max < 64 * 1024,
        "workers={workers} seed={seed}: stack high-water {} overflows 64 KiB",
        outcome.stats.high_water_max
    );
}

// One test, all cells serial: the hammer binds fixed port 48220,
// so parallel cells would collide on bind (EADDRINUSE) even when
// the scheduler is correct. 18 cells (workers 2/3/4 x seeds
// 42..47); any stall fails loudly naming its cell.
#[test]
fn hammer_multi_worker_no_lost_wakeup() {
    for workers in [2, 3, 4] {
        for seed in 42..=47 {
            check_cell(workers, seed);
        }
    }
}
