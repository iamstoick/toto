//! Sequential corpus through the runtime executor (acceptance #7):
//! every positive sequential program must produce identical output
//! through the runtime executor as through the tree-walking interpreter
//! (whose goldens live in `tests/corpus`).
//!
//! Exclusions, each documented:
//! - `diag10_recursion_limit`: the 8,192-frame limit is
//!   INTERPRETER-ONLY (spec §5.9); the runtime executor grows heap
//!   frames and would abort at the 64 KiB guard page instead of
//!   panicking E0211. Never run it here.
//! - Concurrent programs (`uses_concurrency`): covered exactly by the
//!   CLI corpus runner and `concurrent.rs`, not here.
//! - Status-2 programs: static errors, nothing to execute (asserted).

use std::path::PathBuf;

use toto_compiler::{parse_and_check, render_panic};
use toto_runtime::{RuntimeConfig, run_checked};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
}

fn read_or_empty(path: &PathBuf) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

use std::path::Path;

#[test]
fn sequential_matches_goldens_through_runtime() {
    let root = corpus_root();
    let mut areas: Vec<_> = std::fs::read_dir(&root)
        .expect("corpus root readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut failures = Vec::new();
    let mut ran = 0usize;
    let mut skipped_concurrent = 0usize;
    let mut static_failures = 0usize;
    for area in areas {
        let mut files: Vec<_> = std::fs::read_dir(&area)
            .expect("area readable")
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
            if name == "diag10_recursion_limit.gzv" {
                continue;
            }
            let src = std::fs::read_to_string(&case).expect("read case");
            let status = std::fs::read_to_string(case.with_extension("status"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let dir = case.parent().expect("parent").to_path_buf();
            let saved_cwd = std::env::current_dir().expect("cwd");
            // Hold the case directory for the whole case: relative
            // file-IO builtins resolve against the process cwd, and
            // goldens were blessed with cwd set to the case dir
            // (as the CLI corpus runner does).
            std::env::set_current_dir(&dir).expect("set case cwd");
            let parsed = parse_and_check(&src);
            match parsed {
                Err(_) => {
                    assert_eq!(status, "2", "{name}: static failure but status is {status}");
                    static_failures += 1;
                    continue;
                }
                Ok((program, checked)) => {
                    if checked.uses_concurrency {
                        skipped_concurrent += 1;
                        continue;
                    }
                    assert!(
                        status == "0" || status == "1",
                        "{name}: checked but status is {status}"
                    );
                    // Default production config (1 MiB threshold): normal
                    // programs may or may not collect; `concurrent.rs`
                    // covers forced collection.
                    // NOTE: cwd is still the case dir here (restored
                    // below); relative file IO resolves against it.
                    let outcome = run_checked(&program, &checked, RuntimeConfig::default());
                    std::env::set_current_dir(&saved_cwd).expect("restore cwd");
                    let want_out = read_or_empty(&case.with_extension("stdout"));
                    if outcome.stdout != want_out {
                        failures.push(format!("{name}: stdout differs"));
                        continue;
                    }
                    match (status.as_str(), &outcome.result) {
                        ("0", Ok(_)) => {}
                        ("1", Err((diag, trace))) => {
                            let want_err = String::from_utf8_lossy(&read_or_empty(
                                &case.with_extension("stderr"),
                            ))
                            .into_owned();
                            let got_err = render_panic(diag, trace, &src, &name);
                            if got_err != want_err {
                                failures.push(format!(
                                    "{name}: stderr differs:\n got:\n{got_err}\n want:\n{want_err}"
                                ));
                            }
                        }
                        (s, r) => failures.push(format!(
                            "{name}: status {s} vs result {}",
                            if r.is_ok() { "ok" } else { "panic" }
                        )),
                    }
                    ran += 1;
                }
            }
        }
    }
    // `cli01_fileprog` writes `cli01_tmp.txt` next to its source (also
    // true of the CLI corpus runner): remove the artifact so the suite
    // leaves the tree clean. It is the only file-writing program.
    let _ = std::fs::remove_file(corpus_root().join("interp").join("cli01_tmp.txt"));
    // Exact corpus census (update deliberately when the corpus changes:
    // 42 status-0 + 4 status-1 runnable (int04_modzero joined in
    // Phase 4; state01-05 joined in Phase 6; diag16_ffi_array_sig
    // joined in Wave 1); 22 concurrent covered by `concurrent.rs`
    // (conc18_fneq joined in Phase 4, conc19_chan_eq in Phase 5,
    // state06-07 in Phase 6, conc20_net_hammer in Wave 1); diag10
    // excluded interpreter-only; 91 static negatives asserted
    // status-2 above (type37_index_range + diag13-15 FFI joined in
    // Phase 5; 22 Phase-6 negatives joined in Phase 6;
    // diag17_ffi_nonbyte_array joined in Wave 1)).
    assert_eq!(ran, 46, "sequential runnable count changed");
    assert_eq!(skipped_concurrent, 22, "concurrent skip count changed");
    assert_eq!(static_failures, 91, "static negative count changed");
    assert!(
        failures.is_empty(),
        "{} sequential divergence(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
