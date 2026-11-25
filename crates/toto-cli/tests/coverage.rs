//! Stage 5 coverage CLI tests: golden LCOV output, the
//! --fail-under gate, and refusal paths (broken, concurrent,
//! missing files). `SF:` lines are normalized to `SF:FILE` so the
//! golden is path-independent (check.sh does the same with sed).

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
        .join("coverage")
        .join(name)
}

fn normalized_stdout(out: &std::process::Output, fixture_dir: &str) -> String {
    // Strip the absolute fixture directory so the golden is
    // path-independent (the file name itself is stable).
    String::from_utf8(out.stdout.clone())
        .expect("stdout utf8")
        .replace(&format!("SF:{fixture_dir}/"), "SF:")
}

#[test]
fn coverage_golden_lcov() {
    let prog = fixture("branches.gzv");
    let dir = prog
        .parent()
        .expect("fixture dir")
        .to_string_lossy()
        .to_string();
    let out = toto().arg("coverage").arg(&prog).output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let expected = std::fs::read_to_string(fixture("branches.gzv.lcov")).expect("golden readable");
    assert_eq!(normalized_stdout(&out, &dir), expected);
}

#[test]
fn coverage_fail_under_gate() {
    // Partial program: 80% lines, 50% branches.
    let prog = fixture("partial.gzv");
    let denied = toto()
        .arg("coverage")
        .arg("--fail-under")
        .arg("100")
        .arg(&prog)
        .output()
        .expect("runs");
    assert_eq!(denied.status.code(), Some(1));
    assert!(
        String::from_utf8(denied.stderr)
            .expect("stderr utf8")
            .contains("below --fail-under")
    );
    // Threshold at the measured level passes.
    let allowed = toto()
        .arg("coverage")
        .arg("--fail-under")
        .arg("80")
        .arg(&prog)
        .output()
        .expect("runs");
    assert_eq!(allowed.status.code(), Some(0));
    // Full coverage passes a 100 gate.
    let full = toto()
        .arg("coverage")
        .arg("--fail-under")
        .arg("100")
        .arg(fixture("branches.gzv"))
        .output()
        .expect("runs");
    assert_eq!(full.status.code(), Some(0));
}

#[test]
fn coverage_refuses_broken_and_concurrent() {
    let broken = toto()
        .arg("coverage")
        .arg(fixture("broken.gzv"))
        .output()
        .expect("runs");
    assert_eq!(broken.status.code(), Some(2));
    assert!(
        String::from_utf8(broken.stderr)
            .expect("stderr utf8")
            .contains("E0303")
    );
    let conc = toto()
        .arg("coverage")
        .arg(fixture("conc.gzv"))
        .output()
        .expect("runs");
    assert_eq!(conc.status.code(), Some(2));
    assert!(
        String::from_utf8(conc.stderr)
            .expect("stderr utf8")
            .contains("sequential programs only")
    );
}

#[test]
fn coverage_needs_a_file() {
    let out = toto().arg("coverage").output().expect("runs");
    assert_eq!(out.status.code(), Some(3));
    let missing = toto()
        .arg("coverage")
        .arg("no-such-file.gzv")
        .output()
        .expect("runs");
    assert_eq!(missing.status.code(), Some(3));
    let bad_pct = toto()
        .arg("coverage")
        .arg("--fail-under")
        .arg("101")
        .arg(fixture("branches.gzv"))
        .output()
        .expect("runs");
    assert_eq!(bad_pct.status.code(), Some(3));
}

#[test]
fn coverage_multi_file_sections() {
    // Each listed file gets its own SF section.
    let out = toto()
        .arg("coverage")
        .arg(fixture("branches.gzv"))
        .arg(fixture("partial.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("stdout utf8");
    assert_eq!(text.matches("end_of_record").count(), 2, "{text}");
    assert!(text.contains("branches.gzv"), "{text}");
    assert!(text.contains("partial.gzv"), "{text}");
}
