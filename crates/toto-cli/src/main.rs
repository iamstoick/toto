//! `toto` CLI (Phase 1): `toto run <file.gzv>`, `--help`, `--version`.
//! Single-file programs only; exit codes per the spec §10.

use std::io::Write;
use std::process::ExitCode;

use toto_compiler::render_errors;

mod build_cmd;
mod manifest;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "toto — a new programming language (.gzv)\n\
                    \n\
                    Usage:\n\
                    \x20   toto run [--workers N] <file.gzv>   lex, parse, and run a program\n\
                    \x20   toto dev [--workers N] [--drain-timeout SECS] [--opt-level LEVEL] <file.gzv>\n\
                    \x20                                      rebuild and restart on every save\n\
                    \x20   toto init <name>                   scaffold a runnable project\n\
                    \x20   toto build [--release]             build the project executable\n\
                    \x20   toto test                         run the project tests\n\
                    \x20   toto --help                        show this help\n\
                    \x20   toto --version                     show the version\n\
                    \n\
                    Options:\n\
                    \x20   --workers N   scheduler worker threads for concurrent programs\n\
                    \x20                 (default 1: deterministic; higher values enable\n\
                    \x20                 M:N work-stealing with nondeterministic interleavings)\n\
                    \x20   --drain-timeout SECS   `toto dev`: how long a running generation may\n\
                    \x20                 finish draining before it is cancelled (default 5;\n\
                    \x20                 a wedged generation past cancel+grace restarts)\n\
                    \x20   --opt-level LEVEL   `toto dev`: Cranelift optimization for\n\
                    \x20                 generated code: off (default), speed, or\n\
                    \x20                 speed_and_size; behavior is identical\n\
                    \x20                 at every level (see the opt gate)\n";

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
            // `toto dev [--workers N] [--drain-timeout SECS]
            // [--opt-level off|speed|speed_and_size] <file.gzv>`;
            // flags may precede the path in any order after `dev`.
            let mut workers = 1usize;
            let mut drain_secs = 5u64;
            let mut opt = toto_jit::OptLevel::Off;
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
                } else if a == "--opt-level" {
                    match rest.next().and_then(|n| toto_jit::OptLevel::parse(n)) {
                        Some(l) => opt = l,
                        _ => {
                            eprintln!("toto: `--opt-level` needs off, speed, or speed_and_size");
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
                Some(p) => dev_watch(p, workers, drain_secs, opt),
                None => {
                    eprintln!(
                        "toto: `dev` needs a file: toto dev [--workers N] [--drain-timeout SECS] [--opt-level LEVEL] <file.gzv>"
                    );
                    ExitCode::from(3)
                }
            }
        }
        Some("init") => {
            let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
            match rest.as_slice() {
                [name] => init_project(name),
                _ => {
                    eprintln!("toto: `init` needs a name: toto init <name>");
                    ExitCode::from(3)
                }
            }
        }
        Some("build") => {
            let release = args.iter().skip(2).any(|a| a == "--release");
            let bad: Vec<&str> = args
                .iter()
                .skip(2)
                .map(String::as_str)
                .filter(|a| *a != "--release")
                .collect();
            if !bad.is_empty() {
                eprintln!("toto: unexpected `build` flags: {}", bad.join(" "));
                return ExitCode::from(3);
            }
            build_cmd::build(release)
        }
        Some("test") => {
            let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
            if !rest.is_empty() {
                eprintln!("toto: `test` takes no arguments");
                return ExitCode::from(3);
            }
            build_cmd::test_all()
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

fn init_project(dir: &str) -> ExitCode {
    let path = std::path::Path::new(dir);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match manifest::init_project(path, &name) {
        Ok(dir) => {
            println!("toto: initialized `{}`", dir.display());
            println!("toto:   toto.toml / main.gzv / tests/example.gzv");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("{}", e.message);
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
    // `load_and_check` resolves `import` statements against the
    // importing file's directory and checks the merged program; files
    // without imports behave exactly as before (single-file project).
    let (project, checked) = match toto_compiler::project::load_and_check(path, &src, &|p| {
        std::fs::read_to_string(p).ok()
    }) {
        Ok(loaded) => loaded,
        Err(failed) => {
            for fd in &failed.errors {
                let f = &failed.files[fd.file];
                eprint!(
                    "{}",
                    toto_compiler::render_errors(&fd.diags, &f.src, &f.path)
                );
            }
            return ExitCode::from(2);
        }
    };
    // One execution path for `run` and `test` (see `run_loaded`).
    let (code, stdout, stderr_text) = build_cmd::run_loaded(&project, &checked, workers);
    let _ = std::io::stdout().write_all(&stdout);
    eprint!("{stderr_text}");
    ExitCode::from(code as u8)
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

fn dev_watch(path: &str, workers: usize, drain_secs: u64, opt: toto_jit::OptLevel) -> ExitCode {
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
        opt,
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
