//! HTTP interop against external references (Enterprise Wave 2,
//! Stage 2): the real `toto` binary serving the real `gzv.std/http`
//! framework, with `curl(1)` as the client oracle both ways over
//! plain and TLS loopback, plus the Toto client against a minimal
//! Rust reference server (itself sanity-checked by curl) and
//! against `openssl s_server` for HTTPS.
//!
//! What each direction proves (curl is the independent oracle;
//! Toto-vs-Toto conformance lives in `proj16_http`):
//! - our server vs curl (plain): request parsing, routing, keep-alive
//!   reuse, 404s — all asserted on curl's exit code and bytes.
//! - our server vs curl (https, `--cacert` pinning the test CA):
//!   the Stage 1 TLS upgrade under a real handshake, hostname
//!   `localhost` verified by curl itself; wrong-CA curl must fail.
//! - our client vs reference (plain): status/body passthrough
//!   including a 501, against a peer curl also drives.
//! - our client vs `openssl s_server` (https): the client upgrade
//!   path against the reference TLS stack.
//!
//! Mechanics: temp projects under `target/http-interop/<test>/`
//! (fixtures copied from the pinned `proj16_http` set); servers
//! write a ready file after `listen` (a TCP probe would consume a
//! single-shot accept) and are killed after assertions; every wait
//! has a watchdog that names the test instead of hanging the suite.
//! Ports 48521-48524 are unique to this file (the corpus project
//! uses 48511/48512/48518/48519; the TLS suites use 4842x).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn toto_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_toto"))
}

fn corpus_fixtures() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("corpus")
        .join("project")
        .join("proj16_http")
        .join("fixtures")
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = workspace_root()
        .join("target")
        .join("http-interop")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::create_dir_all(dir.join("fixtures")).expect("fixtures dir");
    for f in [
        "ca.pem",
        "server-cert.pem",
        "server-key.pem",
        "other-ca.pem",
    ] {
        std::fs::copy(corpus_fixtures().join(f), dir.join("fixtures").join(f))
            .expect("fixture copy");
    }
    dir
}

fn write_main(dir: &Path, src: &str) {
    std::fs::write(dir.join("main.gzv"), src).expect("main.gzv");
}

/// A running `toto run` child: stdout drained by a reader thread
/// (servers print nothing, but a full pipe would wedge them);
/// stderr to a file; killed on drop.
struct TotoChild {
    child: Child,
    out: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl TotoChild {
    fn spawn(dir: &Path, file: &str, stderr_name: &str) -> TotoChild {
        let err = std::fs::File::create(dir.join(stderr_name)).expect("stderr file");
        let mut child = Command::new(toto_bin())
            .arg("run")
            .arg(file)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("toto run (needed for HTTP interop)");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut src = stdout;
            let _ = src.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        TotoChild { child, out: rx }
    }

    fn take_output(mut self) -> Vec<u8> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.out
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default()
    }
}

impl Drop for TotoChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_ready(path: &Path, what: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if path.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{what}: server never signalled readiness");
}

/// Run a one-shot Toto program to completion with a watchdog.
fn run_toto_to_end(dir: &Path, file: &str, what: &str) -> (i32, String) {
    let dir = dir.to_path_buf();
    let file = file.to_string();
    let what = what.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let output = Command::new(toto_bin())
            .arg("run")
            .arg(&file)
            .current_dir(&dir)
            .output()
            .expect("toto run");
        let _ = tx.send(output);
    });
    let output = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("{what}: Toto program stalled (watchdog)"));
    let code = output.status.code().unwrap_or(-999);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    (code, stdout)
}

/// Run curl, retrying connection-refused (nothing listening consumes
/// no server state). Any other failure returns immediately.
fn curl_until_ready(args: &[&str], _what: &str) -> (i32, Vec<u8>) {
    let start = Instant::now();
    loop {
        let output = Command::new("curl")
            .args(args)
            .output()
            .expect("curl (needed for HTTP interop)");
        if output.status.code() == Some(7) && start.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let code = output.status.code().unwrap_or(-999);
        return (code, output.stdout);
    }
}

fn curl_once(args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let output = Command::new("curl").args(args).output().expect("curl");
    (
        output.status.code().unwrap_or(-999),
        output.stdout,
        output.stderr,
    )
}

const SERVER_PROGRAM: &str = r#"
import "gzv.std/http.gzv";

fn h_hello(req: HttpRequest) -> Result<HttpResponse, string> {
    return http_text(200, "OK", "curl-hi");
}

fn h_echo(req: HttpRequest) -> Result<HttpResponse, string> {
    return Ok(http_response(200, "OK", [], req.body));
}

fn h_secure(req: HttpRequest) -> Result<HttpResponse, string> {
    return http_text(200, "OK", "curl-secure");
}

fn doit() -> Result<string, string> {
    let router = http_router();
    route_add(router, "GET", "/hello", h_hello)?;
    route_add(router, "POST", "/echo", h_echo)?;
    route_add(router, "GET", "/secure", h_secure)?;
    let opts = HttpServeOpts { expose_errors: false, max_body: 65536, keepalive_ms: 0, server_name: "interop" };
    let listener = http_serve(PORT, router, opts)?;
    write_file("ready", "up");
    let stop = chan<int>(1);
    let parked = <-stop;
    if parked == 0 {
        net_close(listener)?;
    } else {
    }
    return Ok("stopped");
}

fn main() -> int {
    println(doit());
    return 0;
}
"#;

const SERVER_TLS_PROGRAM: &str = r#"
import "gzv.std/http.gzv";

fn h_secure(req: HttpRequest) -> Result<HttpResponse, string> {
    return http_text(200, "OK", "curl-secure");
}

fn doit() -> Result<string, string> {
    let router = http_router();
    route_add(router, "GET", "/secure", h_secure)?;
    let opts = HttpServeOpts { expose_errors: false, max_body: 65536, keepalive_ms: 0, server_name: "interop" };
    let cert = read_file("fixtures/server-cert.pem");
    let key = read_file("fixtures/server-key.pem");
    let listener = http_serve_tls(PORT, router, opts, cert, key)?;
    write_file("ready", "up");
    let stop = chan<int>(1);
    let parked = <-stop;
    if parked == 0 {
        net_close(listener)?;
    } else {
    }
    return Ok("stopped");
}

fn main() -> int {
    println(doit());
    return 0;
}
"#;

/// Direction 1a: our plain server vs curl — bodies, echo, keep-alive
/// reuse across two URLs, and the 404 path.
#[test]
fn http_server_against_curl() {
    let dir = scratch_dir("server-curl");
    let port = 48521;
    write_main(
        &dir,
        &SERVER_PROGRAM.replace("http_serve(PORT", &format!("http_serve({port}")),
    );
    let server = TotoChild::spawn(&dir, "main.gzv", "toto.err");
    wait_for_ready(&dir.join("ready"), "plain server");
    let base = format!("http://localhost:{port}");
    let (code, body) = curl_until_ready(&["-sS", &format!("{base}/hello")], "curl hello");
    assert_eq!(code, 0, "curl hello exit");
    assert_eq!(body, b"curl-hi", "hello body");
    // Two URLs over one connection: keep-alive reuse server-side.
    let (code, both, _) = curl_once(&["-sS", &format!("{base}/hello"), &format!("{base}/hello")]);
    assert_eq!(code, 0, "curl keep-alive exit");
    assert_eq!(both, b"curl-hicurl-hi", "keep-alive bodies");
    // Binary echo through a real client.
    let (code, echo, _) = curl_once(&[
        "-sS",
        "-X",
        "POST",
        "--data-binary",
        "curl-data",
        &format!("{base}/echo"),
    ]);
    assert_eq!(code, 0, "curl echo exit");
    assert_eq!(echo, b"curl-data", "echo body");
    // Unknown route: exact 404 status.
    let (code, status, _) = curl_once(&[
        "-sS",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        &format!("{base}/nope"),
    ]);
    assert_eq!(code, 0, "curl 404 exit");
    assert_eq!(status, b"404", "404 status");
    // The framework itself stays silent: every byte the client saw
    // came from curl assertions above, not server chatter.
    let out = server.take_output();
    assert!(out.is_empty(), "server stdout: {out:?}");
}

/// Direction 1b: our TLS server vs curl — chain verified by curl
/// itself (`--cacert` pins the test CA, hostname `localhost` is
/// covered by the fixture SAN); wrong-CA curl must fail.
#[test]
fn http_tls_server_against_curl() {
    let dir = scratch_dir("tls-server-curl");
    let port = 48522;
    write_main(
        &dir,
        &SERVER_TLS_PROGRAM.replace("http_serve_tls(PORT", &format!("http_serve_tls({port}")),
    );
    let server = TotoChild::spawn(&dir, "main.gzv", "toto.err");
    wait_for_ready(&dir.join("ready"), "tls server");
    let ca = dir.join("fixtures").join("ca.pem");
    let other_ca = dir.join("fixtures").join("other-ca.pem");
    let url = format!("https://localhost:{port}/secure");
    let (code, body) = curl_until_ready(
        &["-sS", "--cacert", ca.to_str().unwrap(), &url],
        "curl https",
    );
    assert_eq!(code, 0, "curl https exit");
    assert_eq!(body, b"curl-secure", "https body");
    let (bad_code, _, _) = curl_once(&["-sS", "--cacert", other_ca.to_str().unwrap(), &url]);
    assert_ne!(bad_code, 0, "wrong-CA curl must fail");
    let out = server.take_output();
    assert!(out.is_empty(), "server stdout: {out:?}");
}

/// Minimal reference server (plain HTTP, Rust std only): GET answers
/// 200 with a fixed body, anything else answers 501. Curl drives it
/// first (proving it speaks real HTTP), then the Toto client does.
/// Detached like the Stage 1 watchdog threads: the suite process
/// exit ends it; the bounded connection count keeps it finite.
fn spawn_reference_server(port: u16, dir: &Path, tag: &str) {
    let ready = dir.join(format!("ref-{tag}.ready"));
    let _ = std::fs::remove_file(&ready);
    std::thread::spawn(move || {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("reference bind");
        std::fs::write(&ready, "up").expect("reference ready");
        for stream in listener.incoming().take(8) {
            let mut stream = stream.expect("reference accept");
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let is_get = head.starts_with("GET ");
                let (status, body) = if is_get {
                    ("200 OK", "ref-hello")
                } else {
                    ("501 Not Implemented", "ref-nope")
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    wait_for_ready(&dir.join(format!("ref-{tag}.ready")), "reference server");
}

const CLIENT_PROGRAM: &str = r#"
import "gzv.std/http.gzv";

fn doit() -> Result<string, string> {
    let copts = HttpClientOpts { max_body: 65536, redirects: 0, host_allow: [] };
    let r = http_get("__URL_GET__", copts)?;
    let t = http_bytes_to_text(r.body)?;
    return Ok("ref:" + to_string(r.status) + ":" + t);
}

fn postit() -> Result<string, string> {
    let copts = HttpClientOpts { max_body: 65536, redirects: 0, host_allow: [] };
    let hs: Array<HttpHeader> = [];
    let b: Array<int> = [1, 2, 3];
    let r = http_post("__URL_POST__", hs, b, copts)?;
    return Ok("post:" + to_string(r.status));
}

fn main() -> int {
    println(doit());
    println(postit());
    return 0;
}
"#;

/// Direction 2a: curl sanity-checks the Rust reference, then the
/// Toto client drives it (GET 200 passthrough, POST 501 status
/// passthrough).
#[test]
fn http_client_against_reference() {
    let dir = scratch_dir("client-reference");
    let port = 48523;
    spawn_reference_server(port, &dir, "plain");
    // The reference speaks real HTTP/1.1 (curl is the oracle).
    let (code, body) = curl_until_ready(
        &["-sS", &format!("http://localhost:{port}/hello")],
        "curl reference",
    );
    assert_eq!(code, 0, "curl reference exit");
    assert_eq!(body, b"ref-hello", "reference body");
    // The Toto client against the same reference.
    let src = CLIENT_PROGRAM
        .replace("__URL_GET__", &format!("http://localhost:{port}/hello"))
        .replace("__URL_POST__", &format!("http://localhost:{port}/hello"));
    write_main(&dir, &src);
    let (code, out) = run_toto_to_end(&dir, "main.gzv", "toto client");
    assert_eq!(code, 0, "toto client exit");
    assert!(
        out.contains("ref:200:ref-hello"),
        "client GET markers: {out:?}"
    );
    assert!(out.contains("post:501"), "client POST status: {out:?}");
}

/// Direction 2b: the Toto HTTPS client against `openssl s_server`
/// with a canned HTTP response on stdin.
#[test]
fn http_client_against_openssl_server() {
    let dir = scratch_dir("client-s-server");
    let port = 48524;
    let f = corpus_fixtures();
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\ns-serv-hi!";
    // Held-open stdin peer (both openssl ends die on stdin EOF, so
    // the response is fed up front and stdin stays open).
    let mut peer = Command::new("openssl")
        .args([
            "s_server",
            "-accept",
            &port.to_string(),
            "-cert",
            f.join("server-cert.pem").to_str().unwrap(),
            "-key",
            f.join("server-key.pem").to_str().unwrap(),
            "-quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(dir.join("s_server.err")).unwrap(),
        ))
        .spawn()
        .expect("openssl s_server (needed for HTTPS interop)");
    peer.stdin
        .take()
        .expect("peer stdin")
        .write_all(response)
        .expect("peer stdin write");
    let src = format!(
        r#"
import "gzv.std/http.gzv";

fn doit() -> Result<string, string> {{
    let copts = HttpClientOpts {{ max_body: 65536, redirects: 0, host_allow: [] }};
    let ca = read_file("fixtures/ca.pem");
    let r = http_do(HttpClientReq {{ method: "GET", url: "https://localhost:{port}/x", headers: [], body: [], ca_pem: ca }}, copts)?;
    let t = http_bytes_to_text(r.body)?;
    return Ok("sserv:" + to_string(r.status) + ":" + t);
}}

fn main() -> int {{
    println(doit());
    return 0;
}}
"#
    );
    write_main(&dir, &src);
    // Retry while the listener comes up (refused consumes no state).
    let (code, out) = {
        let start = Instant::now();
        loop {
            let (code, stdout) = run_toto_to_end(&dir, "main.gzv", "toto https client");
            if code == 0 {
                break (code, stdout);
            }
            if stdout.contains("refused") && start.elapsed() < Duration::from_secs(15) {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            panic!("https client failed: exit={code} out={stdout:?}");
        }
    };
    assert_eq!(code, 0, "https client exit");
    assert!(
        out.contains("sserv:200:s-serv-hi!"),
        "https markers: {out:?}"
    );
    let _ = peer.kill();
    let _ = peer.wait();
}
