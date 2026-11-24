//! Wave 4 Stage 4 exit criterion: drain under load, then exit.
//! A live server (child `toto` process, plain HTTP) serves load;
//! SIGTERM mid-load drains in-flight work and exits cleanly,
//! with readiness going 503 while health stays 200. A second
//! case proves the deadline backstop: a hung handler forces a
//! loud nonzero exit plus a post-mortem thread dump.
//! Unix-only (signals); raw `TcpStream` HTTP/1.1 — no client deps.

#![cfg(unix)]

const SERVER: &str = r#"import "gzv.std/http.gzv";
import "gzv.std/net.gzv";
import "gzv.std/config.gzv";

fn burn(n: int) -> int {
    let mut x: int = 0;
    let mut i: int = 0;
    while i < n {
        x = x + i;
        i = i + 1;
    }
    return x;
}

fn h_health() -> fn(HttpRequest) -> Result<HttpResponse, string> {
    return fn(req: HttpRequest) -> Result<HttpResponse, string> {
        return http_text(200, "OK", "ok");
    };
}

fn h_ready() -> fn(HttpRequest) -> Result<HttpResponse, string> {
    return fn(req: HttpRequest) -> Result<HttpResponse, string> {
        let dir = config_env_get("TOTO_DRAIN_DIR");
        if file_exists(dir + "/draining") {
            return http_text(503, "Unavailable", "draining");
        } else {
        }
        return http_text(200, "OK", "ready");
    };
}

fn h_work() -> fn(HttpRequest) -> Result<HttpResponse, string> {
    return fn(req: HttpRequest) -> Result<HttpResponse, string> {
        let v = burn(600000);
        return http_text(200, "OK", to_string(v));
    };
}

fn h_hang() -> fn(HttpRequest) -> Result<HttpResponse, string> {
    return fn(req: HttpRequest) -> Result<HttpResponse, string> {
        let c = chan<int>(0);
        let v = <-c;
        return http_text(200, "OK", to_string(v));
    };
}

fn run() -> Result<string, string> {
    let router = http_router();
    route_add(router, "GET", "/healthz", h_health())?;
    route_add(router, "GET", "/readyz", h_ready())?;
    route_add(router, "GET", "/work", h_work())?;
    route_add(router, "GET", "/hang", h_hang())?;
    let port = config_parse_int(config_env_get("TOTO_TEST_PORT"))?;
    let listener = net_tcp_listen(port)?;
    let opts = HttpServeOpts { max_body: 65536, keepalive_ms: 0, server_name: "drain-test", expose_errors: false };
    // Joinable serve: blocks here until the drain closes the
    // listener. The runtime closed it already (or nothing did);
    // either way the process exits and the OS reclaims the fd.
    http_serve_join(listener, router, opts)?;
    return Ok("SERVED");
}

fn main() -> int {
    println(run());
    return 0;
}
"#;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "toto-drain-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Fallible HTTP/1.1 GET over a fresh connection. Connection:
/// close — no reuse. Errors (refused/reset/timeout) are returned,
/// never panicked: post-SIGTERM the drain closes the listener, so a
/// poll that races the close MUST tolerate a dead connection.
fn http_try(port: u16, target: &str, timeout: Duration) -> Result<(u16, String), String> {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    sock.set_read_timeout(Some(timeout))
        .map_err(|e| format!("timeout: {e}"))?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        target, port
    );
    sock.write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = text.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok((status, body))
}

/// Strict HTTP/1.1 GET: panics on any connection error. Use only
/// where the server is known stable (pre-SIGTERM); post-SIGTERM
/// polling must use `http_try`.
fn http_get(port: u16, target: &str) -> (u16, String) {
    http_try(port, target, Duration::from_secs(20))
        .unwrap_or_else(|e| panic!("http_get {target}: {e}"))
}

/// A child server that is SIGKILLed if the test panics before
/// reaping it. Without this, a spurious failure orphans a server
/// holding the fixed port; the next run's bind then fails (the
/// loser exits 0 silently) and the test talks to a stale, possibly
/// dying holder — a cascade of confusing RSTs. With this, every run
/// starts with a free port (see `assert_port_free`).
struct Guard {
    child: std::process::Child,
}

impl Guard {
    fn child(&mut self) -> &mut std::process::Child {
        &mut self.child
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.child.try_wait().map(|s| s.is_none()).unwrap_or(false) {
            let _ = self.child.kill();
        }
    }
}

/// Fail loudly (not with a mysterious downstream RST) if a stale
/// server from an earlier run still holds the port.
fn assert_port_free(port: u16) {
    if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
        panic!("port {port} already in use — kill the stale test server and re-run");
    }
}

/// Our server must be the one answering: if it died (e.g. a failed
/// bind after a stale holder grabbed the port), health/ready
/// results from a stranger prove nothing.
fn assert_child_alive(child: &mut std::process::Child, stage: &str) {
    assert!(
        child.try_wait().expect("poll").is_none(),
        "server died before {stage} — stale holder on the port?"
    );
}

static HEALTH_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn wait_healthy(port: u16) {
    let start = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let n = HEALTH_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!("HEALTH-ATTEMPT {n}");
            let (code, _) = http_get(port, "/healthz");
            if code == 200 {
                return;
            }
        }
        if start.elapsed() > Duration::from_secs(30) {
            panic!("server never became healthy");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_server(dir: &std::path::Path, port: u16, extra_env: &[(&str, &str)]) -> Guard {
    let prog = dir.join("srv.gzv");
    std::fs::write(&prog, SERVER).expect("workload");
    let toto = env!("CARGO_BIN_EXE_toto");
    let mut cmd = std::process::Command::new(toto);
    cmd.arg("run")
        .arg("--workers")
        .arg("4")
        .arg(&prog)
        .env("TOTO_DRAIN_DIR", dir)
        .env("TOTO_TEST_PORT", port.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // Plain `http_serve` needs its stdlib imports resolvable:
    // run from a dir is self-contained (no imports used).
    Guard {
        child: cmd.spawn().expect("spawn server"),
    }
}

#[test]
fn drain_under_load_then_exit_clean() {
    let dir = scratch("clean");
    let port: u16 = 48783;
    assert_port_free(port);
    let mut child = spawn_server(&dir, port, &[("TOTO_DRAIN_SECS", "10")]);
    wait_healthy(port);
    assert_child_alive(child.child(), "health");
    // Readiness is green before the drain.
    assert_eq!(http_get(port, "/readyz").0, 200, "ready before drain");
    assert_child_alive(child.child(), "ready");
    // In-flight work: two concurrent burns (~2 s each at
    // reference speed; TOTO_DRAIN_SECS=10 keeps slow machines
    // inside the deadline).
    let h1 = std::thread::spawn(move || http_get(port, "/work"));
    let h2 = std::thread::spawn(move || http_get(port, "/work"));
    std::thread::sleep(Duration::from_millis(300));
    // SIGTERM mid-load.
    let rc = unsafe { libc::kill(child.child().id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "sigterm delivered");
    // Drain observables (deterministic — no TCP-timing gamble).
    // drain_begin writes the flag file and closes every tracked
    // listener synchronously (µs apart), so a TCP poll can only see
    // a 503 in a microsecond flag-before-close window; a poll that
    // races the close gets RST/refused instead. Both prove the
    // drain ran — the flag file proves it began, a refused/503
    // proves the listener closed — while the exit-0 plus the
    // in-flight 200s below prove it drained rather than killed.
    let flag = dir.join("draining");
    let poll_timeout = Duration::from_secs(3);
    let start = std::time::Instant::now();
    let mut saw_flag = false;
    let mut saw_closed = false;
    let mut saw_503 = false;
    let mut exited_early = false;
    while start.elapsed() < Duration::from_secs(10) {
        if child.child().try_wait().expect("poll").is_some() {
            exited_early = true;
            break;
        }
        if flag.exists() {
            saw_flag = true;
        }
        match http_try(port, "/readyz", poll_timeout) {
            Ok((503, _)) => {
                saw_503 = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {
                saw_closed = true;
            }
        }
        if saw_flag && (saw_closed || saw_503) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(saw_flag, "drain flag file appeared");
    assert!(
        saw_closed || saw_503 || exited_early,
        "listener closed (503, refused, or clean exit) during drain"
    );
    // Health never 503s; if the listener is already closed the
    // drain closed it, which is the expected path — only a 200
    // answer must actually be 200.
    if let Ok((hcode, _)) = http_try(port, "/healthz", poll_timeout) {
        assert_eq!(hcode, 200, "health stays 200 during drain");
    }
    // In-flight requests complete (200), not killed.
    let (c1, _) = h1.join().expect("worker 1");
    let (c2, _) = h2.join().expect("worker 2");
    assert_eq!((c1, c2), (200, 200), "in-flight drained");
    // Clean exit, promptly (well under the 5 s default deadline).
    let status = wait_timeout_or_kill(child.child(), 30);
    assert!(status.success(), "clean exit 0: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drain_deadline_forces_loud_exit_with_dump() {
    let dir = scratch("forced");
    let port: u16 = 48784;
    assert_port_free(port);
    let dump = dir.join("threads.txt");
    let mut child = spawn_server(
        &dir,
        port,
        &[
            ("TOTO_DRAIN_SECS", "2"),
            ("TOTO_THREAD_DUMP", dump.to_str().unwrap()),
        ],
    );
    wait_healthy(port);
    assert_child_alive(child.child(), "health");
    // Park a request that never finishes.
    let h = std::thread::spawn(move || http_get(port, "/hang"));
    std::thread::sleep(Duration::from_millis(500));
    let rc = unsafe { libc::kill(child.child().id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "sigterm delivered");
    let status = wait_timeout_or_kill(child.child(), 30);
    assert!(!status.success(), "forced exit nonzero: {status:?}");
    // Join the hung request last (its connection dies with the run).
    let _ = h.join();
    // Post-mortem dump names the wedged work.
    let text = std::fs::read_to_string(&dump).expect("forced dump");
    assert!(text.starts_with("threaddump v1\n"), "dump header");
    assert!(text.contains("chan"), "chan wait shown: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `wait` with a hard timeout (tests must never hang the suite).
fn wait_timeout_or_kill(child: &mut std::process::Child, secs: u64) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().expect("poll") {
            return status;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let status = child.wait().expect("reap");
            panic!("child timed out after {secs}s: {status:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
