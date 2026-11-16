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
                    \x20   toto dev [--workers N] [--drain-timeout SECS] <file.gzv>\n\
                    \x20                                      rebuild and restart on every save\n\
                    \x20   toto --help                        show this help\n\
                    \x20   toto --version                     show the version\n\
                    \n\
                    Options:\n\
                    \x20   --workers N   scheduler worker threads for concurrent programs\n\
                    \x20                 (default 1: deterministic; higher values enable\n\
                    \x20                 M:N work-stealing with nondeterministic interleavings)\n\
                    \x20   --drain-timeout SECS   `toto dev`: how long a running generation may\n\
                    \x20                 finish draining before it is cancelled (default 5;\n\
                    \x20                 a wedged generation past cancel+grace restarts)\n";

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
        Some("dev") => {
            // `toto dev [--workers N] [--drain-timeout SECS] <file.gzv>`;
            // flags may precede the path in any order after `dev`.
            let mut workers = 1usize;
            let mut drain_secs = 5u64;
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
                } else if a == "--drain-timeout" {
                    match rest.next().and_then(|n| n.parse::<u64>().ok()) {
                        Some(n) => drain_secs = n,
                        _ => {
                            eprintln!("toto: `--drain-timeout` needs a number of seconds");
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
                Some(p) => dev_watch(p, workers, drain_secs),
                None => {
                    eprintln!(
                        "toto: `dev` needs a file: toto dev [--workers N] [--drain-timeout SECS] <file.gzv>"
                    );
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

/// Ingest one source revision into the dev driver: forward the retired
/// generation's streamed output, log lifecycle to stderr, and report
/// the new running generation. Program stdout stays verbatim on
/// stdout; everything else goes to stderr.
fn ingest(
    driver: &mut toto_jit::dev::DevDriver<toto_jit::dev::ReexecRestart>,
    src: &str,
    filename: &str,
) {
    use toto_jit::dev::RebuildOutcome;
    match driver.rebuild(src, filename) {
        RebuildOutcome::StillBroken(diags) => {
            eprint!("{}", render_errors(&diags, src, filename));
            eprintln!("toto dev: source has errors; waiting for a fix");
        }
        RebuildOutcome::NoChange => {}
        RebuildOutcome::CompileRefused(e) => {
            eprintln!("toto dev: internal error: {e}; keeping the running generation");
        }
        RebuildOutcome::Rebuilt { ended, .. } => {
            if let Some(ended) = ended {
                let _ = std::io::stdout().write_all(&ended.stdout);
                eprint!("{}", ended.stderr);
                eprintln!(
                    "toto dev: generation {} exited ({})",
                    ended.generation, ended.code
                );
            }
            if let Some(n) = driver.current_gen() {
                eprintln!("toto dev: generation {n} started");
            }
        }
        RebuildOutcome::Restarted => {
            // Unreachable with the production restart policy (the
            // process re-executes instead of returning).
            eprintln!("toto dev: generation wedged; restarted");
        }
    }
}

fn dev_watch(path: &str, workers: usize, drain_secs: u64) -> ExitCode {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use toto_jit::dev::{DevDriver, DrainPolicy, PollWatcher, ReexecRestart};

    let initial = match std::fs::read_to_string(path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("toto: couldn't read `{path}`: {e}");
            return ExitCode::from(3);
        }
    };
    let mut watcher =
        match PollWatcher::new(std::path::PathBuf::from(path), Duration::from_millis(100)) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("toto: couldn't watch `{path}`: {e}");
                return ExitCode::from(3);
            }
        };
    let mut driver = DevDriver::new(
        DrainPolicy {
            drain_timeout: Duration::from_secs(drain_secs),
            cancel_grace: Duration::from_secs(5),
        },
        ReexecRestart,
        toto_runtime::RuntimeConfig {
            workers,
            ..toto_runtime::RuntimeConfig::default()
        },
    );
    let stop = AtomicBool::new(false);
    ingest(&mut driver, &initial, path);
    // Close the read-vs-watch gap: a save landing between the two
    // reads above rebuilds immediately instead of waiting a tick.
    if let Ok(Some(newer)) = watcher.poll_once() {
        ingest(&mut driver, &String::from_utf8_lossy(&newer), path);
    }
    // `stop` never fires in the CLI; the loop ends by signal.
    while let Some(bytes) = watcher.wait_change(&stop) {
        ingest(&mut driver, &String::from_utf8_lossy(&bytes), path);
    }
    ExitCode::from(0)
}
