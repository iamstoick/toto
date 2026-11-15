//! Subprocess harness for gate-mode JIT execution: compiles one
//! program, runs `main`, and mirrors the CLI's exit/output contract
//! (the differential compares it against the tree-walking
//! interpreter). Panics render full spec-§6 (the probe owns the
//! source); gate traces are single-frame by design (frame depth is
//! proven through task mode instead).

use std::io::Write;

use toto_compiler::ast::ItemKind;
use toto_compiler::diag::Diag;
use toto_compiler::interp::TraceFrame;
use toto_compiler::span::Span;
use toto_compiler::{mir, parse_and_check, render_errors, render_panic};
use toto_jit::{Jit, JitRt, run_jit};
use toto_runtime::RuntimeConfig;

static PROBE_SRC: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

fn probe_on_panic(info: &toto_jit::runtime::PanicInfo) -> ! {
    let (src, filename) = PROBE_SRC.get().cloned().unwrap_or_default();
    let code: &'static str = Box::leak(format!("E{:04}", info.code).into_boxed_str());
    let diag = Diag::new(
        code,
        info.message.clone(),
        Span::new(info.start as u32, info.end as u32),
        None,
    );
    let trace = vec![TraceFrame {
        name: info.func.clone(),
        span: diag.span,
    }];
    eprint!("{}", render_panic(&diag, &trace, &src, &filename));
    std::process::exit(1);
}

fn main() {
    let mut argv = std::env::args();
    let _ = argv.next();
    let first = argv.next().unwrap_or_default();
    let (task_mode, path) = if first == "--task" {
        (true, argv.next().unwrap_or_default())
    } else {
        (false, first)
    };
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("jit_probe: couldn't read `{path}`: {e}");
        std::process::exit(3);
    });
    let filename = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let (program, checked) = match parse_and_check(&src) {
        Ok(v) => v,
        Err(diags) => {
            eprint!("{}", render_errors(&diags, &src, &filename));
            std::process::exit(2);
        }
    };
    if checked.uses_concurrency && !task_mode {
        eprintln!("jit_probe: concurrent program needs task mode");
        std::process::exit(3);
    }
    let prog = mir::lower(&checked);
    if !prog.fn_index.contains_key("main") {
        // Mirror the interpreter exactly (checked code always has it).
        let diag = Diag::new(
            "E0204",
            "program has no `fn main() -> int`".to_string(),
            Span::new(0, 0),
            None,
        );
        eprint!("{}", render_panic(&diag, &[], &src, &filename));
        std::process::exit(1);
    }
    if task_mode {
        return run_task_mode(&src, &filename, &program, &checked);
    }
    let mut jit = Jit::new().unwrap_or_else(|e| {
        eprintln!("jit_probe: jit session failed: {e}");
        std::process::exit(3);
    });
    if let Err(e) = jit.compile(&prog) {
        eprintln!("jit_probe: compile refused: {e}");
        std::process::exit(3);
    }
    let _ = PROBE_SRC.set((src.clone(), filename.clone()));
    let mut rt = JitRt::new();
    for f in &prog.fns {
        rt.entries.insert(f.name.clone(), jit.ptr(&f.name) as usize);
    }
    rt.func_names = prog.fns.iter().map(|f| f.name.clone()).collect();
    for (id, ptr) in jit.body_entries() {
        rt.bodies.insert(id, ptr as usize);
    }
    rt.fns = program
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Fn(decl) => Some(decl.clone()),
            _ => None,
        })
        .collect();
    rt.on_panic = probe_on_panic;
    rt.func = String::from("main");
    let out = unsafe { Jit::call_ptr(jit.ptr("main"), rt.ctx_ptr(), &[]) };
    std::io::stdout().write_all(&rt.output).unwrap_or_else(|_| {
        std::process::exit(3);
    });
    let n = out as i64;
    if (0..=255).contains(&n) {
        std::process::exit(n as u8);
    }
    // Mirror the interpreter's main-range panic exactly.
    let diag = Diag::new(
        "E0204",
        format!("main returned {n}, which is not a valid exit code 0-255"),
        prog.main_span,
        None,
    );
    let trace = vec![TraceFrame {
        name: String::from("main"),
        span: prog.main_span,
    }];
    eprint!("{}", render_panic(&diag, &trace, &src, &filename));
    std::process::exit(1);
}

/// Task-mode execution: the scheduler-driven JIT on coroutine stacks
/// (concurrent programs allowed). Same exit/output contract.
fn run_task_mode(
    src: &str,
    filename: &str,
    program: &toto_compiler::ast::Program,
    checked: &toto_compiler::hir::CheckedProgram,
) {
    let mut jit = Jit::new_task().unwrap_or_else(|e| {
        eprintln!("jit_probe: jit session failed: {e}");
        std::process::exit(3);
    });
    let prog = mir::lower(checked);
    let session = jit.compile_task(&prog).unwrap_or_else(|e| {
        eprintln!("jit_probe: compile refused: {e}");
        std::process::exit(3);
    });
    let outcome = run_jit(program, checked, RuntimeConfig::default(), &session);
    std::io::stdout()
        .write_all(&outcome.stdout)
        .unwrap_or_else(|_| {
            std::process::exit(3);
        });
    match outcome.result {
        Ok(code) => std::process::exit(code),
        Err((diag, trace)) => {
            eprint!("{}", render_panic(&diag, &trace, src, filename));
            std::process::exit(1);
        }
    }
}
