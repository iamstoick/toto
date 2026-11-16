//! `toto build` and `toto test` (Phase 5, Stage 1).
//!
//! `toto build [--release]` compiles the project to a standalone
//! executable: load + check the merged project, lower to MIR, emit a
//! Cranelift object, generate a shim crate that links the object
//! against the `toto-jit` runtime helpers, and drive the link with a
//! nested `cargo build`. The binary embeds its sources and re-resolves
//! them at startup (structs, display names, panic rendering); only
//! machine code is ahead-of-time.
//!
//! `toto test` runs every `tests/*.gzv` through the same load + check
//! + execute path as `toto run` and reports pass/fail per test.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::manifest::{Manifest, find_root, read_manifest};

/// Resolve the project root from the working directory.
fn project_root() -> Result<(PathBuf, Manifest), ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let manifest_path = find_root(&cwd).ok_or_else(|| {
        eprintln!("toto: no toto.toml found (run from inside a project)");
        ExitCode::from(3)
    })?;
    let root = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest = read_manifest(&manifest_path).map_err(|e| {
        eprintln!("{}", e.message);
        ExitCode::from(3)
    })?;
    Ok((root, manifest))
}

/// Read one resolved import path: embedded `gzv.std` sources first
/// (deterministic toolchain behavior from any directory, including
/// inside built binaries), then disk anchored at `root` (lets stdlib
/// developers iterate with a local `gzv.std/` overlay directory).
pub(crate) fn read_source(root: &Path, p: &str) -> Option<String> {
    if let Some(src) = toto_compiler::stdlib::get(p) {
        return Some(src.to_string());
    }
    std::fs::read_to_string(root.join(p)).ok()
}

/// Load + check the project entry; on failure the diagnostics are
/// already printed and the process exit is returned.
fn load_project(
    root: &Path,
    manifest: &Manifest,
) -> Result<
    (
        toto_compiler::project::Project,
        toto_compiler::hir::CheckedProgram,
    ),
    ExitCode,
> {
    let entry_path = root.join(&manifest.entry);
    let entry_display = manifest.entry.clone();
    let src = std::fs::read_to_string(&entry_path).map_err(|e| {
        eprintln!("toto: couldn't read `{}`: {e}", entry_path.display());
        ExitCode::from(3)
    })?;
    // Reads anchor at the project root so `toto build` works from any
    // working directory; displays stay root-relative and stable.
    match toto_compiler::project::load_and_check(&entry_display, &src, &|p| read_source(root, p)) {
        Ok(loaded) => Ok(loaded),
        Err(failed) => {
            for fd in &failed.errors {
                let f = &failed.files[fd.file];
                eprint!(
                    "{}",
                    toto_compiler::render_errors(&fd.diags, &f.src, &f.path)
                );
            }
            Err(ExitCode::from(2))
        }
    }
}

/// Minimal `r#"..."#` string literal that cannot collide with `s`.
fn raw_str(s: &str) -> String {
    let mut n = 0usize;
    loop {
        let close = format!("\"{}", "#".repeat(n));
        if !s.contains(&close) {
            break;
        }
        n += 1;
    }
    format!("r{0}\"{1}\"{0}", "#".repeat(n), s)
}

/// Directory holding the workspace sources `toto build` links against
/// (baked in at compile time: build needs the source tree present).
fn workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

pub fn build(release: bool) -> ExitCode {
    let (root, manifest) = match project_root() {
        Ok(ok) => ok,
        Err(code) => return code,
    };
    let (project, checked) = match load_project(&root, &manifest) {
        Ok(ok) => ok,
        Err(code) => return code,
    };
    // Foreign calls lower to `toto_foreign` (Phase 5, Stage 5):
    // non-callback FFI builds and runs from generated code;
    // callback-taking externs still refuse at compile time with the
    // `toto run` / `toto test` workaround.
    let mir = toto_compiler::mir::lower(&checked);
    let opt = if release {
        toto_jit::compile::OptLevel::Speed
    } else {
        toto_jit::compile::OptLevel::Off
    };
    let artifact = match toto_jit::aot::compile_to_object(&mir, opt, &manifest.name) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("toto: codegen failed: {e}");
            return ExitCode::from(3);
        }
    };
    let build_dir = root.join("target").join("toto-build");
    let shim_dir = build_dir.join("shim");
    if let Err(e) = std::fs::create_dir_all(shim_dir.join("src")) {
        eprintln!("toto: couldn't create build dir: {e}");
        return ExitCode::from(3);
    }
    if let Err(e) = std::fs::write(
        build_dir.join(format!("{}.o", manifest.name)),
        &artifact.object,
    ) {
        eprintln!("toto: couldn't write object file: {e}");
        return ExitCode::from(3);
    }
    let ws = workspace_dir();
    let shim_manifest = format!(
        "[package]\nname = \"toto-app-shim\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\ntoto-jit = {{ path = \"{}/crates/toto-jit\" }}\ntoto-compiler = {{ path = \"{}/crates/toto-compiler\" }}\ntoto-runtime = {{ path = \"{}/crates/toto-runtime\" }}\n",
        ws.display(),
        ws.display(),
        ws.display()
    );
    if let Err(e) = std::fs::write(shim_dir.join("Cargo.toml"), shim_manifest) {
        eprintln!("toto: couldn't write shim manifest: {e}");
        return ExitCode::from(3);
    }
    let main_rs = gen_shim_main(&project, &artifact);
    if let Err(e) = std::fs::write(shim_dir.join("src").join("main.rs"), main_rs) {
        eprintln!("toto: couldn't write shim sources: {e}");
        return ExitCode::from(3);
    }
    let obj_abs = build_dir.join(format!("{}.o", manifest.name));
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build").current_dir(&shim_dir);
    if release {
        cmd.arg("--release");
    }
    // The object links in through a rustc link arg (absolute path):
    // the shim crate itself stays backend-agnostic. Any user RUSTFLAGS
    // are preserved in front.
    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    rustflags.push_str(&format!("-C link-arg={}", obj_abs.display()));
    cmd.env("RUSTFLAGS", rustflags);
    eprintln!(
        "toto: linking {} ({}) ...",
        manifest.name,
        if release { "release" } else { "debug" }
    );
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("toto: link failed (cargo exited {status})");
            return ExitCode::from(3);
        }
        Err(e) => {
            eprintln!("toto: couldn't run cargo: {e}");
            return ExitCode::from(3);
        }
    }
    let profile = if release { "release" } else { "debug" };
    let built = shim_dir.join("target").join(profile).join("toto-app-shim");
    let out_path = root.join("target").join(&manifest.name);
    if let Err(e) = std::fs::copy(&built, &out_path) {
        eprintln!("toto: couldn't install binary: {e}");
        return ExitCode::from(3);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
    }
    println!("toto: built {}", out_path.display());
    ExitCode::from(0)
}

/// Rust-safe alias for a closure-body link symbol (see above).
fn body_alias(id: u32) -> String {
    format!("toto_body{id}")
}

/// Generate the shim crate's `main.rs`: embedded sources, link-symbol
/// tables from the build, and the `run_aot` hosting call.
fn gen_shim_main(
    project: &toto_compiler::project::Project,
    artifact: &toto_jit::aot::AotArtifact,
) -> String {
    let mut src = String::new();
    src.push_str("// Generated by `toto build`; do not edit.\n");
    src.push_str("use std::collections::HashMap;\nuse std::io::Write;\n\n");
    src.push_str("static SOURCES: &[(&str, &str)] = &[\n");
    for f in &project.files {
        src.push_str(&format!(
            "    ({}, {}),\n",
            raw_str(&f.path),
            raw_str(&f.src)
        ));
    }
    src.push_str("];\n");
    src.push_str(&format!(
        "static ENTRY: &str = {};\n\n",
        raw_str(&project.files[project.entry].path)
    ));
    // `JitTaskCtx` crosses the shim boundary only as an opaque pointer
    // (created and consumed inside `toto-jit`); it is never read or
    // written here, so the layout warning does not apply.
    src.push_str("#[allow(improper_ctypes)]\nextern \"C\" {\n");
    for (_, sym) in &artifact.fn_symbols {
        src.push_str(&format!(
            "    fn {sym}(ctx: *mut toto_jit::task::JitTaskCtx) -> u64;\n"
        ));
    }
    // Body symbols carry a `$` suffix by design (see `body_fn_name`:
    // Toto identifiers cannot spell it, so user functions can never
    // collide with closure bodies). Rust identifiers cannot spell `$`
    // either, so bodies are declared under a safe alias pinned to the
    // real link symbol with `#[link_name]`.
    for (id, sym) in &artifact.body_symbols {
        src.push_str(&format!(
            "    #[link_name = \"{sym}\"]\n    fn {}(ctx: *mut toto_jit::task::JitTaskCtx) -> u64;\n",
            body_alias(*id),
        ));
    }
    src.push_str("}\n\n");
    src.push_str("fn main() {\n    std::process::exit(run());\n}\n\nfn run() -> i32 {\n");
    src.push_str("    let mut workers = 1usize;\n");
    src.push_str("    let mut args = std::env::args().skip(1);\n");
    src.push_str("    while let Some(a) = args.next() {\n");
    src.push_str("        if a == \"--workers\" {\n");
    src.push_str("            match args.next().and_then(|n| n.parse::<usize>().ok()) {\n");
    src.push_str("                Some(n) if n >= 1 => workers = n,\n");
    src.push_str("                _ => { eprintln!(\"toto: `--workers` needs a positive number\"); return 3; }\n");
    src.push_str("            }\n");
    src.push_str("        } else { eprintln!(\"toto: unexpected argument `{a}`\"); return 3; }\n");
    src.push_str("    }\n");
    src.push_str("    let entry_src = SOURCES.iter().find(|(p, _)| *p == ENTRY).map(|(_, s)| *s).unwrap_or(\"\");\n");
    src.push_str("    let table: HashMap<&str, &str> = SOURCES.iter().copied().collect();\n");
    src.push_str("    let (project, checked) = match toto_compiler::project::load_and_check(ENTRY, entry_src, &|p| table.get(p).map(|s| s.to_string())) {\n");
    src.push_str("        Ok(ok) => ok,\n");
    src.push_str("        Err(failed) => {\n");
    src.push_str("            for fd in &failed.errors {\n");
    src.push_str("                let f = &failed.files[fd.file];\n");
    src.push_str("                eprint!(\"{}\", toto_compiler::render_errors(&fd.diags, &f.src, &f.path));\n");
    src.push_str("            }\n");
    src.push_str("            return 2;\n");
    src.push_str("        }\n");
    src.push_str("    };\n");
    src.push_str("    let mut entries = HashMap::new();\n");
    for (name, sym) in &artifact.fn_symbols {
        src.push_str(&format!(
            "    entries.insert({name:?}.to_string(), {sym} as *const () as usize);\n"
        ));
    }
    src.push_str(if artifact.body_symbols.is_empty() {
        "    let bodies = HashMap::new();\n"
    } else {
        "    let mut bodies = HashMap::new();\n"
    });
    for (id, _sym) in &artifact.body_symbols {
        src.push_str(&format!(
            "    bodies.insert({id}u32, {} as *const () as usize);\n",
            body_alias(*id),
        ));
    }
    src.push_str("    let chan_types: Vec<toto_compiler::hir::Ty> = vec![\n");
    for ty in &artifact.chan_types {
        src.push_str(&format!(
            "        {},\n",
            toto_jit::aot::ty_to_rust_expr(ty)
        ));
    }
    src.push_str("    ];\n");
    let entry_sym = artifact
        .fn_symbols
        .iter()
        .find(|(n, _)| n == "main")
        .map(|(_, s)| s.clone())
        .unwrap_or_else(|| "toto_fn_main".to_string());
    src.push_str(&format!(
        "    let outcome = toto_jit::task::run_aot(&project.program, &checked, toto_runtime::RuntimeConfig {{ workers, ..Default::default() }}, {entry_sym} as *const () as usize, entries, bodies, chan_types);\n"
    ));
    src.push_str("    let _ = std::io::stdout().write_all(&outcome.stdout);\n");
    src.push_str("    match outcome.result {\n");
    src.push_str("        Ok(code) => code,\n");
    src.push_str("        Err((diag, trace)) => {\n");
    src.push_str("            eprint!(\"{}\", toto_compiler::render_panic_multi(&diag, &trace, &project));\n");
    src.push_str("            1\n");
    src.push_str("        }\n");
    src.push_str("    }\n}\n");
    src
}

/// Run every `tests/*.gzv` through the `toto run` path and report.
pub fn test_all() -> ExitCode {
    let (root, _manifest) = match project_root() {
        Ok(ok) => ok,
        Err(code) => return code,
    };
    let tests_dir = root.join("tests");
    let mut cases: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&tests_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "gzv") {
                cases.push(path);
            }
        }
    }
    cases.sort();
    if cases.is_empty() {
        println!("toto: no tests in tests/");
        return ExitCode::from(0);
    }
    let mut passed = 0usize;
    let mut failed = 0usize;
    for case in &cases {
        let name = case
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let src = match std::fs::read_to_string(case) {
            Ok(src) => src,
            Err(e) => {
                println!("FAIL {name} (couldn't read: {e})");
                failed += 1;
                continue;
            }
        };
        // Root-relative display keeps failure output stable; reads
        // anchor at the root so tests resolve like the entry does.
        let display = case
            .strip_prefix(&root)
            .unwrap_or(case.as_path())
            .to_string_lossy()
            .into_owned();
        let loaded =
            toto_compiler::project::load_and_check(&display, &src, &|p| read_source(&root, p));
        let (project, checked) = match loaded {
            Ok(ok) => ok,
            Err(failed_load) => {
                println!("FAIL {name} (exit 2)");
                for fd in &failed_load.errors {
                    let f = &failed_load.files[fd.file];
                    eprint!(
                        "{}",
                        toto_compiler::render_errors(&fd.diags, &f.src, &f.path)
                    );
                }
                failed += 1;
                continue;
            }
        };
        let (code, stdout, stderr_text) = run_loaded(&project, &checked, 1);
        if code == 0 {
            println!("ok {name}");
            passed += 1;
        } else {
            println!("FAIL {name} (exit {code})");
            let _ = std::io::stdout().write_all(&stdout);
            eprint!("{stderr_text}");
            failed += 1;
        }
    }
    println!("toto: {passed} passed, {failed} failed");
    if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

/// Execute a loaded project exactly like `toto run`: runtime executor
/// for concurrent programs, interpreter otherwise. Returns
/// (exit code, captured stdout, rendered stderr).
pub(crate) fn run_loaded(
    project: &toto_compiler::project::Project,
    checked: &toto_compiler::hir::CheckedProgram,
    workers: usize,
) -> (i32, Vec<u8>, String) {
    if checked.uses_concurrency {
        let outcome = toto_runtime::run_checked(
            &project.program,
            checked,
            toto_runtime::RuntimeConfig {
                workers,
                ..toto_runtime::RuntimeConfig::default()
            },
        );
        return match outcome.result {
            Ok(code) => (code, outcome.stdout, String::new()),
            Err((diag, trace)) => {
                let err = toto_compiler::render_panic_multi(&diag, &trace, project);
                (1, outcome.stdout, err)
            }
        };
    }
    match toto_compiler::run_program(&project.program, &checked.externs) {
        (Ok(code), stdout) => (code, stdout, String::new()),
        (Err((diag, trace)), stdout) => {
            let err = toto_compiler::render_panic_multi(&diag, &trace, project);
            (1, stdout, err)
        }
    }
}
