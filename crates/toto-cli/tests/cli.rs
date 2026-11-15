//! CLI-level tests: flags and file errors (program behavior is covered
//! by the corpus runner in `corpus.rs`).

use std::process::Command;

fn toto() -> Command {
    let bin = env!("CARGO_BIN_EXE_toto");
    Command::new(bin)
}

#[test]
fn help_and_version() {
    let out = toto().arg("--help").output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("toto run [--workers N] <file.gzv>")
    );
    let out = toto().arg("--version").output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "toto 0.1.0");
}

#[test]
fn missing_file_is_exit_3() {
    let out = toto()
        .arg("run")
        .arg("no-such-file.gzv")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("couldn't read")
    );
}

#[test]
fn bad_usage_is_exit_3() {
    let out = toto().arg("bogus").output().expect("runs");
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn workers_flag_runs_concurrent() {
    // `--workers N` routes through the M:N scheduler end to end.
    let prog = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
        .join("concurrent")
        .join("conc01_pingpong.gzv");
    let out = toto()
        .arg("run")
        .arg("--workers")
        .arg("4")
        .arg(&prog)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "42\n");
}

#[test]
fn workers_flag_rejects_zero() {
    let out = toto()
        .arg("run")
        .arg("--workers")
        .arg("0")
        .arg("no-such-file.gzv")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("needs a positive number")
    );
}
