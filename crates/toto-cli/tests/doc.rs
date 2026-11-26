//! Stage 4 `toto doc` CLI tests: stdout golden, `--out` files
//! golden, and refusal paths (broken, missing, usage). Module names
//! come from the file stem, so goldens are path-independent.

use std::path::PathBuf;
use std::process::Command;

fn toto() -> Command {
    let bin = env!("CARGO_BIN_EXE_toto");
    Command::new(bin)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("doc")
        .join(name)
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).expect("golden readable")
}

#[test]
fn doc_stdout_golden() {
    let out = toto()
        .arg("doc")
        .arg(fixture("math.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).expect("stdout utf8"),
        read_fixture("math.stdout.md")
    );
}

#[test]
fn doc_out_writes_module_and_index() {
    let dir = std::env::temp_dir().join(format!("toto-doc-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = toto()
        .arg("doc")
        .arg("--out")
        .arg(&dir)
        .arg(fixture("math.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(dir.join("math.md")).expect("module written"),
        read_fixture("math.module.md")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("index.md")).expect("index written"),
        read_fixture("math.files-index.md")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doc_refuses_broken_and_missing() {
    let broken = toto()
        .arg("doc")
        .arg(fixture("broken.gzv"))
        .output()
        .expect("runs");
    assert_eq!(broken.status.code(), Some(2));
    assert!(
        String::from_utf8(broken.stderr)
            .expect("stderr utf8")
            .contains("error[")
    );
    let missing = toto()
        .arg("doc")
        .arg(fixture("no-such-file.gzv"))
        .output()
        .expect("runs");
    assert_eq!(missing.status.code(), Some(3));
    let usage = toto().arg("doc").output().expect("runs");
    assert_eq!(usage.status.code(), Some(3));
    let no_out_dir = toto().arg("doc").arg("--out").output().expect("runs");
    assert_eq!(no_out_dir.status.code(), Some(3));
}
