//! Trap probe: JIT-compile `udiv(arg0, arg1)` and call it with a
//! zero divisor. The process is EXPECTED to die from the Cranelift
//! integer-division trap. The parent test (`trap_child_dies`) runs
//! this binary as a child and asserts abnormal termination.
//!
//! Prints the call arguments first (flushed) so the parent can tell
//! a compile/call failure apart from the trap.

use std::io::Write;

use spike_3_cranelift::lower::Jit;
use spike_3_cranelift::toy::Expr;

fn main() {
    let mut jit = Jit::new().expect("jit session");
    // div10 will trap: 10 udiv 0.
    let f = jit.declare("div10", 2);
    jit.define(f, &Expr::div(Expr::arg(0), Expr::arg(1)));
    let ptr = jit.ptr(f);
    let mut rt = spike_3_cranelift::runtime::JitRuntime::new();
    let ctx = rt.ctx_ptr();
    eprintln!("calling div10(10, 0)");
    let _ = std::io::stderr().flush();
    let r = unsafe { Jit::call_ptr(ptr, ctx, &[10, 0]) };
    // Unreachable if the trap fires.
    eprintln!("SURVIVED with {r}");
    std::process::exit(41);
}
