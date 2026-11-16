//! Stack re-measurement under JIT-generated code (Phase 4 gate):
//! realistic corpus programs through the task JIT must stay under
//! the ratified 64 KiB coroutine budget (asserted here; this file
//! runs in both debug and release CI jobs), with per-program
//! high-water printed for the report; deep recursion through JIT
//! frames must hit the guard page (subprocess killed by SIGSEGV,
//! never a clean exit or a trap signal).

use std::path::PathBuf;

use toto_compiler::parse_and_check;
use toto_jit::{Jit, run_jit};
use toto_runtime::RuntimeConfig;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn base_config() -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1024 * 1024,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: false,
        workers: 1,
        blocking_pool_threads: 2,
    }
}

fn pressure_config() -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 1,
        blocking_pool_threads: 2,
    }
}

const PROGRAMS: &[(&str, &str)] = &[
    ("interp", "int01_arith"),
    ("interp", "loop02_for_range"),
    ("interp", "struct01_basic"),
    ("interp", "clo04_higher_order"),
    ("concurrent", "conc01_pingpong"),
];

#[test]
fn jit_stack_high_water_under_budget() {
    let root = corpus_root();
    let mut failures = Vec::new();
    println!("program,config,high_water_max");
    for (area, name) in PROGRAMS {
        let case = root.join(area).join(format!("{name}.gzv"));
        let src = std::fs::read_to_string(&case).expect("read case");
        let (program, checked) = parse_and_check(&src).expect("stack program checks");
        let mut jit = Jit::new_task().expect("session");
        let prog = toto_compiler::mir::lower(&checked);
        let session = jit.compile_task(&prog).expect("stack program compiles");
        for (config, tag) in [(base_config(), "default"), (pressure_config(), "forced-gc")] {
            let outcome = run_jit(&program, &checked, config, &session);
            let hw = outcome.stats.high_water_max;
            println!("{name},{tag},{hw}");
            if hw >= 64 * 1024 {
                failures.push(format!("{name} [{tag}]: high-water {hw} exceeds 64 KiB"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} stack overrun(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

const DEEP_SRC: &str = r#"fn f(n: int) -> int {
    if n <= 0 {
        return 0;
    }
    return f(n - 1);
}
fn main() -> int {
    return f(200000);
}
"#;

#[test]
fn jit_guard_page_overflow() {
    // 200K nested JIT frames (≈ tens of MiB) against a 64 KiB guarded
    // coroutine stack: must die on the guard page, never return.
    let probe = env!("CARGO_BIN_EXE_jit_probe");
    let path = std::env::temp_dir().join(format!("toto-jit-deep-{}", std::process::id()));
    std::fs::write(&path, DEEP_SRC).expect("temp file write");
    let out = std::process::Command::new(probe)
        .arg("--task")
        .arg(&path)
        .current_dir(std::env::temp_dir())
        .output()
        .expect("probe runs");
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.code().is_none(),
        "deep JIT recursion exited {} instead of dying on the guard page",
        out.status.code().unwrap_or(-999)
    );
    // The killing signal is the guard page (SIGSEGV), never a trap
    // instruction (SIGILL/SIGTRAP/SIGFPE) from generated code.
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&out.status),
        Some(11),
        "want SIGSEGV, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
