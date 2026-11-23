//! Wave 3 F1 regression: `tlslib` peer waits must be bounded.
//!
//! The unbounded waits deadlocked the shared blocking pool: a job
//! that blocks on a peer action while the job producing that action
//! sits queued behind it wedges its thread forever (shutdown wait
//! behind a queued peer close; handshake waits behind queued peer
//! handshakes under burst). Every peer wait now carries a 5 s bound
//! plus generous CI slack: a stall fails loudly instead of hanging.
//! Against the pre-fix code these tests hang forever; post-fix each
//! returns in ~5 s.
#![cfg(unix)]

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
        .join("project")
        .join("proj15_tls_echo")
        .join("fixtures")
}

#[test]
fn shutdown_close_bounded_against_silent_peer() {
    let dir = fixtures();
    let ca = std::fs::read_to_string(dir.join("ca.pem")).expect("ca fixture");
    let cert = std::fs::read_to_string(dir.join("server-cert.pem")).expect("cert fixture");
    let key = std::fs::read_to_string(dir.join("server-key.pem")).expect("key fixture");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();

    // Client: connects, completes the handshake, then goes silent —
    // the fd stays open (forgotten, closed at the end) and no
    // close_notify is ever sent.
    let client = std::thread::spawn(move || {
        let stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let fd: RawFd = stream.as_raw_fd();
        std::mem::forget(stream);
        toto_compiler::tlslib::client_connect(fd, &ca).expect("client handshake");
        fd
    });

    let (server_stream, _) = listener.accept().expect("accept");
    let server_fd: RawFd = server_stream.as_raw_fd();
    std::mem::forget(server_stream);
    toto_compiler::tlslib::server_accept(server_fd, &cert, &key).expect("server handshake");

    let client_fd: RawFd = client.join().expect("client thread");

    // The peer never closes: pre-fix code blocks here forever.
    let start = Instant::now();
    toto_compiler::tlslib::shutdown_close(server_fd).expect("shutdown proceeds");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "shutdown_close took {elapsed:?} against a silent peer (bound is 5 s)"
    );

    // Cleanup: plain-close the silent peer and drop its session.
    toto_compiler::netsys::close(client_fd).ok();
    toto_compiler::tlslib::free_session(client_fd);
}

#[test]
fn handshake_server_bounded_against_silent_peer() {
    let dir = fixtures();
    let cert = std::fs::read_to_string(dir.join("server-cert.pem")).expect("cert fixture");
    let key = std::fs::read_to_string(dir.join("server-key.pem")).expect("key fixture");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();

    // Client connects at TCP level and then goes silent — no TLS
    // flight ever arrives.
    let stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let client_fd: RawFd = stream.as_raw_fd();
    std::mem::forget(stream);

    let (server_stream, _) = listener.accept().expect("accept");
    let server_fd: RawFd = server_stream.as_raw_fd();
    std::mem::forget(server_stream);

    // The peer never speaks: pre-fix code blocks here forever.
    let start = Instant::now();
    let err = toto_compiler::tlslib::server_accept(server_fd, &cert, &key)
        .expect_err("silent peer must fail loudly, not hang");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "server handshake took {elapsed:?} against a silent peer (bound is 5 s)"
    );
    assert!(
        err.contains("timed out after 5000 ms waiting for peer flight"),
        "loud timeout, not library text: {err}"
    );

    // Cleanup: both fds were handshaked-never; no sessions exist.
    toto_compiler::netsys::close(client_fd).ok();
    toto_compiler::netsys::close(server_fd).ok();
}

#[test]
fn handshake_client_bounded_against_silent_peer() {
    // A raw TCP acceptor that never speaks TLS: the client's
    // handshake has no peer flight to drive it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    // Detached holder: keeps the accepted socket open and silent
    // past the test (the harness does not wait for stray threads).
    let _holder = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        std::thread::sleep(Duration::from_secs(60));
        drop(stream);
    });

    let dir = fixtures();
    let ca = std::fs::read_to_string(dir.join("ca.pem")).expect("ca fixture");
    let stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let fd: RawFd = stream.as_raw_fd();
    std::mem::forget(stream);

    // The peer never speaks: pre-fix code blocks here forever.
    let start = Instant::now();
    let err = toto_compiler::tlslib::client_connect(fd, &ca)
        .expect_err("silent peer must fail loudly, not hang");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "client handshake took {elapsed:?} against a silent peer (bound is 5 s)"
    );
    assert!(
        err.contains("timed out after 5000 ms waiting for peer flight"),
        "loud timeout, not library text: {err}"
    );

    toto_compiler::netsys::close(fd).ok();
}
