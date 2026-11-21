//! Wave 2 Stage 3 deadline enforcement (ADR-009): a task whose slot
//! carries a passed absolute monotonic-ms deadline fail-fasts with
//! E0214 at its next suspension point — the same fail-fast path as
//! cancellation. The past deadline installs deterministically (no
//! timing in the test); the parked receive never wakes, so the
//! failure can only come from the deadline check. Single worker,
//! exact assertions on code and message.

use toto_compiler::project;
use toto_runtime::{RuntimeConfig, run_checked};

fn run_src(src: &str) -> toto_runtime::RtOutcome {
    let (project, checked) = project::load_and_check(
        "main.gzv",
        src,
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("stage3 deadline program checks");
    assert!(checked.uses_concurrency, "deadline case must park");
    run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )
}

#[test]
fn past_deadline_fails_fast_at_park() {
    let outcome = run_src(
        r#"
import "gzv.std/context.gzv";
import "gzv.std/time.gzv";

fn t() -> Result<bool, string> {
    // Absolute past deadline: installs deterministically expired.
    let past = ctx_with_deadline(ctx_background(), -5000);
    ctx_set_current(past)?;
    let ch = chan<int>(1);
    // The park entry check fires before any wait: E0214, never a
    // hang (nothing is ever sent on `ch`).
    let v: int = <-ch;
    println(v);
    return Ok(true);
}

fn main() -> int {
    let yes: Result<bool, string> = Ok(true);
    if t() != yes {
        println("FAIL");
        return 1;
    } else {
    }
    return 0;
}
"#,
    );
    match &outcome.result {
        Ok(code) => panic!("past deadline parked cleanly (exit {code})"),
        Err((diag, _trace)) => {
            assert_eq!(diag.code, "E0214", "deadline code");
            assert!(
                diag.message.contains("deadline exceeded"),
                "deadline message: {}",
                diag.message
            );
        }
    }
}

#[test]
fn live_deadline_parks_cleanly() {
    // Control: the same shape with a future deadline parks and wakes
    // normally (a sender releases the receive), proving the check
    // admits live waits rather than failing every park.
    let outcome = run_src(
        r#"
import "gzv.std/context.gzv";
import "gzv.std/time.gzv";

fn t() -> Result<bool, string> {
    let fut = ctx_with_deadline(ctx_background(), 60000);
    ctx_set_current(fut)?;
    let ch = chan<int>(1);
    go {
        ch <- 7;
    }
    let v: int = <-ch;
    if v != 7 {
        return Err("bad value");
    } else {
    }
    return Ok(true);
}

fn main() -> int {
    let yes: Result<bool, string> = Ok(true);
    if t() != yes {
        println("FAIL");
        return 1;
    } else {
    }
    return 0;
}
"#,
    );
    match &outcome.result {
        Ok(code) => assert_eq!(*code, 0, "live deadline exits 0"),
        Err((diag, trace)) => panic!(
            "live deadline failed: {} {:?}",
            diag.message,
            trace.iter().map(|f| &f.name).collect::<Vec<_>>()
        ),
    }
}
