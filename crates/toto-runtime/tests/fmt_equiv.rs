//! Formatter equivalence through the runtime executor (Phase 5,
//! Stage 4): concurrent single-file programs and multi-file project
//! programs must behave identically formatted as unformatted.
//!
//! Same discipline as `concurrent.rs`: the GC-pressure config (forced
//! collection at every safepoint) with the same explicit program
//! lists, so formatting is proven under the harshest runtime, not
//! just the default one. Panic goldens pin source locations, which
//! formatting legitimately moves, so status-1 files compare the exit
//! code, stdout bytes, and the diagnostic code + message (golden
//! stderr first line), not the full snippet text.

use std::path::{Path, PathBuf};

use toto_compiler::{fmt, project, render_panic_multi};
use toto_runtime::{RuntimeConfig, run_checked};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus")
}

fn pressure_config() -> RuntimeConfig {
    RuntimeConfig {
        gc_threshold_bytes: 1,
        rng_seed: 0x9E37_79B9_7F4A_7C15,
        stack_bytes: 64 * 1024,
        collect_at_every_safepoint_for_test: true,
        workers: 1,
        blocking_pool_threads: 2,
    }
}

fn golden_head(stderr: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(stderr);
    let first = text.lines().next()?;
    let rest = first
        .strip_prefix("error[")
        .or_else(|| first.strip_prefix("panic["))?;
    let (code, message) = rest.split_once(']')?;
    Some((
        code.to_string(),
        message.strip_prefix(": ").unwrap_or(message).to_string(),
    ))
}

fn parse(src: &str) -> Option<toto_compiler::ast::Program> {
    let (tokens, lex_errors) = toto_compiler::lexer::lex(src);
    if !lex_errors.is_empty() {
        return None;
    }
    let (program, parse_errors) = toto_compiler::parser::parse(tokens, src);
    if !parse_errors.is_empty() {
        return None;
    }
    Some(program)
}

fn check_formatted(
    name: &str,
    formatted: &str,
    dir: &Path,
    want_status: &str,
    want_out: &[u8],
    want_err: &[u8],
    failures: &mut Vec<String>,
) {
    // Imports resolve against the case directory, so both loading
    // and running happen under it (as the CLI corpus runner does).
    let saved_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(dir).expect("set case cwd");
    // Embedded `gzv.std` first, then disk (the CLI's resolution
    // order for project loads).
    let loaded = project::load_and_check(
        "main.gzv",
        formatted,
        &|p| {
            toto_compiler::stdlib::get(p)
                .map(str::to_string)
                .or_else(|| std::fs::read_to_string(p).ok())
        },
        &[],
    );
    let outcome = loaded.map(|(project, checked)| {
        let outcome = run_checked(&project.program, &checked, pressure_config());
        (project, outcome)
    });
    std::env::set_current_dir(&saved_cwd).expect("restore cwd");
    let (project, outcome) = outcome.unwrap_or_else(|e| {
        let mut msg = format!("{name}: formatted project no longer loads:");
        for fd in &e.errors {
            for d in &fd.diags {
                msg.push_str(&format!("\n  [file {}] {}: {}", fd.file, d.code, d.message));
            }
        }
        panic!("{msg}");
    });
    match &outcome.result {
        Ok(code) => {
            if want_status != "0" {
                failures.push(format!(
                    "{name}: formatted exits {code}, want status {want_status}"
                ));
                return;
            }
            if outcome.stdout != want_out {
                failures.push(format!("{name}: formatted stdout differs"));
            }
        }
        Err((diag, trace)) => {
            if want_status != "1" {
                failures.push(format!(
                    "{name}: formatted panics, want status {want_status}: got {} {}",
                    diag.code, diag.message
                ));
                return;
            }
            if outcome.stdout != want_out {
                failures.push(format!("{name}: formatted panic-stdout differs"));
            }
            let head_ok = match golden_head(want_err) {
                Some((code, message)) => code == diag.code && message == diag.message,
                None => false,
            };
            if !head_ok {
                let got_err = render_panic_multi(diag, trace, &project);
                failures.push(format!(
                    "{name}: formatted panic {:?} vs golden {other:?}\n{got_err}",
                    (diag.code, diag.message.as_str()),
                    other = golden_head(want_err),
                ));
            }
        }
    }
}

const POSITIVES: &[&str] = &[
    "conc01_pingpong",
    "conc02_buffered_close",
    "conc03_select_two",
    "conc04_select_default",
    "conc05_moved_struct",
    "conc06_fanout",
    "conc07_zero_values",
    "conc08_go_early_return",
    "conc09_nested_go",
    "conc10_select_send",
    "conc11_chan_of_chan",
    "conc19_chan_eq",
    "conc20_net_hammer",
];

const NEGATIVES: &[&str] = &[
    "conc_neg_send_closed",
    "conc_neg_double_close",
    "conc_neg_deadlock",
    "conc_neg_select_send_closed",
    "conc_neg_child_panic",
    "conc_neg_main_panic",
    "conc18_fneq",
];

#[test]
fn fmt_equiv_concurrent_and_projects() {
    // The formatted corpus (notably the large Wave 2 project files)
    // needs more than the default thread stack during load+check, so
    // the harness provisions it explicitly (scoped to this test, not
    // via environment). 32 MiB matches the proven-sufficient value.
    match std::thread::Builder::new()
        .name("fmt_equiv_main".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(run_fmt_equiv)
    {
        Ok(handle) => match handle.join() {
            Ok(()) => {}
            Err(payload) => std::panic::resume_unwind(payload),
        },
        Err(e) => panic!("fmt_equiv: cannot spawn worker thread: {e}"),
    }
}

fn run_fmt_equiv() {
    let root = corpus_root();
    let mut failures = Vec::new();
    let mut ran = 0usize;
    let condir = root.join("concurrent");
    for name in POSITIVES.iter().chain(NEGATIVES.iter()) {
        let case = condir.join(format!("{name}.gzv"));
        let src = std::fs::read_to_string(&case).expect("read case");
        let program = parse(&src).expect("concurrent case parses");
        let formatted = fmt::format_program(&program);
        let status = std::fs::read_to_string(condir.join(format!("{name}.status")))
            .unwrap_or_default()
            .trim()
            .to_string();
        let want_out = std::fs::read(condir.join(format!("{name}.stdout"))).unwrap_or_default();
        let want_err = std::fs::read(condir.join(format!("{name}.stderr"))).unwrap_or_default();
        check_formatted(
            name,
            &formatted,
            &condir,
            &status,
            &want_out,
            &want_err,
            &mut failures,
        );
        ran += 1;
    }
    // Multi-file project programs: format every `.gzv` into a
    // scratch dir (copying data assets), then run the formatted
    // tree under GC pressure.
    let projdir = root.join("project");
    let mut projects: Vec<_> = std::fs::read_dir(&projdir)
        .expect("project dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    projects.sort();
    for proj in projects {
        let name = proj
            .file_name()
            .expect("dir name")
            .to_string_lossy()
            .into_owned();
        let entry = proj.join("main.gzv");
        if !entry.is_file() {
            continue;
        }
        let src = std::fs::read_to_string(&entry).expect("read main");
        let Some(program) = parse(&src) else { continue };
        let formatted = fmt::format_program(&program);
        let status = std::fs::read_to_string(proj.join("main.status"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if status != "0" && status != "1" {
            continue;
        }
        let want_out = std::fs::read(proj.join("main.stdout")).unwrap_or_default();
        let want_err = std::fs::read(proj.join("main.stderr")).unwrap_or_default();
        let scratch = std::env::temp_dir().join(format!("toto-fmt-{name}"));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        // Recursive copy: format every `.gzv`, copy every other
        // file, so relative imports keep resolving.
        fn mirror(src: &Path, dst: &Path) {
            for ent in std::fs::read_dir(src).expect("read dir") {
                let path = ent.expect("entry").path();
                let fname = path.file_name().expect("file name").to_owned();
                if path.is_dir() {
                    std::fs::create_dir_all(dst.join(&fname)).expect("mkdir");
                    mirror(&path, &dst.join(&fname));
                } else if path.extension().is_some_and(|e| e == "gzv") {
                    let fsrc = std::fs::read_to_string(&path).expect("read file");
                    let fprogram = parse(&fsrc).expect("project file parses");
                    std::fs::write(dst.join(fname), fmt::format_program(&fprogram))
                        .expect("write formatted");
                } else {
                    std::fs::copy(&path, dst.join(fname)).expect("copy asset");
                }
            }
        }
        mirror(&proj, &scratch);
        assert!(
            formatted == std::fs::read_to_string(scratch.join("main.gzv")).expect("read back"),
            "{name}: scratch main mismatch"
        );
        check_formatted(
            &name,
            &formatted,
            &scratch,
            &status,
            &want_out,
            &want_err,
            &mut failures,
        );
        let _ = std::fs::remove_dir_all(&scratch);
        ran += 1;
    }
    assert!(ran > 0, "no concurrent/project programs ran");
    assert!(
        failures.is_empty(),
        "{} fmt-equiv failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("fmt-equiv: {ran} programs equivalent");
}
