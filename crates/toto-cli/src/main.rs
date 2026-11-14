//! `toto` CLI (Phase 1): `toto run <file.gzv>`, `--help`, `--version`.
//! Single-file programs only; exit codes per the spec §10.

use std::io::Write;
use std::process::ExitCode;

use toto_compiler::{Outcome, parse_and_check, render_errors, render_panic, run_source};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "toto — a new programming language (.gzv)\n\
                    \n\
                    Usage:\n\
                    \x20   toto run [--workers N] <file.gzv>   lex, parse, and run a program\n\
                    \x20   toto --help                        show this help\n\
                    \x20   toto --version                     show the version\n\
                    \n\
                    Options:\n\
                    \x20   --workers N   scheduler worker threads for concurrent programs\n\
                    \x20                 (default 1: deterministic; higher values enable\n\
                    \x20                 M:N work-stealing with nondeterministic interleavings)\n";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--help" | "-h") => {
            print!("{HELP}");
            ExitCode::from(0)
        }
        Some("--version" | "-V") => {
            println!("toto {VERSION}");
            ExitCode::from(0)
        }
        Some("run") => {
            // `toto run [--workers N] <file.gzv>`; flags may precede
            // the path in any order after `run`.
            let mut workers = 1usize;
            let mut path: Option<&str> = None;
            let mut rest = args.iter().skip(2);
            while let Some(a) = rest.next() {
                if a == "--workers" {
                    match rest.next().and_then(|n| n.parse::<usize>().ok()) {
                        Some(n) if n >= 1 => workers = n,
                        _ => {
                            eprintln!("toto: `--workers` needs a positive number");
                            return ExitCode::from(3);
                        }
                    }
                } else if path.is_none() {
                    path = Some(a);
                } else {
                    eprintln!("toto: unexpected argument `{a}`");
                    return ExitCode::from(3);
                }
            }
            match path {
                Some(p) => run_file(p, workers),
                None => {
                    eprintln!("toto: `run` needs a file: toto run [--workers N] <file.gzv>");
                    ExitCode::from(3)
                }
            }
        }
        Some(other) => {
            eprintln!("toto: unknown command `{other}`\n\n{HELP}");
            ExitCode::from(3)
        }
        None => {
            eprintln!("{HELP}");
            ExitCode::from(3)
        }
    }
}

fn run_file(path: &str, workers: usize) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("toto: couldn't read `{path}`: {e}");
            return ExitCode::from(3);
        }
    };
    // Diagnostic paths show the path as given on the command line.
    let filename = path;
    // Route concurrent programs to the runtime executor (spec §13);
    // sequential programs stay on the tree-walking interpreter.
    let (program, checked) = match parse_and_check(&src) {
        Ok(parsed) => parsed,
        Err(diags) => {
            eprint!("{}", render_errors(&diags, &src, filename));
            return ExitCode::from(2);
        }
    };
    if checked.uses_concurrency {
        let outcome = toto_runtime::run_checked(
            &program,
            &checked,
            toto_runtime::RuntimeConfig {
                workers,
                ..toto_runtime::RuntimeConfig::default()
            },
        );
        return match outcome.result {
            Ok(code) => {
                let _ = std::io::stdout().write_all(&outcome.stdout);
                ExitCode::from(code as u8)
            }
            Err((diag, trace)) => {
                let _ = std::io::stdout().write_all(&outcome.stdout);
                eprint!("{}", render_panic(&diag, &trace, &src, filename));
                ExitCode::from(1)
            }
        };
    }
    match run_source(&src, filename) {
        Outcome::Success { code, stdout } => {
            let _ = std::io::stdout().write_all(&stdout);
            ExitCode::from(code as u8)
        }
        Outcome::Static(diags) => {
            eprint!("{}", render_errors(&diags, &src, filename));
            ExitCode::from(2)
        }
        Outcome::RuntimePanic {
            diag,
            trace,
            stdout,
        } => {
            let _ = std::io::stdout().write_all(&stdout);
            eprint!("{}", render_panic(&diag, &trace, &src, filename));
            ExitCode::from(1)
        }
    }
}
