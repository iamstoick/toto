//! Subprocess harness for gate panic paths: compiles one MIR-subset
//! program, runs it, and lets `toto_panic` diverge (default handler:
//! spec-§6 shape on stderr, exit 1).
//!
//! Rationale: unwinding through JIT frames is unsound
//! (`cranelift-jit` emits no unwind tables), so a panic originating
//! inside generated code must leave the process rather than unwind.
//! Tests assert on the exit code and stderr.

use toto_compiler::{mir, parse_and_check};
use toto_jit::{Jit, JitRt};

fn main() {
    let case = std::env::args().nth(1).unwrap_or_default();
    // Each case: (source, entry, args). Args are provided by the
    // harness for cases real MIR cannot construct (stale handles).
    let (src, entry, args): (&str, &str, Vec<u64>) = match case.as_str() {
        "divzero" => (
            "fn main() -> int {\n let x = 10;\n let y = x / 0;\n return y;\n}",
            "main",
            vec![],
        ),
        "overflow" => (
            "fn main() -> int {\n let x = 9223372036854775807;\n return x + 1;\n}",
            "main",
            vec![],
        ),
        "min_div" => (
            "fn main() -> int {\n let a = -9223372036854775807 - 1;\n return a / -1;\n}",
            "main",
            vec![],
        ),
        "modzero" => (
            "fn main() -> int {\n let x = 7;\n return x % 0;\n}",
            "main",
            vec![],
        ),
        "oob" => (
            "fn main() -> int {\n let a = [1, 2];\n return a[5];\n}",
            "main",
            vec![],
        ),
        "neg" => (
            "fn main() -> int {\n let a = -9223372036854775807 - 1;\n return -a;\n}",
            "main",
            vec![],
        ),
        "stale" => (
            "fn f(h: Array<int>) -> int {\n return h[0];\n}\nfn main() -> int { return 0; }",
            "f",
            vec![u64::MAX],
        ),
        other => {
            eprintln!("panic_probe: unknown case `{other}`");
            std::process::exit(2);
        }
    };
    let (_, checked) = parse_and_check(src).unwrap_or_else(|ds| {
        eprintln!("panic_probe {case}: check failed: {ds:?}");
        std::process::exit(2);
    });
    let prog = mir::lower(&checked);
    let mut jit = Jit::new().unwrap_or_else(|e| {
        eprintln!("panic_probe {case}: jit session failed: {e}");
        std::process::exit(2);
    });
    if let Err(e) = jit.compile(&prog) {
        eprintln!("panic_probe {case}: subset refused: {e}");
        std::process::exit(2);
    }
    let mut rt = JitRt::new();
    rt.func = entry.to_string();
    unsafe { Jit::call_ptr(jit.ptr(entry), rt.ctx_ptr(), &args) };
    eprintln!("panic_probe {case}: returned without panicking");
    std::process::exit(2);
}
