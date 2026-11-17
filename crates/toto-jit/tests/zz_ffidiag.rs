// Diagnostic probe (retained, Phase 7 Stage 2): wordfreq under task-mode
// JIT with infinite vs default GC threshold, printing RunStats. This is
// the only wordfreq JIT-task measurement (the bench probe is
// single-file-only, E0401); the phase report cites its numbers.
use std::path::PathBuf;
use std::time::Instant;
use toto_jit::{Jit, run_jit};
use toto_runtime::RuntimeConfig;

#[test]
fn zz_ffidiag_threshold() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/wordfreq");
    let work = PathBuf::from("/tmp/zzwf");
    if work.exists() {
        std::fs::remove_dir_all(&work).expect("clean work");
    }
    std::fs::create_dir_all(&work).expect("work dir");
    for name in ["main.gzv", "needle.txt", "input.txt", "toto.toml"] {
        std::fs::copy(src.join(name), work.join(name)).expect("copy file");
    }
    std::fs::create_dir_all(work.join("lib")).expect("lib dir");
    for entry in std::fs::read_dir(src.join("lib")).expect("read lib") {
        let entry = entry.expect("lib entry");
        std::fs::copy(entry.path(), work.join("lib").join(entry.file_name())).expect("copy lib");
    }
    // Scaled input: 1000x stock (6000 lines) for a measurable run.
    let input = std::fs::read_to_string(work.join("input.txt")).expect("read input");
    std::fs::write(work.join("input.txt"), input.repeat(1000)).expect("scale input");
    std::env::set_current_dir(&work).expect("cwd to work");

    let entry_src = std::fs::read_to_string("main.gzv").expect("read main");
    let read = |path: &str| -> Option<String> {
        if let Some(rest) = path.strip_prefix("gzv.std/") {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../stdlib")
                .join(rest);
            return std::fs::read_to_string(p).ok();
        }
        std::fs::read_to_string(PathBuf::from(".").join(path)).ok()
    };
    let (project, checked) =
        toto_compiler::project::load_and_check("main.gzv", &entry_src, &read, &[])
            .expect("wordfreq checks");
    for thresh in [u64::MAX, 1 << 20] {
        let mut jit = Jit::new_task().expect("jit");
        let prog = toto_compiler::mir::lower(&checked);
        let session = jit.compile_task(&prog).expect("compile");
        let config = RuntimeConfig {
            gc_threshold_bytes: thresh,
            ..Default::default()
        };
        let start = Instant::now();
        let outcome = run_jit(&project.program, &checked, config, &session);
        let el = start.elapsed();
        assert!(outcome.result.is_ok(), "run ok");
        eprintln!(
            "threshold={thresh}: elapsed={}ms collections={} allocated={} reclaimed={} switches={} stdout={:?}",
            el.as_millis(),
            outcome.stats.collections,
            outcome.stats.allocated,
            outcome.stats.reclaimed,
            outcome.stats.switches,
            String::from_utf8_lossy(&outcome.stdout),
        );
    }
}
