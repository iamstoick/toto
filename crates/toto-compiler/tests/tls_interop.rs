//! TLS interop against the reference (Enterprise Wave 2, Stage 1):
//! sequential Toto programs through the tree-walking interpreter
//! (`ffi.rs` arms) with `openssl s_server` / `openssl s_client` as
//! the peer — both directions, plus the untrusted-chain negative.
//!
//! Fixtures are pinned (`tests/corpus/project/proj15_tls_echo/
//! fixtures/`, generated once, verified with `openssl verify` at
//! generation). No library wording is pinned: positives assert the
//! program's own markers plus exact relayed bytes; negatives assert
//! the program's own markers, never OpenSSL text (which differs
//! between OpenSSL and LibreSSL).
//!
//! Mechanics (learned the hard way, recorded here):
//! - Both openssl ends close the connection when their stdin hits
//!   EOF, which wins the race against the relay — so each peer's
//!   stdin is a held-open pipe, fed its bytes up front.
//! - Captures are pipes drained by a reader thread (never files: a
//!   file capture needs a clean exit to flush stdio buffers, and a
//!   bare-TCP readiness probe consumes `s_server -naccept 1`'s
//!   single slot — verified empirically — so no `-naccept` and no
//!   probes into TLS ports).
//! - Readiness without probes: the Toto client retries on
//!   connection-refused (nothing listening consumes no server
//!   state); the Toto server writes a ready file after `listen`
//!   (a probe `accept` would consume its single-shot server).
//!
//! A watchdog turns any stall into a loud failure naming the test
//! instead of hanging the suite.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

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

/// Run a sequential Toto program through the interpreter with a
/// watchdog (a TLS stall must fail loudly, never hang the suite).
fn run_program(src: &str, what: &str) -> (i32, Vec<u8>) {
    let src = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = toto_compiler::run_source(&src, "tls_interop.gzv");
        let _ = tx.send(outcome);
    });
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("{what}: Toto program stalled (TLS deadlock?)"));
    match outcome {
        toto_compiler::Outcome::Success { code, stdout } => (code, stdout),
        toto_compiler::Outcome::Static(diags) => panic!("{what}: static errors: {diags:?}"),
        toto_compiler::Outcome::RuntimePanic { diag, .. } => {
            panic!("{what}: runtime panic: {} {}", diag.code, diag.message)
        }
    }
}

/// Run the client until the server is up: connection-refused consumes
/// no server state, so retrying is exact (unlike a TCP probe, which
/// would consume a single-shot accept).
fn run_client_until_ready(src: &str, what: &str) -> (i32, Vec<u8>) {
    let start = Instant::now();
    loop {
        let (code, out) = run_program(src, what);
        let text = String::from_utf8_lossy(&out);
        if code == 1 && text.contains("refused") && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        return (code, out);
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

/// Direction 1: Toto client (interpreter) against `openssl s_server`.
/// Both record directions verified: the client reads the server's
/// bytes, and the server's capture holds the client's bytes.
#[test]
fn tls_client_against_openssl_server() {
    let dir = scratch_dir("compiler-client");
    let server = spawn_s_server(48421, &dir);
    let (code, stdout) = run_client_until_ready(&client_program(48421, "ca.pem"), "tls client");
    assert_eq!(code, 0, "client exit");
    assert!(
        has_marker(&stdout, b"CLIENT-OK"),
        "client marker: {stdout:?}"
    );
    assert_eq!(
        server.captured("s_server"),
        b"Hi",
        "s_server received client bytes"
    );
}

/// Direction 2: Toto server (interpreter) against `openssl s_client`.
/// The client verifies the chain (`-verify_return_error` fails loudly
/// on a bad chain); the echo proves both record directions.
#[test]
fn tls_server_against_openssl_client() {
    let dir = scratch_dir("compiler-server");
    let ready = dir.join("ready");
    let _ = std::fs::remove_file(&ready);
    let src = server_program(48422, &ready);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = toto_compiler::run_source(&src, "tls_interop.gzv");
        let _ = tx.send(outcome);
    });
    wait_for_ready(&ready, "Toto server");
    let client = spawn_s_client(48422, &fixtures().join("ca.pem"), &dir, "s_client");
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("tls server: Toto program stalled (TLS deadlock?)"));
    let (code, stdout) = match outcome {
        toto_compiler::Outcome::Success { code, stdout } => (code, stdout),
        toto_compiler::Outcome::Static(diags) => panic!("static errors: {diags:?}"),
        toto_compiler::Outcome::RuntimePanic { diag, .. } => {
            panic!("runtime panic: {} {}", diag.code, diag.message)
        }
    };
    assert_eq!(code, 0, "server exit");
    assert!(
        has_marker(&stdout, b"SERVER-OK"),
        "server marker: {stdout:?}"
    );
    assert_eq!(client.captured("s_client"), b"Hi", "s_client received echo");
}

/// Negative: a client trusting the wrong CA must fail the handshake
/// (no library wording pinned — the program's own marker decides).
#[test]
fn tls_client_rejects_untrusted_chain() {
    let dir = scratch_dir("compiler-negative");
    let server = spawn_s_server(48423, &dir);
    let (code, stdout) = run_client_until_ready(
        &client_program(48423, "other-ca.pem"),
        "tls negative client",
    );
    assert_eq!(code, 0, "negative client exit");
    assert!(
        has_marker(&stdout, b"NEG-OK"),
        "negative marker: {stdout:?}"
    );
    drop(server);
}
