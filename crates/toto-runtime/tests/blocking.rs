//! Dedicated blocking pool (Stage 2 policy): file-system builtins run
//! on pool threads so no worker ever stalls on IO.
//!
//! - `blocking_file_io_parity`: the same program through the
//!   interpreter, the 1-worker runtime, and the 4-worker runtime
//!   produces identical status/stdout/stderr (same values, same E0210
//!   messages — formatted by one shared code path).
//! - `blocking_fifo_nonstall_order`: on ONE worker, a task parked on a
//!   pool read still lets every other task run to completion first
//!   (deterministic print order proves the worker was never stuck in
//!   the syscall).
//! - `blocking_fifo_rendezvous_no_deadlock`: reader and writer open
//!   opposite ends of a FIFO. `open` on a FIFO blocks until the peer
//!   appears — inline execution would wedge the only worker forever
//!   (E0216); the pool lets both ends meet. Linux-only (FIFOs).
//! - `blocking_error_paths`: missing-file and bad-path errors match
//!   the interpreter exactly.
//!
//! Temp files live under `std::env::temp_dir` with per-test unique
//! names; every test removes its artifacts. No chdir games.

use std::path::PathBuf;

use toto_compiler::{Outcome, parse_and_check, render_panic, run_source};
use toto_runtime::{RuntimeConfig, run_checked};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("toto-blocking-{}-{name}", std::process::id()))
}

fn rt_config(workers: usize) -> RuntimeConfig {
    RuntimeConfig {
        workers,
        ..RuntimeConfig::default()
    }
}

fn run_interp(src: &str, filename: &str) -> (String, Vec<u8>, String) {
    match run_source(src, filename) {
        Outcome::Success { code, stdout } => (code.to_string(), stdout, String::new()),
        Outcome::Static(diags) => ("static".to_string(), Vec::new(), format!("{diags:?}")),
        Outcome::RuntimePanic {
            diag,
            trace,
            stdout,
        } => (
            "1".to_string(),
            stdout,
            render_panic(&diag, &trace, src, filename),
        ),
    }
}

fn run_rt(src: &str, workers: usize) -> (String, Vec<u8>, String) {
    let (program, checked) = parse_and_check(src).expect("checked");
    let outcome = run_checked(&program, &checked, rt_config(workers));
    match &outcome.result {
        Ok(code) => (code.to_string(), outcome.stdout, String::new()),
        Err((diag, trace)) => (
            "1".to_string(),
            outcome.stdout,
            render_panic(diag, trace, src, "prog.gzv"),
        ),
    }
}

const PARITY_PROG: &str = r#"
fn main() -> int {
    write_file(DATA, "10\n20\n\n30\n");
    let text = read_file(DATA);
    let lines = split(text, "\n");
    println(len(lines));
    let mut total = 0;
    for line in lines {
        let t = trim(line);
        if t == "" {
            continue;
        }
        total = total + parse_int(t);
    }
    println(total);
    println(file_exists(DATA));
    println(file_exists(MISSING));
    return 0;
}
"#;

fn parity_src(data: &str, missing: &str) -> String {
    PARITY_PROG
        .replace("DATA", &format!("\"{data}\""))
        .replace("MISSING", &format!("\"{missing}\""))
}

#[test]
fn blocking_file_io_parity() {
    let data = tmp("data.txt");
    let missing = tmp("missing.txt");
    let _ = std::fs::remove_file(&missing);
    // No `go`: sequential programs run on both the interpreter and
    // the runtime executor (sequential differential), so this is a
    // pure builtin-parity check through the blocking pool.
    let src = parity_src(&data.to_string_lossy(), &missing.to_string_lossy());
    let a = run_interp(&src, "prog.gzv");
    let b = run_rt(&src, 1);
    let c = run_rt(&src, 4);
    assert_eq!(b, a, "1-worker parity");
    assert_eq!(c, a, "4-worker parity");
    let _ = std::fs::remove_file(&data);
}

#[test]
fn blocking_fifo_nonstall_order() {
    // One worker: the reader parks on the pool while the main task
    // runs to its own park; deterministic print order proves the
    // worker looped instead of wedging in the syscall.
    let data = tmp("order.txt");
    std::fs::write(&data, "hello-pool").expect("seed file");
    let d = data.to_string_lossy();
    let src = format!(
        r#"
fn main() -> int {{
    let ch = chan<int>();
    go {{
        let text = read_file("{d}");
        println(text);
        ch <- 1
    }}
    println("b-done");
    let x = <-ch;
    println(x);
    return 0;
}}
"#
    );
    let (status, out, err) = run_rt(&src, 1);
    assert_eq!(status, "0", "err: {err}");
    assert_eq!(out, b"b-done\nhello-pool\n1\n".to_vec());
    let _ = std::fs::remove_file(&data);
}

#[test]
fn blocking_fifo_rendezvous_no_deadlock() {
    // FIFO open(2) blocks until the peer end appears. Inline execution
    // on one worker would wedge the thread forever (E0216); the pool
    // lets the two ends meet. Requires two pool threads (reader and
    // writer block simultaneously).
    let fifo = tmp("rendezvous.fifo");
    let _ = std::fs::remove_file(&fifo);
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(fifo.as_os_str().as_bytes()).expect("cstring");
        let r = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(r, 0, "mkfifo");
    }
    #[cfg(not(unix))]
    panic!("FIFO rendezvous test needs Unix");
    let f = fifo.to_string_lossy();
    let src = format!(
        r#"
fn main() -> int {{
    let done = chan<int>(2);
    go {{
        let text = read_file("{f}");
        println(text);
        done <- 1
    }}
    go {{
        write_file("{f}", "through-the-fifo");
        println("wrote");
        done <- 1
    }}
    let a = <-done;
    let b = <-done;
    println(a + b);
    return 0;
}}
"#
    );
    let (status, out, err) = run_rt(&src, 1);
    assert_eq!(status, "0", "fifo run failed: {err}");
    // Reader/writer completion order varies (two pool threads); the
    // set of lines is exact.
    let mut lines: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
    lines.retain(|l| !l.is_empty());
    lines.sort_unstable();
    assert_eq!(
        lines,
        vec![
            b"2".as_slice(),
            b"through-the-fifo".as_slice(),
            b"wrote".as_slice()
        ]
    );
    let _ = std::fs::remove_file(&fifo);
}

#[test]
fn blocking_error_paths() {
    let missing = tmp("nope.txt");
    let _ = std::fs::remove_file(&missing);
    let bad = tmp("no-such-dir/x.txt");
    let m = missing.to_string_lossy();
    let b = bad.to_string_lossy();
    let src = format!(
        r#"
fn main() -> int {{
    println(read_file("{m}"));
    return 0;
}}
"#
    );
    let a = run_interp(&src, "prog.gzv");
    let r1 = run_rt(&src, 1);
    let r4 = run_rt(&src, 4);
    assert_eq!(a.0, "1");
    assert_eq!(r1, a, "1-worker error parity");
    assert_eq!(r4, a, "4-worker error parity");

    let src = format!(
        r#"
fn main() -> int {{
    println(file_exists("{m}"));
    println(file_exists("{b}"));
    write_file("{b}", "x");
    return 0;
}}
"#
    );
    let a = run_interp(&src, "prog.gzv");
    let r1 = run_rt(&src, 1);
    assert_eq!(r1, a, "exists/write error parity");
}
