//! `toto` CLI (Phase 1): `toto run <file.gzv>`, `--help`, `--version`.
//! Single-file programs only; exit codes per the spec §10.

use std::io::Write;
use std::process::ExitCode;

use toto_compiler::{render_errors, render_panic, run_source, Outcome};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "toto — a new programming language (.gzv)\n\
                    \n\
                    Usage:\n\
                    \x20   toto run <file.gzv>   lex, parse, and run a program\n\
                    \x20   toto --help           show this help\n\
                    \x20   toto --version        show the version\n";

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
        Some("run") => match args.get(2) {
            Some(path) => run_file(path),
            None => {
                eprintln!("toto: `run` needs a file: toto run <file.gzv>");
                ExitCode::from(3)
            }
        },
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

fn run_file(path: &str) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("toto: couldn't read `{path}`: {e}");
            return ExitCode::from(3);
        }
    };
    // Diagnostic paths show the path as given on the command line.
    let filename = path;
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
