//! Wave 4 Stage 3 green-thread dumps: E0216 auto-dump and
//! SIGUSR2-equivalent on-demand dump (via the flag path), asserted
//! against real in-process wedges. Deterministic by construction
//! (fixed task ids/names/chan indices on one worker; stacks are
//! function names, never addresses).
//!
//! Mirrors `tests/corpus/project/proj27_dbg` (same wedge shape;
//! the corpus golden pins the E0216 diagnostic, this file pins
//! the dump content).

use toto_compiler::project;
use toto_runtime::{run_checked, RuntimeConfig};

const WEDGE: &str = r#"fn main() -> int {
    let a = chan<int>(0);
    let b = chan<int>(0);
    go {
        let x = <-a;
        b <- x;
    }
    go {
        let y = <-b;
        a <- y;
    }
    return 0;
}
"#;

fn dump_path(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!(
        "toto-dbg-{}-{}-{}.txt",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    p.display().to_string()
}

fn lines(path: &str) -> Vec<String> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|_| panic!("dump missing: {path}"));
    text.lines().map(str::to_string).collect()
}

#[test]
fn e0216_wedge_auto_dumps_every_task() {
    let out = dump_path("auto");
    // SAFETY: only test in this binary touches these vars.
    unsafe {
        std::env::set_var("TOTO_THREAD_DUMP", &out);
    }
    let (project, checked) = project::load_and_check(
        "main.gzv",
        WEDGE,
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("wedge program checks");
    assert!(checked.uses_concurrency, "wedge must park");
    let outcome = run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    );
    // E0216: the run fails, and the dump lands anyway.
    match &outcome.result {
        Ok(code) => panic!("wedge completed cleanly (exit {code})"),
        Err((diag, _)) => assert_eq!(diag.code, "E0216", "code"),
    }
    let ls = lines(&out);
    assert_eq!(ls.first().map(String::as_str), Some("threaddump v1"), "header");
    // Both wedged tasks, named, with their chan waits.
    assert!(
        ls.iter().any(|l| l.starts_with("task 1 go-1 blocked chan0 recv")),
        "task 1: {ls:?}"
    );
    assert!(
        ls.iter().any(|l| l.starts_with("task 2 go-2 blocked chan1 recv")),
        "task 2: {ls:?}"
    );
    // Channels name their waiters (the post-mortem payload).
    assert!(
        ls.iter()
            .any(|l| l.starts_with("chan 0 ") && l.contains("recv_waiters=[1]")),
        "chan 0: {ls:?}"
    );
    assert!(
        ls.iter()
            .any(|l| l.starts_with("chan 1 ") && l.contains("recv_waiters=[2]")),
        "chan 1: {ls:?}"
    );
    // Lanes drained (nothing in flight — a pure chan wedge).
    assert!(
        ls.iter().any(|l| l == "pool cli queued=0 in_flight=0"),
        "cli idle: {ls:?}"
    );
    let _ = std::fs::remove_file(&out);
    // SAFETY: restore a neutral value for later tests in this process.
    unsafe {
        std::env::remove_var("TOTO_THREAD_DUMP");
    }
}
