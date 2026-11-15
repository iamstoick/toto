//! JIT compile-time measurement: compiles every checking corpus
//! program in both modes and asserts each function compiles within
//! a generous bound (guards pathological slowdown, not a benchmark).
//! Prints the full table (visible with `-- --nocapture`;
//! `scripts/check.sh` passes it so CI logs carry the numbers for the
//! phase report). Spike 3 toy baseline: 450–950 µs/function (debug).

use std::path::PathBuf;

use toto_compiler::{mir, parse_and_check};
use toto_jit::Jit;

/// Generous per-function bound (debug CI): ~25× the Spike 3 toy
/// maximum. Catches 100× pathology, never flakes on slow runners.
const PER_FN_MICROS: u64 = 25_000;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

#[test]
fn jit_compile_timings() {
    let root = corpus_root();
    let mut areas: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("corpus root")
        .map(|e| e.expect("area entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut gate_compiled = 0u32;
    let mut task_compiled = 0u32;
    let mut failures = Vec::new();
    println!("program,mode,function,micros");
    for area in areas {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&area)
            .expect("area dir")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
            .collect();
        files.sort();
        for case in files {
            let name = case
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            let src = std::fs::read_to_string(&case).expect("read case");
            let checked = match parse_and_check(&src) {
                Ok((_, checked)) => checked,
                Err(_) => continue,
            };
            let prog = mir::lower(&checked);
            // Gate mode covers sequential programs only (concurrency
            // is refused, by design).
            if !checked.uses_concurrency {
                let mut jit = Jit::new().expect("gate session");
                match jit.compile(&prog) {
                    Ok(()) => {
                        for (f, us) in &jit.timings {
                            println!("{name},gate,{f},{us}");
                            if *us > PER_FN_MICROS {
                                failures.push(format!("{name}: gate {f} took {us}µs"));
                            }
                        }
                        gate_compiled += 1;
                    }
                    Err(e) => failures.push(format!("{name}: gate compile refused: {e}")),
                }
            }
            let mut jit = Jit::new_task().expect("task session");
            match jit.compile_task(&prog) {
                Ok(_) => {
                    for (f, us) in &jit.timings {
                        println!("{name},task,{f},{us}");
                        if *us > PER_FN_MICROS {
                            failures.push(format!("{name}: task {f} took {us}µs"));
                        }
                    }
                    task_compiled += 1;
                }
                Err(e) => failures.push(format!("{name}: task compile refused: {e}")),
            }
        }
    }
    // Exact census (123 cases: 41 sequential checking incl. diag10,
    // 18 concurrent, 64 static).
    assert_eq!(gate_compiled, 41, "gate-compiled count changed");
    assert_eq!(task_compiled, 59, "task-compiled count changed");
    assert!(
        failures.is_empty(),
        "{} timing/compile failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
