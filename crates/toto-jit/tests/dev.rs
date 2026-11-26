//! Stage 2 dev-driver tests: classifier matrix, settle decisions,
//! rebuild ordering / no-mixing / parked cancellation (live), the
//! parked-recv cancellation proof (executor-vs-JIT differential), and
//! watcher units + live behavior.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use toto_compiler::{parse_and_check, render_panic};
use toto_jit::dev::{
    ChangeKind, DevDriver, DrainPolicy, Generation, LiveRun, PollWatcher, RebuildOutcome,
    RestartPolicy, SettleOutcome, classify_change, settle_run,
};
use toto_jit::{Jit, OptLevel, run_jit};
use toto_runtime::sched::RunStats;
use toto_runtime::{RtOutcome, RuntimeConfig};

fn test_config(workers: usize) -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers,
        blocking_pool_threads: 2,
        // No coverage sink on dev-driver test paths (Wave 8).
        coverage: None,
    }
}

fn short_policy() -> DrainPolicy {
    DrainPolicy {
        drain_timeout: Duration::from_millis(300),
        cancel_grace: Duration::from_millis(300),
    }
}

fn checked(src: &str) -> toto_compiler::hir::CheckedProgram {
    parse_and_check(src).expect("test source checks").1
}

fn zero_stats() -> RunStats {
    RunStats {
        tasks_spawned: 0,
        switches: 0,
        steals: 0,
        high_water_max: 0,
        collections: 0,
        allocated: 0,
        reclaimed: 0,
    }
}

fn ok_outcome(stdout: &str) -> RtOutcome {
    RtOutcome {
        result: Ok(0),
        stdout: stdout.as_bytes().to_vec(),
        stats: zero_stats(),
    }
}

/// Deterministic stub run: finished after `polls` `is_finished`
/// probes (`usize::MAX` = never finishes). Records calls.
struct StubRun {
    polls_left: AtomicUsize,
    cancel_called: AtomicBool,
    /// Report finished once cancelled (models a run unwinding on
    /// scope-failure release).
    finish_on_cancel: bool,
    outcome: Mutex<Option<RtOutcome>>,
    log: Arc<Mutex<Vec<String>>>,
    name: &'static str,
}

impl StubRun {
    fn countdown(
        polls: usize,
        outcome: RtOutcome,
        log: Arc<Mutex<Vec<String>>>,
        name: &'static str,
    ) -> Self {
        Self {
            polls_left: AtomicUsize::new(polls),
            cancel_called: AtomicBool::new(false),
            finish_on_cancel: false,
            outcome: Mutex::new(Some(outcome)),
            log,
            name,
        }
    }

    fn never(log: Arc<Mutex<Vec<String>>>, name: &'static str) -> Self {
        Self {
            polls_left: AtomicUsize::new(usize::MAX),
            cancel_called: AtomicBool::new(false),
            finish_on_cancel: false,
            outcome: Mutex::new(Some(ok_outcome(""))),
            log,
            name,
        }
    }

    fn finishes_on_cancel(
        outcome: RtOutcome,
        log: Arc<Mutex<Vec<String>>>,
        name: &'static str,
    ) -> Self {
        Self {
            polls_left: AtomicUsize::new(usize::MAX),
            cancel_called: AtomicBool::new(false),
            finish_on_cancel: true,
            outcome: Mutex::new(Some(outcome)),
            log,
            name,
        }
    }
}

impl LiveRun for StubRun {
    fn is_finished(&self) -> bool {
        if self.finish_on_cancel && self.cancel_called.load(Ordering::SeqCst) {
            return true;
        }
        let p = self.polls_left.load(Ordering::SeqCst);
        if p == 0 {
            return true;
        }
        if p != usize::MAX {
            self.polls_left.fetch_sub(1, Ordering::SeqCst);
        }
        false
    }

    fn cancel(&self) {
        self.cancel_called.store(true, Ordering::SeqCst);
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:cancel", self.name));
    }

    fn join(self: Box<Self>) -> RtOutcome {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:join", self.name));
        self.outcome
            .lock()
            .expect("outcome")
            .take()
            .expect("join once")
    }
}

/// Recording restart double: shares its fired flag so tests can
/// observe the timeout-to-restart path without a real re-exec.
#[derive(Clone, Default)]
struct MockRestart {
    fired: Arc<AtomicBool>,
    reasons: Arc<Mutex<Vec<String>>>,
}

impl RestartPolicy for MockRestart {
    fn restart(&self, reason: &str) {
        self.fired.store(true, Ordering::SeqCst);
        self.reasons
            .lock()
            .expect("reasons")
            .push(reason.to_string());
    }
}

const BASE: &str = "struct P { x: int }\n\
    fn add(a: int, b: int) -> int { return a + b; }\n\
    fn unused() -> int { return 7; }\n\
    fn main() -> int { let p = P { x: 1 }; println(add(p.x, 2)); return 0; }\n";

#[test]
fn classify_matrix() {
    // Identical source: no change.
    assert_eq!(
        classify_change(&checked(BASE), &checked(BASE)),
        ChangeKind::NoChange
    );
    // Body-only: edited literal, same types and layout.
    let body = BASE.replace("add(p.x, 2)", "add(p.x, 3)");
    assert_eq!(
        classify_change(&checked(BASE), &checked(&body)),
        ChangeKind::BodyOnly
    );
    // Signature change: extra parameter.
    let sig = BASE.replace(
        "fn add(a: int, b: int) -> int { return a + b; }",
        "fn add(a: int, b: int, c: int) -> int { return a + b + c; }",
    );
    let sig = sig.replace("add(p.x, 2)", "add(p.x, 2, 0)");
    assert_eq!(
        classify_change(&checked(BASE), &checked(&sig)),
        ChangeKind::Restart
    );
    // Struct field added (the literal must provide the new field to keep checking).
    let field = BASE
        .replace("struct P { x: int }", "struct P { x: int, y: int }")
        .replace("P { x: 1 }", "P { x: 1, y: 2 }");
    assert_eq!(
        classify_change(&checked(BASE), &checked(&field)),
        ChangeKind::Restart
    );
    // Struct field removed.
    let no_field = BASE
        .replace("struct P { x: int }", "struct P { }")
        .replace("P { x: 1 }", "P { }")
        .replace("add(p.x, 2)", "add(1, 2)");
    assert_eq!(
        classify_change(&checked(BASE), &checked(&no_field)),
        ChangeKind::Restart
    );
    // Function added.
    let added = format!("{BASE}fn extra() -> int {{ return 1; }}\n");
    assert_eq!(
        classify_change(&checked(BASE), &checked(&added)),
        ChangeKind::Restart
    );
    // Function removed (unused, so it still checks).
    let removed = BASE.replace("fn unused() -> int { return 7; }\n", "");
    assert_eq!(
        classify_change(&checked(BASE), &checked(&removed)),
        ChangeKind::Restart
    );
    // Channel element type changed.
    let ch_old = "fn main() -> int { let ch = chan<int>(); close(ch); return 0; }\n";
    let ch_new = "fn main() -> int { let ch = chan<string>(); close(ch); return 0; }\n";
    assert_eq!(
        classify_change(&checked(ch_old), &checked(ch_new)),
        ChangeKind::Restart
    );
}

#[test]
fn settle_drains_natural_finish_without_cancel() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let policy = short_policy();
    let restart = MockRestart::default();
    assert!(!restart.fired.load(Ordering::SeqCst));
    // Finished after 3 polls: the drain must wait, not cancel.
    let run = Box::new(StubRun::countdown(
        3,
        ok_outcome("done\n"),
        log.clone(),
        "s",
    ));
    let (program, checked) = parse_and_check("fn main() -> int { return 0; }\n").expect("checks");
    let built = Generation::compile(
        1,
        "src".into(),
        "f.gzv".into(),
        program,
        checked,
        OptLevel::Off,
    )
    .expect("compiles");
    let (settle, ended) = settle_run(run, &built, &policy, &restart);
    assert_eq!(settle, SettleOutcome::Drained);
    let ended = ended.expect("output");
    assert_eq!(ended.code, "0");
    assert_eq!(ended.stdout, b"done\n");
    let log = log.lock().expect("log");
    assert!(log.iter().any(|e| e == "s:join"), "joined, log={log:?}");
    assert!(
        !log.iter().any(|e| e == "s:cancel"),
        "never cancelled, log={log:?}"
    );
    assert!(!restart.fired.load(Ordering::SeqCst));
}

#[test]
fn settle_cancels_after_drain_timeout() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let policy = short_policy();
    let restart = MockRestart::default();
    // Never finishes on its own, unwinds promptly once cancelled.
    let run = Box::new(StubRun::finishes_on_cancel(
        ok_outcome("late\n"),
        log.clone(),
        "s",
    ));
    let (program, checked) = parse_and_check("fn main() -> int { return 0; }\n").expect("checks");
    let built = Generation::compile(
        1,
        "src".into(),
        "f.gzv".into(),
        program,
        checked,
        OptLevel::Off,
    )
    .expect("compiles");
    let (settle, ended) = settle_run(run, &built, &policy, &restart);
    assert_eq!(settle, SettleOutcome::Cancelled);
    let ended = ended.expect("output");
    assert_eq!(ended.code, "0");
    assert_eq!(ended.stdout, b"late\n");
    let log = log.lock().expect("log");
    assert!(log.iter().any(|e| e == "s:cancel"), "log={log:?}");
    assert!(log.iter().any(|e| e == "s:join"), "log={log:?}");
    assert!(!restart.fired.load(Ordering::SeqCst));
}

#[test]
fn settle_restart_after_cancel_grace() {
    let log = Arc::new(Mutex::new(Vec::new()));
    // Zero grace: cancel lands but the run never finishes, so the
    // restart policy must fire promptly.
    let policy = DrainPolicy {
        drain_timeout: Duration::from_millis(50),
        cancel_grace: Duration::from_millis(0),
    };
    let restart = MockRestart::default();
    let fired = restart.fired.clone();
    let run = Box::new(StubRun::never(log, "s"));
    let (program, checked) = parse_and_check("fn main() -> int { return 0; }\n").expect("checks");
    let built = Generation::compile(
        1,
        "src".into(),
        "f.gzv".into(),
        program,
        checked,
        OptLevel::Off,
    )
    .expect("compiles");
    let (settle, ended) = settle_run(run, &built, &policy, &restart);
    assert_eq!(settle, SettleOutcome::Restarted);
    assert!(ended.is_none());
    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(restart.reasons.lock().expect("r").len(), 1);
}

fn mock_driver() -> DevDriver<MockRestart> {
    DevDriver::new(
        short_policy(),
        MockRestart::default(),
        test_config(1),
        OptLevel::Off,
    )
}

#[test]
fn still_broken_keeps_old_running_and_recovers() {
    let mut driver = mock_driver();
    let v1 = "fn main() -> int { println(1); return 0; }\n";
    match driver.rebuild(v1, "app.gzv") {
        RebuildOutcome::Rebuilt { ended, settle } => {
            assert!(ended.is_none());
            assert!(settle.is_none());
        }
        other => panic!("first ingest spawns, got {other:?}"),
    }
    assert_eq!(driver.current_gen(), Some(1));
    // Static breakage: old generation keeps running.
    match driver.rebuild("fn main() -> int { let x = ; }\n", "app.gzv") {
        RebuildOutcome::StillBroken(_) => {}
        other => panic!("broken source stays broken, got {other:?}"),
    }
    assert_eq!(driver.current_gen(), Some(1));
    // A fixed source rebuilds from the still-running generation.
    let v2 = "fn main() -> int { println(2); return 0; }\n";
    match driver.rebuild(v2, "app.gzv") {
        RebuildOutcome::Rebuilt { ended, settle } => {
            let ended = ended.expect("old run retired");
            assert_eq!(ended.generation, 1);
            assert_eq!(ended.stdout, b"1\n");
            assert_eq!(settle, Some(SettleOutcome::Drained));
        }
        other => panic!("fixed source rebuilds, got {other:?}"),
    }
    assert_eq!(driver.current_gen(), Some(2));
}

#[test]
fn no_change_keeps_generation_without_respawn() {
    let mut driver = mock_driver();
    let v1 = "fn main() -> int { println(1); return 0; }\n";
    assert!(matches!(
        driver.rebuild(v1, "app.gzv"),
        RebuildOutcome::Rebuilt { .. }
    ));
    assert_eq!(driver.current_gen(), Some(1));
    // Trailing comment: identical MIR, identical spans.
    let v1c = format!("{v1}// patched\n");
    assert!(matches!(
        driver.rebuild(&v1c, "app.gzv"),
        RebuildOutcome::NoChange
    ));
    assert_eq!(driver.current_gen(), Some(1));
}

#[test]
fn generations_stream_in_order_without_mixing() {
    let mut driver = mock_driver();
    let v1 = "fn main() -> int { println(\"one\"); return 0; }\n";
    let v2 = "fn main() -> int { println(\"two\"); return 0; }\n";
    let v3 = "fn main() -> int { println(\"three\"); return 0; }\n";
    assert!(matches!(
        driver.rebuild(v1, "app.gzv"),
        RebuildOutcome::Rebuilt { .. }
    ));
    // Every program returns immediately, so every settle drains with
    // no timing dependence.
    let e1 = match driver.rebuild(v2, "app.gzv") {
        RebuildOutcome::Rebuilt { ended, settle } => {
            assert_eq!(settle, Some(SettleOutcome::Drained));
            ended.expect("gen 1 output")
        }
        other => panic!("v2 rebuilds, got {other:?}"),
    };
    assert_eq!(e1.generation, 1);
    assert_eq!(e1.stdout, b"one\n");
    let e2 = match driver.rebuild(v3, "app.gzv") {
        RebuildOutcome::Rebuilt { ended, settle } => {
            assert_eq!(settle, Some(SettleOutcome::Drained));
            ended.expect("gen 2 output")
        }
        other => panic!("v3 rebuilds, got {other:?}"),
    };
    assert_eq!(e2.generation, 2);
    assert_eq!(e2.stdout, b"two\n");
    assert_eq!(e2.stderr, "");
    assert_eq!(driver.current_gen(), Some(3));
}

/// Live cancel path: generation 1 holds a task parked in `recv` while
/// main spins forever in a non-blocking `select`/`default` (each
/// iteration pre-checks scope failure, so cancel releases it within
/// microseconds). A rebuild must cancel (not hang), join, and report
/// the E0214 scope failure. NOTE: an all-parked generation can never
/// reach this path — the scheduler correctly reaps it as an E0216
/// deadlock first (verified via jit_probe `--task`); the parked task
/// here survives because the spinning main keeps the run live.
/// The child prints before parking, so stdout proves it reached `recv`.
#[test]
fn rebuild_cancels_parked_generation() {
    let mut driver = mock_driver();
    let v1 = "fn main() -> int {\n\
        \x20   let ch = chan<int>();\n\
        \x20   go {\n\
        \x20       println(\"parked\");\n\
        \x20       let v = <-ch;\n\
        \x20       println(v);\n\
        \x20   }\n\
        \x20   while true {\n\
        \x20       select {\n\
        \x20           let w = <-ch {\n\
        \x20               println(w)\n\
        \x20           }\n\
        \x20           default {\n\
        \x20           }\n\
        \x20       }\n\
        \x20   }\n\
        \x20   return 0;\n\
        }\n";
    assert!(matches!(
        driver.rebuild(v1, "app.gzv"),
        RebuildOutcome::Rebuilt { .. }
    ));
    // Spawn + park is microseconds; this margin only decides how long
    // the test waits before rebuilding, never the outcome: the run
    // cannot finish on its own (main spins forever), so settle always
    // finds it live and always cancels.
    std::thread::sleep(Duration::from_millis(300));
    // A terminating generation 2: it differs, so gen 1 settles
    // (Cancelled), then gen 2 runs to completion — no leaked spinner.
    let v2 = "fn main() -> int { println(\"patched\"); return 0; }\n";
    let (ended, settle) = match driver.rebuild(v2, "app.gzv") {
        RebuildOutcome::Rebuilt { ended, settle } => (ended, settle),
        other => panic!("v2 rebuilds, got {other:?}"),
    };
    assert_eq!(settle, Some(SettleOutcome::Cancelled));
    let ended = ended.expect("gen 1 output");
    assert_eq!(ended.generation, 1);
    assert_eq!(ended.code, "panic");
    assert_eq!(ended.stdout, b"parked\n", "child ran before cancel");
    assert!(
        ended.stderr.contains("E0214"),
        "scope failure, got:\n{}",
        ended.stderr
    );
    assert_eq!(driver.current_gen(), Some(2));
}

/// Parked-recv cancellation proof (Phase 4 exit criterion): a child
/// parked in `recv` while the scope fails must be released cleanly —
/// no hang, no deadlock, no leaked task. The victim prints before
/// parking, so workers=1 (FIFO: victim runs, parks, then the
/// signaller fires and main fails) deterministically exercises the
/// parked-release path; workers 2/4 cover the interleavings.
const PARKED_SRC: &str = "fn main() -> int {\n\
    \x20   let ch = chan<int>();\n\
    \x20   let gate = chan<int>();\n\
    \x20   go {\n\
    \x20       println(\"parked\");\n\
    \x20       let v = <-ch;\n\
    \x20       println(v);\n\
    \x20   }\n\
    \x20   go {\n\
    \x20       gate <- 1;\n\
    \x20   }\n\
    \x20   let g = <-gate;\n\
    \x20   let x = 1 / (g - g);\n\
    \x20   return x;\n\
    }\n";

fn render_outcome(
    outcome: &toto_runtime::RtOutcome,
    src: &str,
    filename: &str,
) -> (String, Vec<u8>, String) {
    match &outcome.result {
        Ok(code) => (code.to_string(), outcome.stdout.clone(), String::new()),
        Err((diag, trace)) => (
            "panic".to_string(),
            outcome.stdout.clone(),
            render_panic(diag, trace, src, filename),
        ),
    }
}

/// Reference outcome from the runtime executor (the tree-walker
/// cannot run concurrent programs): code, stdout, rendered stderr.
fn reference_outcome(
    program: &toto_compiler::ast::Program,
    checked: &toto_compiler::hir::CheckedProgram,
    workers: usize,
) -> (String, Vec<u8>, String) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/concurrent");
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    let outcome = toto_runtime::run_checked(program, checked, test_config(workers));
    std::env::set_current_dir(&saved).expect("restore cwd");
    render_outcome(&outcome, PARKED_SRC, "parked.gzv")
}

/// workers=1 pins the parked path (victim runs and parks before the
/// signaller fires and main fails), so the victim provably parks
/// before main fails: stdout proves it ran, and the golden proves it
/// was released. Asserts JIT == executor exactly, plus the E0202
/// (never E0216) requirement on both.
#[test]
fn parked_recv_cancelled_on_scope_failure() {
    let (program, checked) = parse_and_check(PARKED_SRC).expect("checks");
    let prog = toto_compiler::mir::lower(&checked);
    let mut jit = Jit::new_task().expect("session");
    let session = jit.compile_task(&prog).expect("compiles");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/concurrent");
    // Deterministic single-worker golden, both executors.
    let (want_code, want_out, want_err) = reference_outcome(&program, &checked, 1);
    assert_eq!(want_code, "panic");
    assert_eq!(want_out, b"parked\n", "victim ran before main failed");
    assert!(want_err.contains("E0202"), "div-zero, got:\n{want_err}");
    assert!(!want_err.contains("E0216"), "no deadlock, got:\n{want_err}");
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    let got = run_jit(&program, &checked, test_config(1), &session);
    std::env::set_current_dir(&saved).expect("restore cwd");
    let (code, out, err) = render_outcome(&got, PARKED_SRC, "parked.gzv");
    assert_eq!((code, out, err), (want_code, want_out, want_err.clone()));
    // M:N interleavings: the victim may or may not print before the
    // failure, but the outcome class and trace are invariant — no
    // hang, no deadlock, no leaked task, on every run.
    for workers in [2usize, 4] {
        for _ in 0..30 {
            let saved = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(&dir).expect("set case cwd");
            let got = run_jit(&program, &checked, test_config(workers), &session);
            std::env::set_current_dir(&saved).expect("restore cwd");
            let (code, out, err) = render_outcome(&got, PARKED_SRC, "parked.gzv");
            assert_eq!(code, "panic", "workers={workers}");
            assert!(
                out == b"parked\n" || out.is_empty(),
                "workers={workers}, out={out:?}"
            );
            assert_eq!(err, want_err, "workers={workers}");
        }
    }
}

fn unique_tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("toto-dev-test-{}-{}-{name}", std::process::id(), n))
}

#[test]
fn watcher_ignores_untouched_saves() {
    let dir = unique_tmp("same");
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let file = dir.join("app.gzv");
    std::fs::write(&file, "v1\n").expect("write");
    let mut w = PollWatcher::new(file.clone(), Duration::from_millis(10)).expect("watch");
    assert!(w.poll_once().expect("poll").is_none());
    // Same bytes, new mtime: not a change.
    std::fs::write(&file, "v1\n").expect("rewrite");
    assert!(w.poll_once().expect("poll").is_none());
    // Different bytes: a change.
    std::fs::write(&file, "v2\n").expect("rewrite");
    assert_eq!(
        w.poll_once().expect("poll").as_deref(),
        Some(b"v2\n".as_slice())
    );
    // Consumed: quiet again.
    assert!(w.poll_once().expect("poll").is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn watcher_live_change_and_stop() {
    use std::sync::atomic::AtomicBool;
    let dir = unique_tmp("live");
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let file = dir.join("app.gzv");
    std::fs::write(&file, "v1\n").expect("write");
    let mut w = PollWatcher::new(file.clone(), Duration::from_millis(10)).expect("watch");
    let stop = AtomicBool::new(false);
    // A change from another thread is observed.
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(&file, "v2\n").expect("write");
    });
    let got = w.wait_change(&stop);
    writer.join().expect("writer");
    assert_eq!(got.as_deref(), Some(b"v2\n".as_slice()));
    // A fired stop flag returns promptly without a change.
    let stop2 = AtomicBool::new(true);
    assert!(w.wait_change(&stop2).is_none());
    std::fs::remove_dir_all(&dir).ok();
}
