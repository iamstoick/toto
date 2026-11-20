//! Golden corpus runner: every `tests/corpus/*/*.gzv` runs through the
//! real `toto` binary; stdout, stderr, and exit code must match goldens.

use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
}

fn cases() -> Vec<PathBuf> {
    let root = corpus_root();
    let mut out = Vec::new();
    let mut areas: Vec<_> = std::fs::read_dir(&root)
        .expect("corpus root readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    for area in areas {
        // Multi-file project cases live one level down: each immediate
        // subdirectory with a `main.gzv` is one case (support files are
        // never run standalone). Every other area runs top-level `*.gzv`.
        if area.file_name().is_some_and(|n| n == "project") {
            let mut dirs: Vec<_> = std::fs::read_dir(&area)
                .expect("project area readable")
                .map(|e| e.expect("dir entry").path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for dir in dirs {
                let main = dir.join("main.gzv");
                assert!(
                    main.is_file(),
                    "project case {} has no main.gzv",
                    dir.display()
                );
                out.push(main);
            }
            continue;
        }
        let mut files: Vec<_> = std::fs::read_dir(&area)
            .expect("area readable")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
            .collect();
        files.sort();
        out.extend(files);
    }
    out
}

fn sibling(path: &Path, ext: &str) -> PathBuf {
    path.with_extension(ext)
}

fn read_or_empty(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

#[test]
fn corpus() {
    let toto = env!("CARGO_BIN_EXE_toto");
    let cases = cases();
    assert!(
        !cases.is_empty(),
        "no corpus cases found under {}",
        corpus_root().display()
    );
    let mut failures = Vec::new();
    for case in &cases {
        let name = case
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let dir = case.parent().expect("parent dir").to_path_buf();
        let output = Command::new(toto)
            .arg("run")
            .arg(&name)
            .current_dir(&dir)
            .output()
            .expect("toto runs");
        // Clean up program-created scratch files before comparing.
        for entry in std::fs::read_dir(&dir).expect("read case dir") {
            let path = entry.expect("entry").path();
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("_tmp.txt"))
            {
                let _ = std::fs::remove_file(&path);
            }
        }
        let want_out = read_or_empty(&sibling(case, "stdout"));
        let want_err = read_or_empty(&sibling(case, "stderr"));
        let want_status: i32 = String::from_utf8(read_or_empty(&sibling(case, "status")))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(-999);
        let got_status = output.status.code().unwrap_or(-999);
        // Name the killing signal when there is one (Unix): a silent
        // `-999` hides whether the child segfaulted, aborted, or was
        // OOM-killed, which decides the whole diagnosis.
        #[cfg(unix)]
        let got_signal = {
            use std::os::unix::process::ExitStatusExt;
            output
                .status
                .signal()
                .map(|s| format!(" (signal {s})"))
                .unwrap_or_default()
        };
        #[cfg(not(unix))]
        let got_signal = String::new();
        if output.stdout != want_out || output.stderr != want_err || got_status != want_status {
            failures.push(format!(
                "--- {}\n want exit={} got exit={}{}\n want stdout={:?}\n got  stdout={:?}\n want stderr={:?}\n got  stderr={:?}",
                case.display(),
                want_status,
                got_status,
                got_signal,
                String::from_utf8_lossy(&want_out),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&want_err),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
