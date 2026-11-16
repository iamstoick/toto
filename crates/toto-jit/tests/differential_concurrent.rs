//! JIT concurrent differential: the 17 concurrent programs through
//! the scheduler-driven JIT under deterministic mode (workers: 1,
//! fixed seed, forced GC), asserting exact goldens (status, stdout,
//! stderr) like `concurrent.rs`, each run twice (determinism proof
//! for the JIT path).

use std::path::PathBuf;

use toto_compiler::{parse_and_check, render_panic};
use toto_jit::{Jit, run_jit};
use toto_runtime::RuntimeConfig;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn read_or_empty(p: &std::path::Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_default()
}

fn det_config() -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E3779B97F4A7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 1,
        blocking_pool_threads: 2,
    }
}

#[test]
fn differential_task_concurrent() {
    let dir = corpus_root().join("concurrent");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("concurrent dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
        .collect();
    files.sort();
    let mut ran = 0u32;
    let mut failures = Vec::new();
    for case in files {
        let name = case
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let src = std::fs::read_to_string(&case).expect("read case");
        let status = std::fs::read_to_string(case.with_extension("status"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let want_out = read_or_empty(&case.with_extension("stdout"));
        let want_err =
            String::from_utf8_lossy(&read_or_empty(&case.with_extension("stderr"))).into_owned();
        let (program, checked) = match parse_and_check(&src) {
            Ok(v) => v,
            Err(diags) => {
                failures.push(format!(
                    "{name}: concurrent case failed to check: {diags:?}"
                ));
                continue;
            }
        };
        if !checked.uses_concurrency {
            failures.push(format!("{name}: concurrent case not flagged concurrent"));
            continue;
        }
        // Compile once, run twice (fresh scheduler each run): the two
        // runs must agree exactly (determinism), and match goldens.
        let mut jit = match Jit::new_task() {
            Ok(j) => j,
            Err(e) => {
                failures.push(format!("{name}: jit session failed: {e}"));
                continue;
            }
        };
        let prog = toto_compiler::mir::lower(&checked);
        let session = match jit.compile_task(&prog) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: task JIT refused: {e}"));
                continue;
            }
        };
        let saved_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(case.parent().expect("parent")).expect("set case cwd");
        let first = run_jit(&program, &checked, det_config(), &session);
        let second = run_jit(&program, &checked, det_config(), &session);
        std::env::set_current_dir(&saved_cwd).expect("restore cwd");
        let render = |o: &toto_runtime::RtOutcome| -> (String, Vec<u8>, String) {
            match &o.result {
                Ok(code) => (code.to_string(), o.stdout.clone(), String::new()),
                Err((diag, trace)) => (
                    "panic".to_string(),
                    o.stdout.clone(),
                    render_panic(diag, trace, &src, &name),
                ),
            }
        };
        let (c1, o1, e1) = render(&first);
        let (c2, o2, e2) = render(&second);
        if (c1.clone(), o1.clone(), e1.clone()) != (c2, o2, e2) {
            failures.push(format!("{name}: nondeterministic across runs"));
            continue;
        }
        if first.stats.tasks_spawned < 1 || first.stats.collections < 1 {
            failures.push(format!(
                "{name}: invariant stats failed: tasks_spawned={} switches={} steals={} high_water_max={} collections={} allocated={} reclaimed={}",
                first.stats.tasks_spawned,
                first.stats.switches,
                first.stats.steals,
                first.stats.high_water_max,
                first.stats.collections,
                first.stats.allocated,
                first.stats.reclaimed
            ));
            continue;
        }
        match status.as_str() {
            "0" => {
                if c1.parse::<i32>().is_err() || o1 != want_out || !e1.is_empty() {
                    failures.push(format!("{name}: golden mismatch (status 0)"));
                }
            }
            "1" => {
                if c1 != "panic" || o1 != want_out || e1 != want_err {
                    failures.push(format!(
                        "{name}: golden mismatch (status 1):\n got stderr:\n{e1}\n want:\n{want_err}"
                    ));
                }
            }
            s => failures.push(format!("{name}: unexpected golden status {s}")),
        }
        ran += 1;
    }
    assert_eq!(ran, 19, "concurrent differential count changed");
    assert!(
        failures.is_empty(),
        "{} concurrent divergence(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
