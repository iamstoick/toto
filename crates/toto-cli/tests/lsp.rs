//! LSP protocol test (Phase 5, Stage 4): drive `toto lsp` over
//! piped stdio and assert hover, go-to-definition, and diagnostics.
//! Headless/protocol-only verification (no real editor run); see
//! `docs/lsp.md` for the disclosed verification status.
//!
//! The helper speaks Content-Length framing in both directions.

use std::io::{Read, Write};
use std::process::Stdio;

struct Session {
    child: std::process::Child,
    buf: Vec<u8>,
    next_id: i64,
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

impl Session {
    fn start() -> Session {
        let toto = env!("CARGO_BIN_EXE_toto");
        let child = std::process::Command::new(toto)
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn toto lsp");
        Session {
            child,
            buf: Vec::new(),
            next_id: 1,
        }
    }

    fn send(&mut self, body: &str) {
        self.child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(&frame(body.as_bytes()))
            .expect("write request");
    }

    fn recv(&mut self) -> String {
        let stdout = self.child.stdout.as_mut().expect("stdout");
        // Read headers line by line.
        let mut len: Option<usize> = None;
        loop {
            let mut line = Vec::new();
            loop {
                let mut b = [0u8; 1];
                stdout.read_exact(&mut b).expect("read header byte");
                line.push(b[0]);
                if line.ends_with(b"\r\n") {
                    break;
                }
            }
            let line = String::from_utf8(line).expect("header utf8");
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                len = Some(rest.trim().parse().expect("content length"));
            }
        }
        let mut body = vec![0u8; len.expect("length header")];
        stdout.read_exact(&mut body).expect("read body");
        String::from_utf8(body).expect("body utf8")
    }

    fn request(&mut self, method: &str, params: &str) -> String {
        eprintln!("TEST >>> {method}");
        let id = self.next_id;
        self.next_id += 1;
        // Skip any pending notifications first (diagnostics arrive
        // before responses).
        self.send(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}"
        ));
        loop {
            let msg = self.recv();
            if msg.contains("\"method\"") {
                self.buf.extend_from_slice(msg.as_bytes());
            } else {
                assert!(msg.contains(&format!("\"id\":{id}")), "wrong id: {msg}");
                return msg;
            }
        }
    }

    fn notify(&mut self, method: &str, params: &str) {
        self.send(&format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{params}}}"
        ));
    }

    fn shutdown(&mut self) -> std::process::ExitStatus {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"shutdown\",\"params\":{{}}}}"
        ));
        let _ = self.recv();
        self.notify("exit", "{}");
        self.child.wait().expect("wait")
    }
}

// The tests below string-match JSON fragments instead of parsing:
// the server is dependency-free on purpose, and so is this driver.

fn did_open(uri: &str, text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("{{\"textDocument\":{{\"uri\":\"{uri}\",\"text\":\"{escaped}\"}}}}")
}

#[test]
fn lsp_hover_definition_diagnostics() {
    let dir = std::env::temp_dir().join("toto-lsp-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).expect("mkdir");
    std::fs::write(
        dir.join("lib").join("math.gzv"),
        "fn double(n: int) -> int {\n    return n + n;\n}\n",
    )
    .expect("write lib");
    // Plain-ASCII sources so scalar columns are exact. `double(21)`
    // sits at line 2, columns 12-17; the use of `m` at line 3:11.
    let main = "import \"lib/math.gzv\";\nfn main() -> int {\n    let m = double(21);\n    println(m + bad);\n    return 0;\n}\n";
    let _main_path = dir.join("main.gzv");
    let uri = format!("file://{}/main.gzv", dir.to_string_lossy());

    let mut s = Session::start();
    let init = s.request("initialize", "{\"capabilities\":{}}");
    assert!(init.contains("\"hoverProvider\":true"), "caps: {init}");
    assert!(init.contains("\"definitionProvider\":true"), "caps: {init}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(&uri, main));

    // Diagnostics: `bad` is E0301 at line 3 (0-based), columns 16-19.
    let mut seen_e0301 = false;
    for _ in 0..4 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") && msg.contains(&uri) {
            assert!(msg.contains("\"code\":\"E0301\""), "diag: {msg}");
            assert!(msg.contains("\"line\":3"), "diag: {msg}");
            seen_e0301 = true;
            break;
        }
    }
    assert!(seen_e0301, "no E0301 diagnostic published");

    // Hover on `double` (line 2, col 13): cross-file signature.
    let hover = s.request(
        "textDocument/hover",
        &format!("{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":2,\"character\":13}}}}"),
    );
    assert!(hover.contains("fn double(n: int) -> int"), "hover: {hover}");

    // Definition on `double`: jumps into lib/math.gzv line 0.
    let def = s.request(
        "textDocument/definition",
        &format!("{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":2,\"character\":13}}}}"),
    );
    assert!(def.contains("lib/math.gzv"), "def: {def}");
    assert!(def.contains("\"line\":0"), "def: {def}");

    // Builtin hover on `println` (line 3, col 5).
    let hover = s.request(
        "textDocument/hover",
        &format!(
            "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":3,\"character\":5}}}}"
        ),
    );
    assert!(hover.contains("println"), "builtin hover: {hover}");

    // Builtin definition is null (nothing to jump to).
    let def = s.request(
        "textDocument/definition",
        &format!(
            "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":3,\"character\":5}}}}"
        ),
    );
    assert!(def.contains("\"result\":null"), "builtin def: {def}");

    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lsp_local_types_and_broken_refusal() {
    // A clean file: local uses carry checked types; a broken file
    // publishes diagnostics and answers null hovers.
    let src = "fn main() -> int {\n    let x = 1 + 2;\n    println(x);\n    return 0;\n}\n";
    let uri = "file:///tmp/toto-lsp-clean.gzv";
    let mut s = Session::start();
    let _ = s.request("initialize", "{\"capabilities\":{}}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(uri, src));
    // Drain the (empty) diagnostics notification.
    let mut drained = false;
    for _ in 0..2 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") {
            drained = true;
            break;
        }
    }
    assert!(drained, "no diagnostics published");

    // Use of `x` at line 2, col 12: checked type int.
    let hover = s.request(
        "textDocument/hover",
        &format!("{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":2,\"character\":12}}}}"),
    );
    assert!(hover.contains("x: int"), "local hover: {hover}");

    // Declaration of `x` at line 1, col 8: `let x`.
    let hover = s.request(
        "textDocument/hover",
        &format!(
            "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":1,\"character\":8}}}}"
        ),
    );
    assert!(hover.contains("let x"), "decl hover: {hover}");

    // Definition of the use jumps back to the declaration line.
    let def = s.request(
        "textDocument/definition",
        &format!("{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":2,\"character\":12}}}}"),
    );
    assert!(def.contains("\"line\":1"), "local def: {def}");
    assert!(def.contains("\"character\":8"), "local def: {def}");

    // Break the file via didChange: diagnostics carry E0101, and
    // hover outside any name stays null.
    s.notify(
        "textDocument/didChange",
        &format!(
            "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"contentChanges\":[{{\"text\":\"fn main( -> int {{{{\"}}]}}"
        ),
    );
    let mut seen_err = false;
    for _ in 0..2 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") && msg.contains("\"code\":\"E") {
            seen_err = true;
            break;
        }
    }
    assert!(seen_err, "no error diagnostic after break");
    let hover = s.request(
        "textDocument/hover",
        &format!(
            "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":0,\"character\":0}}}}"
        ),
    );
    assert!(hover.contains("\"result\":null"), "broken hover: {hover}");

    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_malformed_frame_is_skipped() {
    // A garbage frame must not kill the session: the server skips
    // it and keeps answering (regression test for the silent-exit
    // EOF once caused by a malformed didChange frame).
    let src = "fn main() -> int {\n    return 0;\n}\n";
    let uri = "file:///tmp/toto-lsp-garbage.gzv";
    let mut s = Session::start();
    let _ = s.request("initialize", "{\"capabilities\":{}}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(uri, src));
    for _ in 0..2 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") {
            break;
        }
    }
    // Framing-valid but JSON-invalid body.
    s.send("{\"jsonrpc\":\"oops\"}");
    s.send("not json at all!");
    // The session survives: hover still answers.
    let hover = s.request(
        "textDocument/hover",
        &format!(
            "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":0,\"character\":4}}}}"
        ),
    );
    assert!(hover.contains("main"), "hover after garbage: {hover}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}
