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

/// Scratch project root unique to one test (pid-scoped; cleaned up).
fn scratch_project(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("toto-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn init_scaffolds_runnable_project() {
    let dir = scratch_project("init");
    let target = dir.join("hello");
    let out = toto().arg("init").arg(&target).output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert!(target.join("toto.toml").is_file());
    assert!(target.join("main.gzv").is_file());
    assert!(target.join("tests").join("example.gzv").is_file());
    // The scaffold runs as-is.
    let out = toto()
        .arg("run")
        .arg(target.join("main.gzv"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "hello from toto\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_refuses_existing_dir() {
    let dir = scratch_project("init-exists");
    let target = dir.join("taken");
    std::fs::create_dir_all(&target).expect("scratch dir");
    let out = toto().arg("init").arg(&target).output().expect("runs");
    assert_eq!(out.status.code(), Some(3));
    let _ = std::fs::remove_dir_all(&dir);
}

const BUILD_PROG: &str = "fn fact(n: int) -> int {\n    if n <= 1 {\n        return 1;\n    }\n    return n * fact(n - 1);\n}\n\nfn main() -> int {\n    println(fact(10));\n    let x: Result<int, string> = Ok(7);\n    println(x);\n    let s = \"answer\";\n    println(s);\n    return 0;\n}\n";

#[test]
fn test_command_reports_pass_and_fail() {
    let dir = scratch_project("testcmd");
    let out = toto()
        .arg("init")
        .arg(dir.join("app"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let root = dir.join("app");
    std::fs::write(
        root.join("tests").join("good.gzv"),
        "fn main() -> int {\n    println(\"good\");\n    return 0;\n}\n",
    )
    .expect("write test");
    std::fs::write(
        root.join("tests").join("bad.gzv"),
        "fn main() -> int {\n    return 1;\n}\n",
    )
    .expect("write test");
    // The scaffold's own example passes too: 2 pass, 1 fail.
    let out = toto()
        .arg("test")
        .current_dir(&root)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("ok example.gzv"), "stdout was:\n{stdout}");
    assert!(stdout.contains("ok good.gzv"), "stdout was:\n{stdout}");
    assert!(
        stdout.contains("FAIL bad.gzv (exit 1)"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("2 passed, 1 failed"),
        "stdout was:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_produces_equivalent_executable() {
    let dir = scratch_project("build");
    let out = toto()
        .arg("init")
        .arg(dir.join("calc"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let root = dir.join("calc");
    std::fs::write(root.join("main.gzv"), BUILD_PROG).expect("write main");
    let out = toto()
        .arg("build")
        .current_dir(&root)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "build failed");
    let bin = root.join("target").join("calc");
    assert!(bin.is_file(), "no binary at {}", bin.display());
    let built = std::process::Command::new(&bin)
        .output()
        .expect("built binary runs");
    let run = toto()
        .arg("run")
        .arg(root.join("main.gzv"))
        .output()
        .expect("runs");
    assert_eq!(built.status.code(), run.status.code());
    assert_eq!(built.stdout, run.stdout);
    assert_eq!(built.stderr, run.stderr);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_offline_needs_no_network() {
    // Production requirement (Phase 7, Stage 2): `toto build --offline`
    // performs no network fetch — cargo's own offline gate hard-errors
    // on any network attempt, so exit 0 here proves zero dependence.
    // Runs after `build_produces_equivalent_executable` warms the cache
    // in the same test process; a cold cache fails loudly (documented
    // in --help), it never silently fetches.
    let dir = scratch_project("buildoffline");
    let out = toto()
        .arg("init")
        .arg(dir.join("calc"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let root = dir.join("calc");
    std::fs::write(root.join("main.gzv"), BUILD_PROG).expect("write main");
    let out = toto()
        .arg("build")
        .arg("--offline")
        .current_dir(&root)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "offline build failed");
    let bin = root.join("target").join("calc");
    assert!(bin.is_file(), "no binary at {}", bin.display());
    let built = std::process::Command::new(&bin)
        .output()
        .expect("built binary runs");
    let run = toto()
        .arg("run")
        .arg(root.join("main.gzv"))
        .output()
        .expect("runs");
    assert_eq!(built.status.code(), run.status.code());
    assert_eq!(built.stdout, run.stdout);
    assert_eq!(built.stderr, run.stderr);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_runs_ffi_programs() {
    // Non-callback FFI lowers to `toto_foreign` and builds: the
    // built binary must match `run` byte-for-byte.
    let dir = scratch_project("buildffi");
    let out = toto()
        .arg("init")
        .arg(dir.join("app"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let root = dir.join("app");
    std::fs::write(
        root.join("main.gzv"),
        "unsafe extern \"C\" {\n    fn strlen(s: *c_char) -> c_long;\n    fn pow(x: c_double, y: c_double) -> c_double;\n}\n\nfn main() -> int {\n    println(strlen(\"hi\"));\n    println(pow(2.0, 10.0));\n    return 0;\n}\n",
    )
    .expect("write main");
    let out = toto()
        .arg("build")
        .current_dir(&root)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "build failed");
    let bin = root.join("target").join("app");
    assert!(bin.is_file(), "no binary at {}", bin.display());
    let built = std::process::Command::new(&bin)
        .output()
        .expect("built binary runs");
    let run = toto()
        .arg("run")
        .arg(root.join("main.gzv"))
        .output()
        .expect("runs");
    assert_eq!(built.status.code(), run.status.code());
    assert_eq!(built.stdout, run.stdout);
    assert_eq!(built.stderr, run.stderr);
    assert_eq!(
        String::from_utf8(built.stdout).unwrap(),
        "2\n1024.0\n",
        "wrong output"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_refuses_callback_ffi() {
    // Callbacks need evaluator drain with callable invocation,
    // which generated code cannot do: clean refusal with the
    // `run`/`test` workaround, never a miscompile.
    let dir = scratch_project("buildfficb");
    let out = toto()
        .arg("init")
        .arg(dir.join("app"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let root = dir.join("app");
    std::fs::write(
        root.join("main.gzv"),
        "unsafe extern \"C\" {\n    fn pthread_create(t: *c_void, attr: *c_void, f: fn(*c_void) -> *c_void, arg: *c_void) -> c_int;\n}\n\nfn answer(arg: *c_void) -> *c_void {\n    return arg;\n}\n\nfn main() -> int {\n    println(pthread_create(None, None, answer, None));\n    return 0;\n}\n",
    )
    .expect("write main");
    let out = toto()
        .arg("build")
        .current_dir(&root)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("takes a callback"),
        "wrong stderr"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fmt_reformats_and_is_stable() {
    // Messy input formats in place; a second pass (and `--check`)
    // reports clean.
    let dir = scratch_project("fmtbasic");
    let out = toto()
        .arg("init")
        .arg(dir.join("app"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let main = dir.join("app").join("main.gzv");
    std::fs::write(&main, "fn main()->int{\nprintln(  1+2 );\nreturn 0;\n}\n").expect("write");
    let out = toto().arg("fmt").arg(&main).output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let text = std::fs::read_to_string(&main).expect("read back");
    assert!(text.contains("fn main() -> int {"), "got:\n{text}");
    assert!(text.contains("println(1 + 2);"), "got:\n{text}");
    let out = toto()
        .arg("fmt")
        .arg("--check")
        .arg(&main)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fmt_check_flags_and_broken_refuses() {
    let dir = scratch_project("fmtcheck");
    let out = toto()
        .arg("init")
        .arg(dir.join("app"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let main = dir.join("app").join("main.gzv");
    std::fs::write(&main, "fn main()->int{\nreturn 0;\n}\n").expect("write");
    let out = toto()
        .arg("fmt")
        .arg("--check")
        .arg(&main)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("would reformat"),
        "wrong stdout"
    );
    std::fs::write(&main, "fn main( -> int {\n").expect("write broken");
    let out = toto().arg("fmt").arg(&main).output().expect("runs");
    assert_eq!(out.status.code(), Some(2));
    // Broken input is never overwritten.
    assert_eq!(
        std::fs::read_to_string(&main).expect("read back"),
        "fn main( -> int {\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
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

#[test]
fn opt_level_flag_documented() {
    let out = toto().arg("--help").output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("--opt-level LEVEL")
    );
}

#[test]
fn opt_level_flag_rejects_bad_value() {
    let out = toto()
        .arg("dev")
        .arg("--opt-level")
        .arg("bogus")
        .arg("no-such-file.gzv")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("needs off, speed, or speed_and_size")
    );
}
