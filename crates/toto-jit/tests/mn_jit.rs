//! M:N hammer through the JIT path (Phase 4 re-run of the Phase 3
//! stress suite): race-free exact goldens, tie-break membership,
//! racy legal outcomes, negatives exact, the pair hammer at 1/2/4/8
//! workers plus a forced-GC pass, and the 1,710-run looped hammer
//! (19 programs × 30 rounds × workers {1,2,4} under forced GC — the
//! 1,530-run Phase 3 shape grown by `conc18_fneq` and
//! `conc19_chan_eq`). The single transient Phase 3 `mn` failure is
//! dispositioned by this file's green run: reproduced-and-fixed, or
//! absent — never unexplained.

use std::path::{Path, PathBuf};

use toto_compiler::ast::Program;
use toto_compiler::hir::CheckedProgram;
use toto_compiler::{parse_and_check, render_panic};
use toto_jit::{Jit, JitSession, run_jit};
use toto_runtime::RuntimeConfig;

fn concurrent_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
        .join("concurrent")
}

fn mn_config() -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 4,
        blocking_pool_threads: 2,
    }
}

struct CaseOut {
    status: String,
    stdout: Vec<u8>,
    stderr: String,
}

fn load(name: &str) -> (String, Program, CheckedProgram, JitSession) {
    let case = concurrent_dir().join(format!("{name}.gzv"));
    let src = std::fs::read_to_string(&case).expect("read case");
    let (program, checked) = parse_and_check(&src)
        .map_err(|diags| format!("{name}: static errors: {diags:?}"))
        .expect("checked");
    assert!(checked.uses_concurrency, "{name}: not concurrent");
    let mut jit = Jit::new_task().expect("jit session");
    let prog = toto_compiler::mir::lower(&checked);
    let session = jit.compile_task(&prog).expect("jit compiles");
    (src, program, checked, session)
}

fn run_compiled(
    name: &str,
    src: &str,
    program: &Program,
    checked: &CheckedProgram,
    session: &JitSession,
    config: RuntimeConfig,
) -> CaseOut {
    let dir = concurrent_dir();
    let saved = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("set case cwd");
    let outcome = run_jit(program, checked, config, session);
    std::env::set_current_dir(&saved).expect("restore cwd");
    assert!(outcome.stats.tasks_spawned >= 1, "{name}: no tasks spawned");
    assert!(
        outcome.stats.collections >= 1,
        "{name}: no collections under M:N pressure"
    );
    let (status, stderr) = match &outcome.result {
        Ok(_) => ("0".to_string(), String::new()),
        Err((diag, trace)) => (
            "1".to_string(),
            render_panic(diag, trace, src, &format!("{name}.gzv")),
        ),
    };
    CaseOut {
        status,
        stdout: outcome.stdout,
        stderr,
    }
}

fn run_case(name: &str, config: RuntimeConfig) -> CaseOut {
    let (src, program, checked, session) = load(name);
    run_compiled(name, &src, &program, &checked, &session, config)
}

fn golden(name: &str, ext: &str) -> Vec<u8> {
    std::fs::read(concurrent_dir().join(format!("{name}.{ext}"))).unwrap_or_default()
}

fn golden_str(name: &str, ext: &str) -> String {
    String::from_utf8_lossy(&golden(name, ext)).into_owned()
}

#[test]
fn mn_jit_exact_race_free() {
    for name in [
        "conc01_pingpong",
        "conc02_buffered_close",
        "conc04_select_default",
        "conc05_moved_struct",
        "conc06_fanout",
        "conc07_zero_values",
        "conc08_go_early_return",
        "conc09_nested_go",
        "conc11_chan_of_chan",
        "conc19_chan_eq",
    ] {
        let out = run_case(name, mn_config());
        assert_eq!(
            out.status,
            golden_str(name, "status").trim(),
            "{name}: status"
        );
        assert_eq!(out.stdout, golden(name, "stdout"), "{name}: stdout");
        if out.status == "0" {
            assert!(
                golden(name, "stderr").is_empty(),
                "{name}: unexpected stderr golden"
            );
        } else {
            assert_eq!(out.stderr, golden_str(name, "stderr"), "{name}: stderr");
        }
    }
}

#[test]
fn mn_jit_select_tiebreak_membership() {
    let out = run_case("conc03_select_two", mn_config());
    assert_eq!(out.status, "0");
    assert!(
        out.stdout == b"10\n".to_vec() || out.stdout == b"20\n".to_vec(),
        "conc03: illegal output {:?}",
        out.stdout
    );
}

#[test]
fn mn_jit_racy_select_legal_outcomes() {
    let out = run_case("conc10_select_send", mn_config());
    if out.status == "0" {
        assert_eq!(out.stdout, b"1\n2\n".to_vec());
    } else {
        assert_eq!(out.status, "1");
        // The E0216 branch prints `2` first (the select took the `b`
        // branch, then the trailing `<-b` deadlocks) — never empty.
        assert_eq!(out.stdout, b"2\n".to_vec());
        assert!(
            out.stderr.contains("panic[E0216]"),
            "conc10: non-deadlock failure is illegal: {}",
            out.stderr
        );
    }
}

#[test]
fn mn_jit_stress_pairing() {
    let src = r#"fn main() -> int {
    let ch = chan<int>();
    let mut round = 0;
    while round < 40 {
        for k in 0..8 {
            go {
                ch <- k
            }
        }
        let mut got = 0;
        let mut n = 0;
        while n < 8 {
            got = got + <-ch;
            n = n + 1;
        }
        if got != 28 {
            println("MISMATCH");
            return 1;
        }
        round = round + 1;
    }
    println("ok");
    return 0;
}"#;
    let (program, checked) = parse_and_check(src).expect("stress program checks");
    assert!(checked.uses_concurrency, "stress program is concurrent");
    let mut jit = Jit::new_task().expect("jit session");
    let prog = toto_compiler::mir::lower(&checked);
    let session = jit.compile_task(&prog).expect("stress program compiles");
    for workers in [1usize, 2, 4, 8] {
        let cfg = RuntimeConfig {
            workers,
            ..mn_config()
        };
        let cfg = RuntimeConfig {
            collect_at_every_safepoint_for_test: false,
            ..cfg
        };
        let outcome = run_jit(&program, &checked, cfg, &session);
        assert!(
            outcome.result.is_ok(),
            "stress pairing w={workers}: {}",
            outcome
                .result
                .as_ref()
                .err()
                .map(|(d, _)| format!("{} {}", d.code, d.message))
                .unwrap_or_default()
        );
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "ok\n",
            "stress pairing w={workers}: stdout"
        );
    }
    let outcome = run_jit(&program, &checked, mn_config(), &session);
    assert!(
        outcome.result.is_ok(),
        "stress pairing w=4 forced-gc: {}",
        outcome
            .result
            .as_ref()
            .err()
            .map(|(d, _)| format!("{} {}", d.code, d.message))
            .unwrap_or_default()
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.stdout),
        "ok\n",
        "stress pairing w=4 forced-gc: stdout"
    );
}

#[test]
fn mn_jit_negatives_exact() {
    for name in [
        "conc_neg_send_closed",
        "conc_neg_double_close",
        "conc_neg_deadlock",
        "conc_neg_select_send_closed",
        "conc_neg_child_panic",
        "conc_neg_main_panic",
        "conc18_fneq",
    ] {
        let out = run_case(name, mn_config());
        assert_eq!(out.status, "1", "{name}: status");
        assert_eq!(out.stdout, golden(name, "stdout"), "{name}: stdout");
        assert_eq!(out.stderr, golden_str(name, "stderr"), "{name}: stderr");
    }
}

const RACE_FREE: &[&str] = &[
    "conc01_pingpong",
    "conc02_buffered_close",
    "conc04_select_default",
    "conc05_moved_struct",
    "conc06_fanout",
    "conc07_zero_values",
    "conc08_go_early_return",
    "conc09_nested_go",
    "conc11_chan_of_chan",
    "conc19_chan_eq",
];

const NEGATIVES: &[&str] = &[
    "conc_neg_send_closed",
    "conc_neg_double_close",
    "conc_neg_deadlock",
    "conc_neg_select_send_closed",
    "conc_neg_child_panic",
    "conc_neg_main_panic",
    "conc18_fneq",
];

fn check_legal(name: &str, out: &CaseOut, failures: &mut Vec<String>) {
    if RACE_FREE.contains(&name) {
        if out.status != golden_str(name, "status").trim()
            || out.stdout != golden(name, "stdout")
            || out.stderr != golden_str(name, "stderr")
        {
            failures.push(format!("{name}: race-free golden mismatch"));
        }
    } else if name == "conc03_select_two" {
        if out.status != "0" || !(out.stdout == b"10\n".to_vec() || out.stdout == b"20\n".to_vec())
        {
            failures.push(format!("{name}: illegal tie-break {:?}", out.stdout));
        }
    } else if name == "conc10_select_send" {
        let legal_ok = out.status == "0" && out.stdout == b"1\n2\n".to_vec();
        let legal_race = out.status == "1"
            && out.stdout == b"2\n".to_vec()
            && out.stderr.contains("panic[E0216]");
        if !(legal_ok || legal_race) {
            failures.push(format!("{name}: illegal racy outcome {}", out.status));
        }
    } else if NEGATIVES.contains(&name) {
        if out.status != "1"
            || out.stdout != golden(name, "stdout")
            || out.stderr != golden_str(name, "stderr")
        {
            failures.push(format!("{name}: negative golden mismatch"));
        }
    } else {
        failures.push(format!("{name}: unknown program in hammer list"));
    }
}

#[test]
fn mn_jit_hammer_1710() {
    // 19 programs × 30 rounds × workers {1,2,4}, forced GC. Compile
    // once per program (compile is deterministic), run hot: this
    // hammers the runtime for flakes, including the transient
    // Phase 3 `mn` failure (absent here or reproduced-and-fixed).
    let mut names: Vec<&str> = Vec::new();
    names.extend(RACE_FREE.iter().copied());
    names.push("conc03_select_two");
    names.push("conc10_select_send");
    names.extend(NEGATIVES.iter().copied());
    assert_eq!(names.len(), 19, "hammer program count changed");
    let mut failures = Vec::new();
    let mut runs = 0u32;
    for name in names {
        let (src, program, checked, session) = load(name);
        for _ in 0..30 {
            for workers in [1usize, 2, 4] {
                let cfg = RuntimeConfig {
                    workers,
                    ..mn_config()
                };
                let out = run_compiled(name, &src, &program, &checked, &session, cfg);
                runs += 1;
                check_legal(name, &out, &mut failures);
            }
        }
    }
    assert_eq!(runs, 1710, "hammer volume changed");
    assert!(
        failures.is_empty(),
        "{} hammer failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
