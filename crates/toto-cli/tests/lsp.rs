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
    // Plain-ASCII sources so UTF-16 columns equal scalar columns.
    // `double(21)` sits at line 2, columns 12-17.
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

fn refs_request(uri: &str, line: u64, character: u64, include_decl: bool) -> String {
    format!(
        "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":{line},\"character\":{character}}},\"context\":{{\"includeDeclaration\":{include_decl}}}}}",
    )
}

#[test]
fn lsp_references_capability_advertised() {
    let mut s = Session::start();
    let init = s.request("initialize", "{\"capabilities\":{}}");
    assert!(init.contains("\"referencesProvider\":true"), "caps: {init}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_references_local_respects_shadowing() {
    // `m` in main (line 1) vs `m` in other (line 7): same name,
    // different bindings. Requests target main's `m` only.
    let src = "fn main() -> int {\n    let m = 1;\n    let n = m + m;\n    println(n);\n    return 0;\n}\nfn other() -> int {\n    let m = 99;\n    return m;\n}\n";
    let uri = "file:///tmp/toto-lsp-refs-local.gzv";
    let mut s = Session::start();
    let _ = s.request("initialize", "{\"capabilities\":{}}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(uri, src));
    // Drain diagnostics (clean file: one empty notification).
    for _ in 0..2 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") {
            break;
        }
    }
    // Use site of main's `m` (line 2, col 12), with declaration.
    let res = s.request("textDocument/references", &refs_request(uri, 2, 12, true));
    assert!(
        res.contains("\"start\":{\"line\":2,\"character\":12}"),
        "use 1: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":2,\"character\":16}"),
        "use 2: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":1,\"character\":8}"),
        "decl: {res}"
    );
    // other()'s `m` (lines 7-8) must not appear.
    assert!(!res.contains("\"line\":7"), "shadow leak: {res}");
    assert!(!res.contains("\"line\":8"), "shadow leak: {res}");
    // Without includeDeclaration: the declaration drops out.
    let res = s.request("textDocument/references", &refs_request(uri, 2, 12, false));
    assert!(
        res.contains("\"start\":{\"line\":2,\"character\":12}"),
        "use 1: {res}"
    );
    assert!(
        !res.contains("\"start\":{\"line\":1,\"character\":8}"),
        "decl excluded: {res}"
    );
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_references_function_cross_file() {
    let dir = std::env::temp_dir().join("toto-lsp-refs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).expect("mkdir");
    std::fs::write(
        dir.join("lib").join("math.gzv"),
        "fn double(n: int) -> int {\n    return n + n;\n}\n",
    )
    .expect("write lib");
    // Two call sites (lines 2-3); declaration lives in lib/math.gzv.
    let main = "import \"lib/math.gzv\";\nfn main() -> int {\n    let a = double(21);\n    println(double(1));\n    return 0;\n}\n";
    let uri = format!("file://{}/main.gzv", dir.to_string_lossy());
    let mut s = Session::start();
    let _ = s.request("initialize", "{\"capabilities\":{}}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(&uri, main));
    for _ in 0..4 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") && msg.contains(&uri) {
            break;
        }
    }
    let res = s.request("textDocument/references", &refs_request(&uri, 2, 14, true));
    assert!(
        res.contains("\"start\":{\"line\":2,\"character\":12}"),
        "call 1: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":3,\"character\":12}"),
        "call 2: {res}"
    );
    assert!(res.contains("lib/math.gzv"), "decl file: {res}");
    assert!(res.contains("\"line\":0"), "decl line: {res}");
    let res = s.request("textDocument/references", &refs_request(&uri, 2, 14, false));
    assert!(!res.contains("lib/math.gzv"), "decl excluded: {res}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lsp_references_struct_field_and_param_decl() {
    let src = "struct Pt { x: int, y: int }\nfn id(v: int) -> int {\n    return v;\n}\nfn main() -> int {\n    let p = Pt { x: 1, y: 2 };\n    println(p.x);\n    return id(1);\n}\n";
    let uri = "file:///tmp/toto-lsp-refs-struct.gzv";
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
    // Struct name at its literal (line 5, col 12): literal site plus
    // declaration (line 0, col 7) when asked.
    let res = s.request("textDocument/references", &refs_request(uri, 5, 12, true));
    assert!(
        res.contains("\"start\":{\"line\":5,\"character\":12}"),
        "lit: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":0,\"character\":7}"),
        "decl: {res}"
    );
    // Field `x` in `p.x` (line 6, col 14): the name span, the
    // literal entry (line 5, col 17), and the field declaration
    // (line 0, col 12) when asked. `y` (decl 0:20, entry 5:23)
    // must not appear.
    let res = s.request("textDocument/references", &refs_request(uri, 6, 14, true));
    assert!(
        res.contains("\"start\":{\"line\":6,\"character\":14}"),
        "use: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":5,\"character\":17}"),
        "lit entry: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":0,\"character\":12}"),
        "field decl: {res}"
    );
    assert!(
        !res.contains("\"line\":0,\"character\":20"),
        "y leak: {res}"
    );
    assert!(
        !res.contains("\"line\":5,\"character\":23"),
        "y leak: {res}"
    );
    // Parameter declaration site (line 1, col 6): declaration plus
    // the use in the body.
    let res = s.request("textDocument/references", &refs_request(uri, 1, 6, true));
    assert!(
        res.contains("\"start\":{\"line\":1,\"character\":6}"),
        "param: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":2,\"character\":11}"),
        "use: {res}"
    );
    // Builtin references are an empty list, not null.
    let res = s.request("textDocument/references", &refs_request(uri, 6, 5, true));
    assert!(res.contains("\"result\":[]"), "builtin: {res}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

fn rename_request(uri: &str, line: u64, character: u64, new_name: &str) -> String {
    format!(
        "{{\"textDocument\":{{\"uri\":\"{uri}\"}},\"position\":{{\"line\":{line},\"character\":{character}}},\"newName\":\"{new_name}\"}}"
    )
}

#[test]
fn lsp_rename_local_edits_decl_and_uses() {
    let src = "fn helper(x: int) -> int {\n    return x;\n}\nfn main() -> int {\n    let r = helper(1);\n    println(r);\n    return 0;\n}\n";
    let uri = "file:///tmp/toto-lsp-rename-local.gzv";
    let mut s = Session::start();
    let init = s.request("initialize", "{\"capabilities\":{}}");
    assert!(init.contains("\"renameProvider\":true"), "caps: {init}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(uri, src));
    for _ in 0..2 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") {
            break;
        }
    }
    // Rename local `r` (use at line 5, col 12) to `total`.
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 12, "total"));
    assert!(res.contains("\"newText\":\"total\""), "edits: {res}");
    assert!(
        res.contains("\"start\":{\"line\":4,\"character\":8}"),
        "decl: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":5,\"character\":12}"),
        "use: {res}"
    );
    assert_eq!(res.matches("\"newText\"").count(), 2, "edits: {res}");
    // Same-name rename succeeds with empty changes.
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 12, "r"));
    assert!(res.contains("\"changes\":{}"), "noop: {res}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_rename_function_cross_file() {
    let dir = std::env::temp_dir().join("toto-lsp-rename");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).expect("mkdir");
    std::fs::write(
        dir.join("lib").join("math.gzv"),
        "fn helper(x: int) -> int {\n    return x + 1;\n}\n",
    )
    .expect("write lib");
    let main = "import \"lib/math.gzv\";\nfn main() -> int {\n    println(helper(41));\n    return helper(1);\n}\n";
    let uri = format!("file://{}/main.gzv", dir.to_string_lossy());
    let mut s = Session::start();
    let _ = s.request("initialize", "{\"capabilities\":{}}");
    s.notify("initialized", "{}");
    s.notify("textDocument/didOpen", &did_open(&uri, main));
    for _ in 0..4 {
        let msg = s.recv();
        if msg.contains("publishDiagnostics") && msg.contains(&uri) {
            break;
        }
    }
    // Rename `helper` (call at line 2, col 14) to `assist`.
    let res = s.request(
        "textDocument/rename",
        &rename_request(&uri, 2, 14, "assist"),
    );
    assert!(res.contains("\"newText\":\"assist\""), "edits: {res}");
    assert_eq!(res.matches("\"newText\"").count(), 3, "edits: {res}");
    assert!(res.contains("lib/math.gzv"), "lib edit: {res}");
    assert!(
        res.contains("\"start\":{\"line\":0,\"character\":3}"),
        "decl: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":2,\"character\":12}"),
        "call 1: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":3,\"character\":11}"),
        "call 2: {res}"
    );
    assert!(
        !res.contains("\"start\":{\"line\":3,\"character\":12}"),
        "off by one: {res}"
    );
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lsp_rename_field_uses_name_spans() {
    let src = "struct Pt { x: int, y: int }\nfn main() -> int {\n    let p = Pt { x: 1, y: 2 };\n    println(p.x);\n    return 0;\n}\n";
    let uri = "file:///tmp/toto-lsp-rename-field.gzv";
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
    // Rename field `x` (use at line 3, col 14) to `ex`: the use edit
    // covers exactly the name, not the whole `p.x`.
    let res = s.request("textDocument/rename", &rename_request(uri, 3, 14, "ex"));
    assert!(
        res.contains(
            "\"start\":{\"line\":3,\"character\":14},\"end\":{\"line\":3,\"character\":15}"
        ),
        "use: {res}"
    );
    assert!(
        res.contains(
            "\"start\":{\"line\":2,\"character\":17},\"end\":{\"line\":2,\"character\":18}"
        ),
        "lit entry: {res}"
    );
    assert!(
        res.contains(
            "\"start\":{\"line\":0,\"character\":12},\"end\":{\"line\":0,\"character\":13}"
        ),
        "decl: {res}"
    );
    assert_eq!(res.matches("\"newText\":\"ex\"").count(), 3, "edits: {res}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_rename_refusals() {
    let src = "fn helper() -> int {\n    return 1;\n}\nfn main() -> int {\n    let assist = 2;\n    println(helper());\n    return assist;\n}\n";
    let uri = "file:///tmp/toto-lsp-rename-refuse.gzv";
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
    // Builtin target (`println` call, line 5).
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 5, "out"));
    assert!(res.contains("\"code\":-32803"), "builtin: {res}");
    // Duplicate top-level name (`helper` call, line 5).
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 14, "main"));
    assert!(res.contains("\"code\":-32803"), "dup: {res}");
    assert!(res.contains("already declared"), "dup: {res}");
    // Capture: `assist` is bound as a local in the edited file.
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 14, "assist"));
    assert!(res.contains("\"code\":-32803"), "capture: {res}");
    // Invalid identifier and keyword.
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 14, "123abc"));
    assert!(res.contains("\"code\":-32602"), "invalid: {res}");
    let res = s.request("textDocument/rename", &rename_request(uri, 5, 14, "fn"));
    assert!(res.contains("\"code\":-32602"), "keyword: {res}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_rename_local_shadowing_capture_refused() {
    // Renaming `x` to `g` would capture the global use in `println`.
    let src = "fn g() -> int {\n    return 1;\n}\nfn main() -> int {\n    let x = 1;\n    println(g());\n    return x;\n}\n";
    let uri = "file:///tmp/toto-lsp-rename-capture.gzv";
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
    let res = s.request("textDocument/rename", &rename_request(uri, 4, 8, "g"));
    assert!(res.contains("\"code\":-32803"), "capture: {res}");
    assert!(res.contains("used as a global"), "capture: {res}");
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
}

#[test]
fn lsp_positions_are_utf16() {
    // 𝕩 is 1 scalar but 2 UTF-16 units: the `e` use sits at UTF-16
    // column 26 (scalar 25). A scalar server would resolve nothing.
    let src = "fn main() -> int {\n    let e = \"\u{1d565}\"; println(e);\n    return 0;\n}\n";
    let uri = "file:///tmp/toto-lsp-utf16.gzv";
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
    let res = s.request("textDocument/references", &refs_request(uri, 1, 26, true));
    assert!(
        res.contains("\"start\":{\"line\":1,\"character\":26}"),
        "use: {res}"
    );
    assert!(
        res.contains("\"start\":{\"line\":1,\"character\":8}"),
        "decl: {res}"
    );
    let status = s.shutdown();
    assert!(status.success(), "lsp exit: {status:?}");
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
