//! Stage 5 coverage CLI tests: golden LCOV output, the
//! --fail-under gate, and refusal paths (broken, missing files),
//! plus Wave 8 concurrent goldens (go/channel, select arms,
//! branches) with a double-run determinism gate. `SF:` lines are
//! normalized to `SF:FILE` so the golden is path-independent
//! (check.sh does the same with sed).

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
fn coverage_refuses_broken() {
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
}

// Wave 8: concurrent programs are measured on the runtime
// executor under a single worker, not refused. Golden LCOV
// output for the go/channel, select-arm, and branch fixtures,
// plus a double-run determinism gate (workers = 1, fixed seed).
fn concurrent_golden(name: &str) -> String {
    let prog = fixture(name);
    let dir = prog
        .parent()
        .expect("fixture dir")
        .to_string_lossy()
        .to_string();
    let out = toto().arg("coverage").arg(&prog).output().expect("runs");
    assert_eq!(out.status.code(), Some(0), "{name}");
    let expected =
        std::fs::read_to_string(fixture(&format!("{name}.lcov"))).expect("golden readable");
    let actual = normalized_stdout(&out, &dir);
    assert_eq!(actual, expected, "{name}");
    actual
}

#[test]
fn coverage_concurrent_golden_lcov() {
    concurrent_golden("conc.gzv");
}

#[test]
fn coverage_concurrent_select_arms() {
    // Case 0 fires, then case 1, then `default`: every arm is
    // taken exactly once and no dispatch-poll edge leaks into
    // arms 0/1 of the two-case selects. The both-ready tie-break
    // deterministically picks case 0 under the fixed seed.
    let lcov = concurrent_golden("conc_select.gzv");
    assert!(lcov.contains("BRDA:5,0,0,1\n"), "{lcov}");
    assert!(lcov.contains("BRDA:22,0,2,1\n"), "{lcov}");
    assert!(lcov.contains("BRDA:37,0,0,1\n"), "{lcov}");
    assert!(lcov.contains("BRDA:37,0,1,0\n"), "{lcov}");
}

#[test]
fn coverage_concurrent_branches() {
    // Full range loop: entered (arm 0) and completed via the
    // bottom check (arm 1); empty range: skipped (arm 1 only).
    let lcov = concurrent_golden("conc_branches.gzv");
    assert!(lcov.contains("BRDA:14,0,0,3\n"), "{lcov}");
    assert!(lcov.contains("BRDA:14,0,1,1\n"), "{lcov}");
    assert!(lcov.contains("BRDA:23,0,0,0\n"), "{lcov}");
    assert!(lcov.contains("BRDA:23,0,1,1\n"), "{lcov}");
    assert!(lcov.contains("DA:24,0\n"), "{lcov}");
}

#[test]
fn coverage_concurrent_deterministic() {
    // Two back-to-back runs must agree byte for byte.
    for name in ["conc.gzv", "conc_select.gzv", "conc_branches.gzv"] {
        let prog = fixture(name);
        let first = toto().arg("coverage").arg(&prog).output().expect("runs");
        let second = toto().arg("coverage").arg(&prog).output().expect("runs");
        assert_eq!(first.status.code(), Some(0), "{name}");
        assert_eq!(second.status.code(), Some(0), "{name}");
        assert_eq!(first.stdout, second.stdout, "{name}");
    }
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
