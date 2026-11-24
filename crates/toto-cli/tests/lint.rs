//! Stage 1 lint CLI tests: exit codes, warning kinds, and each rule
//! firing (and staying silent) on fixture programs.

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
        .join("lint")
        .join(name)
}

fn proj_fixture(name: &str) -> PathBuf {
    fixture("proj").join(name)
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr utf8")
}

#[test]
fn unreachable_fires_e0501_as_warning() {
    let out = toto()
        .arg("lint")
        .arg(fixture("unreachable.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let err = stderr_of(&out);
    assert!(err.contains("warning[E0501]"), "got:\n{err}");
    assert!(!err.contains("error["), "warnings only, got:\n{err}");
}

#[test]
fn deny_warnings_turns_findings_into_failure() {
    let out = toto()
        .arg("lint")
        .arg("--deny-warnings")
        .arg(fixture("unreachable.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
    let clean = toto()
        .arg("lint")
        .arg("--deny-warnings")
        .arg(fixture("clean.gzv"))
        .output()
        .expect("runs");
    assert_eq!(clean.status.code(), Some(0));
}

#[test]
fn unused_let_flags_shadowed_outer_only() {
    let out = toto()
        .arg("lint")
        .arg(fixture("unused_let.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let err = stderr_of(&out);
    assert_eq!(err.matches("warning[E0502]").count(), 1, "got:\n{err}");
    assert!(err.contains("unused binding `x`"), "got:\n{err}");
    assert!(!err.contains("_ignored"), "underscore exempt, got:\n{err}");
    assert!(!err.contains("`used`"), "used binding silent, got:\n{err}");
}

#[test]
fn clean_file_is_silent() {
    let out = toto()
        .arg("lint")
        .arg(fixture("clean.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr_of(&out).is_empty());
}

#[test]
fn broken_file_refuses_with_check_error() {
    let out = toto()
        .arg("lint")
        .arg(fixture("broken.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2));
    let err = stderr_of(&out);
    assert!(err.contains("error[E0303]"), "got:\n{err}");
    assert!(
        !err.contains("warning["),
        "no lint on broken code, got:\n{err}"
    );
}

#[test]
fn library_without_main_gets_tier_a_only() {
    let out = toto()
        .arg("lint")
        .arg(fixture("library.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let err = stderr_of(&out);
    assert!(err.contains("warning[E0502]"), "got:\n{err}");
    assert!(err.contains("`dead`"), "got:\n{err}");
}

#[test]
fn used_import_is_silent_unused_import_flags_e0503() {
    let used = toto()
        .arg("lint")
        .arg(proj_fixture("main_used.gzv"))
        .output()
        .expect("runs");
    assert_eq!(used.status.code(), Some(0));
    assert!(stderr_of(&used).is_empty());
    let unused = toto()
        .arg("lint")
        .arg(proj_fixture("main_unused.gzv"))
        .output()
        .expect("runs");
    assert_eq!(unused.status.code(), Some(0));
    let err = stderr_of(&unused);
    assert!(err.contains("warning[E0503]"), "got:\n{err}");
    assert!(err.contains("unused import `lib.gzv`"), "got:\n{err}");
}

#[test]
fn lint_needs_a_file() {
    let out = toto().arg("lint").output().expect("runs");
    assert_eq!(out.status.code(), Some(3));
    let missing = toto()
        .arg("lint")
        .arg("no-such-file.gzv")
        .output()
        .expect("runs");
    assert_eq!(missing.status.code(), Some(3));
}
