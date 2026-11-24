//! Wave 4 Stage 3 SIGUSR2: a live service dumps its threads on
//! signal and keeps running, then exits cleanly. Unix-only.
//! Mirrors `profile_signal.rs` (child process + real signal).

#![cfg(unix)]

const WORKLOAD: &str = r#"fn burn(n: int) -> int {
    let mut x: int = 0;
    let mut i: int = 0;
    while i < n {
        x = x + i;
        i = i + 1;
    }
    return x;
}

fn main() -> int {
    let done = chan<int>(1);
    go {
        done <- burn(12000000);
    }
    let v = <-done;
    println(v);
    return 0;
}
"#;

#[test]
fn sigusr2_dumps_live_threads_without_stopping() {
    let dir = std::env::temp_dir().join(format!(
        "toto-dbg-sig-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let prog = dir.join("slow.gzv");
    std::fs::write(&prog, WORKLOAD).expect("workload");
    let out = dir.join("threads.txt");
    let toto = env!("CARGO_BIN_EXE_toto");
    let mut child = std::process::Command::new(toto)
        .arg("run")
        .arg(&prog)
        .env("TOTO_THREAD_DUMP", &out)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn toto");
    // Let the burn get going, then trigger mid-run.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(child.try_wait().expect("poll").is_none(), "still running");
    let pid = child.id() as libc::pid_t;
    // SAFETY: signaling our own direct child, nothing else.
    let rc = unsafe { libc::kill(pid, libc::SIGUSR2) };
    assert_eq!(rc, 0, "sigusr2 delivered");
    // The service keeps running AND the dump lands.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(child.try_wait().expect("poll").is_none(), "survived signal");
    let text = std::fs::read_to_string(&out).expect("mid-run thread dump");
    assert!(text.starts_with("threaddump v1\n"), "dump header");
    assert!(text.contains("task 1 go-1"), "burner task present");
    assert!(
        text.contains("blocked") || text.contains("running"),
        "states"
    );
    // Let it finish: clean exit, still green after the signal.
    let status = child.wait_timeout_or_kill(180);
    assert!(status.success(), "clean exit: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `wait` with a hard timeout (tests must never hang the suite).
trait WaitTimeoutOrKill {
    fn wait_timeout_or_kill(&mut self, secs: u64) -> std::process::ExitStatus;
}

impl WaitTimeoutOrKill for std::process::Child {
    fn wait_timeout_or_kill(&mut self, secs: u64) -> std::process::ExitStatus {
        let start = std::time::Instant::now();
        let limit = std::time::Duration::from_secs(secs);
        loop {
            if let Some(status) = self.try_wait().expect("poll") {
                return status;
            }
            if start.elapsed() > limit {
                let _ = self.kill();
                let status = self.wait().expect("reap");
                panic!("child timed out after {secs}s: {status:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
