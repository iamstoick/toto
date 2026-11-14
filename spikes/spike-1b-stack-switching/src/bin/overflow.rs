//! Overflow probe: runaway recursion on a small guarded stack.
//!
//! The parent test (`overflow_child_traps`) runs this binary as a
//! child process and asserts it dies by signal (SIGSEGV from the
//! guard page) instead of exiting or corrupting anything. Graceful
//! recovery is NOT attempted in this spike.

use std::hint::black_box;
use std::io::Write;

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult};

/// Intentional runaway: the guard page must turn this into a trap.
#[allow(unconditional_recursion)]
fn recurse(depth: u32) -> ! {
    // ~4 KiB frame so a 64 KiB stack overflows after a few frames.
    let buf = [0x5Au64; 512];
    let sum: u64 = buf.iter().sum();
    black_box(sum);
    if depth.is_multiple_of(4) {
        eprintln!("depth={depth}");
        let _ = std::io::stderr().flush();
    }
    recurse(depth + 1)
}

fn main() {
    let stack = DefaultStack::new(64 * 1024).expect("task stack");
    let mut co = Coroutine::with_stack(stack, |_, ()| {
        recurse(0);
    });
    match co.resume(()) {
        CoroutineResult::Yield(()) => unreachable!("never yields"),
        CoroutineResult::Return(()) => unreachable!("never returns"),
    }
}
