//! Focused interpreter tests: semantics the corpus covers end to end,
//! plus error-code and partial-output properties.

use toto_compiler::{Outcome, run_source};

fn run(src: &str) -> Outcome {
    run_source(src, "test.gzv")
}

fn success_code(src: &str) -> i32 {
    match run(src) {
        Outcome::Success { code, .. } => code,
        Outcome::Static(diags) => panic!("static errors: {diags:?}"),
        Outcome::RuntimePanic { diag, .. } => panic!("panic: {diag:?}"),
    }
}

fn panic_code(src: &str) -> &'static str {
    match run(src) {
        Outcome::RuntimePanic { diag, .. } => diag.code,
        other => panic!("expected panic, got {other:?}"),
    }
}

#[test]
fn overflow_panics_with_span_code() {
    assert_eq!(
        panic_code("fn main() -> int {\n return 9223372036854775807 + 1;\n}\n"),
        "E0201"
    );
}

#[test]
fn unit_falloff_returns_unit_for_unit_fns() {
    let code =
        success_code("fn greet() -> unit {\n}\nfn main() -> int {\n greet();\n return 0;\n}\n");
    assert_eq!(code, 0);
}

#[test]
fn non_unit_falloff_panics() {
    assert_eq!(
        panic_code("fn f() -> int {\n let x = 1;\n}\nfn main() -> int {\n return f();\n}\n"),
        "E0212"
    );
}

#[test]
fn nan_and_identity() {
    let code = success_code(
        "fn main() -> int {\n let n = 0.0 / 0.0;\n if n == n {\n return 1;\n }\n return 0;\n}\n",
    );
    assert_eq!(code, 0);
}

#[test]
fn closure_capture_copies_scalars() {
    let code = success_code(
        "fn main() -> int {\n let mut x = 1;\n let g = fn() -> int { return x; };\n x = 2;\n return g();\n}\n",
    );
    assert_eq!(code, 1);
}

#[test]
fn closure_sees_innermost_shadowed_binding() {
    let code = success_code(
        "fn main() -> int {\n let x = 1;\n let f = fn() -> int {\n let x = 2;\n let g = fn() -> int { return x; };\n return g();\n };\n return f() + x;\n}\n",
    );
    assert_eq!(code, 3);
}

#[test]
fn cyclic_display_terminates() {
    match run("fn main() -> int {\n let a = [];\n push(a, a);\n println(a);\n return 0;\n}\n") {
        Outcome::Success { stdout, .. } => {
            assert_eq!(stdout, b"[<cycle>]\n");
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn stdout_before_panic_is_preserved() {
    match run("fn main() -> int {\n println(\"before\");\n println(1 / 0);\n return 0;\n}\n") {
        Outcome::RuntimePanic { stdout, diag, .. } => {
            assert_eq!(stdout, b"before\n");
            assert_eq!(diag.code, "E0202");
        }
        other => panic!("expected panic, got {other:?}"),
    }
}
