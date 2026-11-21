//! TLS interop against the reference (Enterprise Wave 2, Stage 1):
//! sequential Toto programs through the runtime executor (pool path:
//! `BlockingOp::Tls` in `sched.rs`, `exec_tls` in `exec.rs`) with
//! `openssl s_server` / `openssl s_client` as the peer — both
//! directions, plus the untrusted-chain negative.
//!
//! Same fixtures, mechanics, and no-pinned-wording rule as the
//! interpreter-side suite (`toto-compiler/tests/tls_interop.rs`;
//! see its header for the stdin/pipe/readiness rationale). These
//! programs are sequential (no `go`) yet still exercise the pool:
//! every TLS step parks the task on the dedicated blocking pool,
//! never inline on a worker, never on the poller.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use toto_compiler::parse_and_check;
use toto_runtime::{RtOutcome, RuntimeConfig, run_checked};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn fixtures() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("corpus")
        .join("project")
        .join("proj15_tls_echo")
        .join("fixtures")
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = workspace_root()
        .join("target")
        .join("tls-interop")
        .join(name);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A held-open openssl peer: stdin stays open (fed `send` up front)
/// so the relay survives the whole session; stdout is a pipe drained
/// by a reader thread expecting exactly 2 bytes. Dropping kills the
/// peer (no `-naccept`, no clean-exit dependence).
struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    capture: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl Peer {
    fn spawn(
        program: &str,
        args: &[String],
        send: &[u8],
        capture_len: usize,
        stderr_path: &Path,
    ) -> Peer {
        let err = std::fs::File::create(stderr_path).expect("stderr file");
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("openssl peer (needed for TLS interop)");
        let mut stdin = child.stdin.take().expect("peer stdin");
        stdin.write_all(send).expect("peer stdin");
        stdin.flush().expect("peer stdin flush");
        let stdout = child.stdout.take().expect("peer stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        // Detached drain: `read_exact` returns as soon as the bytes
        // arrive (no exit needed); if they never do, the main thread
        // fails on its own timeout and this thread dies at process
        // exit (same detached-thread discipline as the net watchdog).
        std::thread::spawn(move || {
            read_exact_into(stdout, capture_len, tx);
        });
        Peer {
            child,
            stdin: Some(stdin),
            capture: rx,
        }
    }

    fn captured(self, what: &str) -> Vec<u8> {
        self.capture
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|_| panic!("{what}: relayed bytes never arrived"))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
    }
}

fn read_exact_into(mut stdout: ChildStdout, len: usize, tx: std::sync::mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; len];
    if stdout.read_exact(&mut buf).is_ok() {
        let _ = tx.send(buf);
    }
}

fn spawn_s_server(port: u16, dir: &Path) -> Peer {
    let f = fixtures();
    Peer::spawn(
        "openssl",
        &[
            "s_server".to_string(),
            "-accept".to_string(),
            port.to_string(),
            "-cert".to_string(),
            f.join("server-cert.pem").display().to_string(),
            "-key".to_string(),
            f.join("server-key.pem").display().to_string(),
            "-quiet".to_string(),
        ],
        b"Hi",
        2,
        &dir.join("s_server.err"),
    )
}

fn spawn_s_client(port: u16, ca: &Path, dir: &Path, name: &str) -> Peer {
    Peer::spawn(
        "openssl",
        &[
            "s_client".to_string(),
            "-connect".to_string(),
            format!("127.0.0.1:{port}"),
            "-CAfile".to_string(),
            ca.display().to_string(),
            "-quiet".to_string(),
            "-verify_return_error".to_string(),
        ],
        b"Hi",
        2,
        &dir.join(format!("{name}.err")),
    )
}

/// Run a sequential Toto program on the M:N executor with a watchdog
/// (a TLS stall must fail loudly, never hang the suite).
fn run_program(src: &str, what: &str) -> RtOutcome {
    let (program, checked) = parse_and_check(src)
        .map_err(|diags| format!("{what}: static errors: {diags:?}"))
        .expect("checked");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = run_checked(&program, &checked, RuntimeConfig::default());
        let _ = tx.send(outcome);
    });
    rx.recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("{what}: Toto program stalled (TLS deadlock?)"))
}

/// Run the client until the server is up: connection-refused consumes
/// no server state, so retrying is exact (unlike a TCP probe, which
/// would consume a single-shot accept).
fn run_client_until_ready(src: &str, what: &str) -> RtOutcome {
    let start = Instant::now();
    loop {
        let outcome = run_program(src, what);
        let text = String::from_utf8_lossy(&outcome.stdout);
        let refused = matches!(&outcome.result, Ok(1))
            && text.contains("refused")
            && start.elapsed() < Duration::from_secs(10);
        if refused {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        return outcome;
    }
}

/// Wait for a ready file the server program writes after `listen`
/// (probing would consume its single-shot `accept`).
fn wait_for_ready(path: &Path, what: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if path.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{what}: server never signalled readiness");
}

const EXTERNS: &str = r#"
unsafe extern "C" {
    fn toto_rt_net_connect_ip(ip: *c_char, port: c_int) -> c_int;
    fn toto_rt_net_listen(port: c_int) -> c_int;
    fn toto_rt_net_accept(listener: c_int) -> c_int;
    fn toto_rt_net_close(sock: c_int) -> c_int;
    fn toto_rt_net_last_error() -> string;
    fn toto_rt_tls_connect(conn: c_int, ca_pem: *c_char) -> c_int;
    fn toto_rt_tls_accept(conn: c_int, cert_pem: *c_char, key_pem: *c_char) -> c_int;
    fn toto_rt_tls_send(conn: c_int, data: Array<int>) -> c_int;
    fn toto_rt_tls_recv(conn: c_int, max: c_int) -> Array<int>;
    fn toto_rt_tls_close(sock: c_int) -> c_int;
    fn toto_rt_tls_last_error() -> string;
}
"#;

fn client_program(port: u16, ca_name: &str) -> String {
    let ca = fixtures().join(ca_name);
    format!(
        r#"{EXTERNS}
fn main() -> int {{
    let fd = toto_rt_net_connect_ip("127.0.0.1", {port});
    if fd < 0 {{
        println(toto_rt_net_last_error());
        return 1;
    }}
    let ca = read_file("{}");
    let tls = toto_rt_tls_connect(fd, ca);
    if tls < 0 {{
        println("NEG-OK");
        return 0;
    }}
    if toto_rt_tls_send(tls, [72, 105]) != 2 {{
        println(toto_rt_tls_last_error());
        return 1;
    }}
    let resp = toto_rt_tls_recv(tls, 16);
    if len(resp) != 2 || resp[0] != 72 || resp[1] != 105 {{
        println("BAD-ECHO");
        return 1;
    }}
    toto_rt_tls_close(tls);
    println("CLIENT-OK");
    return 0;
}}
"#,
        ca.display()
    )
}

fn server_program(port: u16, ready: &Path) -> String {
    let f = fixtures();
    format!(
        r#"{EXTERNS}
fn main() -> int {{
    let listener = toto_rt_net_listen({port});
    if listener < 0 {{
        println(toto_rt_net_last_error());
        return 1;
    }}
    write_file("{}", "up");
    let conn = toto_rt_net_accept(listener);
    if conn < 0 {{
        println(toto_rt_net_last_error());
        return 1;
    }}
    let cert = read_file("{}");
    let key = read_file("{}");
    let tls = toto_rt_tls_accept(conn, cert, key);
    if tls < 0 {{
        println("NEG-OK");
        return 0;
    }}
    let data = toto_rt_tls_recv(tls, 16);
    if len(data) != 2 || data[0] != 72 || data[1] != 105 {{
        println("BAD-ECHO");
        return 1;
    }}
    if toto_rt_tls_send(tls, data) != 2 {{
        println(toto_rt_tls_last_error());
        return 1;
    }}
    toto_rt_net_close(listener);
    toto_rt_tls_close(tls);
    println("SERVER-OK");
    return 0;
}}
"#,
        ready.display(),
        f.join("server-cert.pem").display(),
        f.join("server-key.pem").display()
    )
}

fn has_marker(stdout: &[u8], marker: &[u8]) -> bool {
    stdout.windows(marker.len()).any(|w| w == marker)
}

fn check_success(outcome: &RtOutcome, marker: &[u8], what: &str) {
    match &outcome.result {
        Ok(0) => {}
        other => panic!("{what}: bad exit: {other:?} stdout: {:?}", outcome.stdout),
    }
    assert!(
        has_marker(&outcome.stdout, marker),
        "{what}: marker missing: {:?}",
        outcome.stdout
    );
}

/// Direction 1: Toto client (executor pool) against `openssl s_server`.
#[test]
fn tls_client_against_openssl_server() {
    let dir = scratch_dir("runtime-client");
    let server = spawn_s_server(48431, &dir);
    let outcome = run_client_until_ready(&client_program(48431, "ca.pem"), "tls client");
    check_success(&outcome, b"CLIENT-OK", "tls client");
    assert_eq!(
        server.captured("s_server"),
        b"Hi",
        "s_server received client bytes"
    );
}

/// Direction 2: Toto server (executor pool) against `openssl s_client`.
#[test]
fn tls_server_against_openssl_client() {
    let dir = scratch_dir("runtime-server");
    let ready = dir.join("ready");
    let _ = std::fs::remove_file(&ready);
    let src = server_program(48432, &ready);
    let (program, checked) = parse_and_check(&src)
        .map_err(|diags| format!("static errors: {diags:?}"))
        .expect("checked");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = run_checked(&program, &checked, RuntimeConfig::default());
        let _ = tx.send(outcome);
    });
    wait_for_ready(&ready, "Toto server");
    let client = spawn_s_client(48432, &fixtures().join("ca.pem"), &dir, "s_client");
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("tls server: Toto program stalled (TLS deadlock?)"));
    check_success(&outcome, b"SERVER-OK", "tls server");
    assert_eq!(client.captured("s_client"), b"Hi", "s_client received echo");
}

/// Negative: a client trusting the wrong CA must fail the handshake
/// (no library wording pinned — the program's own marker decides).
#[test]
fn tls_client_rejects_untrusted_chain() {
    let dir = scratch_dir("runtime-negative");
    let server = spawn_s_server(48433, &dir);
    let outcome = run_client_until_ready(
        &client_program(48433, "other-ca.pem"),
        "tls negative client",
    );
    check_success(&outcome, b"NEG-OK", "tls negative client");
    drop(server);
}
