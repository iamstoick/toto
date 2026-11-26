//! Yield from inside live JIT frames: a task suspends mid-JIT-code
//! (channel rendezvous), forced collections run while it is parked,
//! and its published roots survive: values cross back correctly on
//! resume. Asserts exact stdout plus collection activity. (A parked
//! sender, not a yield loop: `main` blocks in `toto_yield` with JIT
//! frames live on its coroutine stack until the spawned task pairs.)

use toto_compiler::parse_and_check;
use toto_jit::{Jit, run_jit};
use toto_runtime::RuntimeConfig;

const SRC: &str = r#"fn main() -> int {
    let c = chan<int>();
    let arr = [10, 20, 30];
    go {
        let v = <-c;
        println(v);
    }
    c <- 5;
    println(arr[0] + arr[1] + arr[2]);
    return 0;
}
"#;

#[test]
fn yield_from_live_jit_frames() {
    let (program, checked) = parse_and_check(SRC).expect("checks");
    assert!(checked.uses_concurrency);
    let mut jit = Jit::new_task().expect("session");
    let prog = toto_compiler::mir::lower(&checked);
    let session = jit.compile_task(&prog).expect("compiles");
    let config = RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 1,
        blocking_pool_threads: 2,
        // No coverage sink on yield test paths (Wave 8).
        coverage: None,
    };
    let outcome = run_jit(&program, &checked, config, &session);
    // `5` crosses the channel to the spawned task; `60` proves main's
    // array survived forced collections while main was parked.
    assert_eq!(outcome.stdout, b"5\n60\n".to_vec());
    assert!(
        matches!(outcome.result, Ok(0)),
        "result: {:?}",
        outcome.result
    );
    assert!(
        outcome.stats.tasks_spawned >= 2,
        "tasks: {}",
        outcome.stats.tasks_spawned
    );
    assert!(
        outcome.stats.collections >= 1,
        "no collections under pressure"
    );
}
