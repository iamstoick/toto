//! Guard-page demonstration (NOT a test — it aborts by design).
//!
//! Builds the same 64 KiB `DefaultStack` the production scheduler uses
//! for tasks, then recurses natively until the `PROT_NONE` guard page
//! fires. Expected outcome: the process dies from `SIGSEGV` (never
//! silent corruption, never a graceful recovery).
//!
//! Run: `cargo run -p toto-runtime --example overflow`; observe the
//! signal with `echo $?` (139 = 128 + SIGSEGV).

use corosensei::{Coroutine, CoroutineResult, Yielder};

fn recurse(n: u64, y: &Yielder<(), ()>) {
    // Touch the stack so the frames are real.
    let mut scratch = [0u64; 64];
    scratch[0] = n;
    std::hint::black_box(&scratch);
    if n == 0 {
        y.suspend(());
    } else {
        recurse(n - 1, y);
    }
}

fn main() {
    let stack = corosensei::stack::DefaultStack::new(64 * 1024).expect("task stack");
    let mut co: Coroutine<(), (), (), _> =
        Coroutine::with_stack(stack, |y: &Yielder<(), ()>, _init| {
            recurse(10_000_000, y);
        });
    match co.resume(()) {
        CoroutineResult::Yield(()) => println!("suspended (unexpected)"),
        CoroutineResult::Return(()) => println!("returned (unexpected)"),
    }
}
