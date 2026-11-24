//! Wave 4 Stage 2 signal trigger: SIGUSR1 mid-run writes the
//! profile file while the service keeps running, and the service
//! then exits cleanly. Unix-only (the trigger path is unix-only
//! by design). Uses a scratch workload (not the corpus — no
//! golden impact) and the built `toto` binary.

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
fn sigusr1_dumps_profile_without_stopping_service() {
    let dir = std::env::temp_dir().join(format!(
        "toto-prof-sig-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let prog = dir.join("slow.gzv");
    std::fs::write(&prog, WORKLOAD).expect("workload");
    let out = dir.join("sig.prof");
    let toto = env!("CARGO_BIN_EXE_toto");
    let mut child = std::process::Command::new(toto)
        .arg("run")
        .arg(&prog)
        .env("TOTO_PROFILE", "1")
        .env("TOTO_PROFILE_OUT", &out)
        .env("TOTO_PROFILE_HZ", "100")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn toto");
    // Let the burn get going, then trigger mid-run.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(child.try_wait().expect("poll").is_none(), "still running");
    let pid = child.id() as i32;
    // SAFETY: signaling our own direct child, nothing else.
    let rc = unsafe { libc::kill(pid, libc::SIGUSR1) };
    assert_eq!(rc, 0, "sigusr1 delivered");
    // The service must keep running AND the dump must land.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(child.try_wait().expect("poll").is_none(), "survived signal");
    let text = std::fs::read_to_string(&out).expect("mid-run profile dump");
    assert!(text.starts_with("profile v1\n"), "dump header");
    assert!(text.contains("task 1 go samples="), "task attributed");
    assert!(text.contains("stack 1 go;burn "), "burn stack");
    // Let it finish: exit dump overwrites with the full run.
    let status = child.wait_timeout_or_kill(180);
    assert!(status.success(), "clean exit: {status:?}");
    let text2 = std::fs::read_to_string(&out).expect("exit profile dump");
    assert!(text2.starts_with("profile v1\n"), "exit header");
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
