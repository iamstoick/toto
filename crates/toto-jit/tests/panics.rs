//! Extended panic goldens: every reachable failing op through
//! generated code — subprocess exit 1, zero signals, exact
//! interpreter codes/messages/spans (full §6 heads byte-compared
//! against the tree-walking interpreter on the same sources, so no
//! span is hand-computed). Gate traces are single-frame by design
//! (presence asserted; depth is proven through task mode).
//!
//! Only checker-reachable failures appear here: the checker rejects
//! float-`%`, cross-type arithmetic/ordering, bad negations, missing
//! fields, and non-array lengths statically (E0303/E0305/E0310), so
//! those helpers stay dead-correct backstops. The one reachable
//! `E0203` (`fn` vs closure) is pinned twice: here and as
//! `conc18_fneq` for the concurrent executors.

const CASES: &[(&str, &str)] = &[
    (
        "sub_overflow",
        r#"fn main() -> int {
    let a = -9223372036854775807 - 1;
    return a - 1;
}
"#,
    ),
    (
        "mul_overflow",
        r#"fn main() -> int {
    let x = 9223372036854775807;
    return x * 2;
}
"#,
    ),
    (
        "store_oob",
        r#"fn main() -> int {
    let mut a = [1, 2];
    a[5] = 9;
    return 0;
}
"#,
    ),
    (
        "index_neg",
        r#"fn main() -> int {
    let a = [1, 2];
    return a[-1];
}
"#,
    ),
    (
        "falloff",
        r#"fn f() -> int {
    let x = 1;
}
fn main() -> int {
    return f();
}
"#,
    ),
    (
        "falloff_closure",
        r#"fn main() -> int {
    let g = fn() -> int { let x = 1; };
    return g();
}
"#,
    ),
    (
        "func_closure_eq",
        r#"fn f(a: int) -> int {
    return a;
}
fn main() -> int {
    let h = f;
    let g = fn(a: int) -> int { return a; };
    println(h == g);
    return 0;
}
"#,
    ),
    (
        "parse_int_invalid",
        r#"fn main() -> int {
    return parse_int("12x");
}
"#,
    ),
    (
        "parse_int_range",
        r#"fn main() -> int {
    return parse_int("99999999999999999999999");
}
"#,
    ),
    (
        "to_int_nan",
        r#"fn main() -> int {
    let n = 0.0 / 0.0;
    return to_int(n);
}
"#,
    ),
    (
        "abs_min",
        r#"fn main() -> int {
    let a = -9223372036854775807 - 1;
    return abs(a);
}
"#,
    ),
    (
        "pop_empty",
        r#"fn main() -> int {
    let mut a = [1];
    pop(a);
    return pop(a);
}
"#,
    ),
    (
        "read_missing",
        r#"fn main() -> int {
    let s = read_file("no-such-jit-probe-file.txt");
    return 0;
}
"#,
    ),
];

#[test]
fn panics_match_interpreter() {
    let probe = env!("CARGO_BIN_EXE_jit_probe");
    let mut failures = Vec::new();
    for (i, (name, src)) in CASES.iter().enumerate() {
        // Tree-walking oracle (must panic: a checking program that
        // returns here is a bad case, reported loudly).
        let (diag, trace) = match toto_compiler::run_source(src, "oracle.gzv") {
            toto_compiler::Outcome::RuntimePanic { diag, trace, .. } => (diag, trace),
            other => {
                failures.push(format!("{name}: interpreter did not panic: {other:?}"));
                continue;
            }
        };
        // Gate JIT subprocess on a temp file (unique per process and
        // case; removed afterwards, best-effort).
        let path =
            std::env::temp_dir().join(format!("toto-jit-panic-{}-{i}.gzv", std::process::id()));
        if std::fs::write(&path, src).is_err() {
            failures.push(format!("{name}: temp file write failed"));
            continue;
        }
        let out = std::process::Command::new(probe)
            .arg(&path)
            .current_dir(std::env::temp_dir())
            .output()
            .expect("probe runs");
        let _ = std::fs::remove_file(&path);
        match out.status.code() {
            Some(1) => {}
            Some(c) => {
                failures.push(format!("{name}: probe exit {c}, want 1"));
                continue;
            }
            None => {
                failures.push(format!("{name}: probe killed by signal"));
                continue;
            }
        }
        // Full §6 heads byte-compared (code, message, span, snippet,
        // carets); traces asserted present (gate depth is one frame
        // by design, the interpreter's is full).
        let filename = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let want = toto_compiler::render_panic(&diag, &trace, src, &filename);
        let got = String::from_utf8_lossy(&out.stderr).into_owned();
        let want_head = want.split("stack trace:\n").next().unwrap_or("");
        let got_head = got.split("stack trace:\n").next().unwrap_or("");
        if want_head != got_head {
            failures.push(format!("{name}: stderr head differs:\n got:\n{got_head}\n want:\n{want_head}"));
            continue;
        }
        if !got.contains("stack trace:\n") || !got.contains("in main") {
            failures.push(format!("{name}: probe trace missing/incomplete"));
            continue;
        }
    }
    assert_eq!(CASES.len(), 13, "panic case count changed");
    assert!(
        failures.is_empty(),
        "{} panic divergence(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
