//! `toto` CLI (Phase 1): `toto run <file.gzv>`, `--help`, `--version`.
//! Single-file programs only; exit codes per the spec §10.

use std::io::Write;
use std::process::ExitCode;

use toto_compiler::render_errors;

mod build_cmd;
mod lsp;
mod manifest;
mod packages;
mod pkg_cmd;
mod semver;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "toto — a new programming language (.gzv)\n\
                    \n\
                    Usage:\n\
                    \x20   toto run [--workers N] <file.gzv>   lex, parse, and run a program\n\
                    \x20   toto dev [--workers N] [--drain-timeout SECS] [--opt-level LEVEL] <file.gzv>\n\
                    \x20                                      rebuild and restart on every save\n\
                    \x20   toto init <name>                   scaffold a runnable project\n\
                    \x20   toto build [--release] [--offline]  build the project executable\n\
                    \x20                                      (--offline: no network; fails loudly on a cold cargo cache)\n\
                    \x20   toto dist [--release] [--offline]   self-contained dist (bin + bundled lib closure)\n\
                    \x20                                      under target/dist/<name>/ (runs with empty env)\n\
                    \x20   toto add <name> --path DIR | --git URL [--rev R | --tag T]\n\
                    \x20                                      add a dependency (writes toto.toml + toto.lock)\n\
                    \x20   toto update [pkg...]              re-resolve dependencies, rewrite toto.lock\n\
                    \x20   toto publish                      pack <name>-<version>.gzv-pkg (local only)\n\
                    \x20   toto test                         run the project tests\n\
                    \x20   toto fmt [--check] <file.gzv>...   format files in place\n\
                    (--check: list files that would change, no writes)\n\
                    \x20   toto lint [--deny-warnings] <file.gzv>...\n\
                    \x20                                      hygiene lints (E05xx warnings)\n\
                    \x20   toto coverage [--fail-under PCT] <file.gzv>...\n\
                    \x20                                      LCOV report on stdout (sequential programs only)\n\
                    \x20   toto doc [--out DIR] <file.gzv>...  API docs as Markdown\n\
                    \x20                                      (stdout, or per-module files + index with --out)\n\
                    \x20   toto lsp                          basic LSP server on stdio\n\
                    (hover, go-to-definition, diagnostics)\n\
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
            let offline = args.iter().skip(2).any(|a| a == "--offline");
            let bad: Vec<&str> = args
                .iter()
                .skip(2)
                .map(String::as_str)
                .filter(|a| *a != "--release" && *a != "--offline")
                .collect();
            if !bad.is_empty() {
                eprintln!("toto: unexpected `build` flags: {}", bad.join(" "));
                return ExitCode::from(3);
            }
            build_cmd::build(release, offline)
        }
        Some("dist") => {
            let release = args.iter().skip(2).any(|a| a == "--release");
            let offline = args.iter().skip(2).any(|a| a == "--offline");
            let bad: Vec<&str> = args
                .iter()
                .skip(2)
                .map(String::as_str)
                .filter(|a| *a != "--release" && *a != "--offline")
                .collect();
            if !bad.is_empty() {
                eprintln!("toto: unexpected `dist` flags: {}", bad.join(" "));
                return ExitCode::from(3);
            }
            build_cmd::dist(release, offline)
        }
        Some("test") => {
            let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
            if !rest.is_empty() {
                eprintln!("toto: `test` takes no arguments");
                return ExitCode::from(3);
            }
            build_cmd::test_all()
        }
        Some("add") => {
            // `toto add <name> --path DIR | --git URL [--rev R | --tag T] [--version REQ] | <file.gzv-pkg>`
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            if rest.is_empty() {
                eprintln!(
                    "toto: `add` needs a package: toto add <name> --path DIR | --git URL [--rev R | --tag T] | <file.gzv-pkg>"
                );
                return ExitCode::from(3);
            }
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (root, manifest) = match build_cmd::project_root_for(&cwd) {
                Ok(ok) => ok,
                Err(code) => return code,
            };
            let cache = packages::Cache::from_env();
            pkg_cmd::add(&root, manifest, &cache, &rest)
        }
        Some("update") => {
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (root, manifest) = match build_cmd::project_root_for(&cwd) {
                Ok(ok) => ok,
                Err(code) => return code,
            };
            let cache = packages::Cache::from_env();
            pkg_cmd::update(&root, manifest, &cache, &rest)
        }
        Some("publish") => {
            let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
            if !rest.is_empty() {
                eprintln!("toto: `publish` takes no arguments");
                return ExitCode::from(3);
            }
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (root, manifest) = match build_cmd::project_root_for(&cwd) {
                Ok(ok) => ok,
                Err(code) => return code,
            };
            pkg_cmd::publish(&root, manifest)
        }
        Some("fmt") => {
            let mut check = false;
            let mut files = Vec::new();
            for a in args.iter().skip(2) {
                if a == "--check" {
                    check = true;
                } else {
                    files.push(a.clone());
                }
            }
            if files.is_empty() {
                eprintln!("toto: `fmt` needs a file: toto fmt [--check] <file.gzv>...");
                return ExitCode::from(3);
            }
            fmt_files(&files, check)
        }
        Some("coverage") => {
            let mut fail_under: Option<u32> = None;
            let mut files = Vec::new();
            let mut args = args.iter().skip(2).peekable();
            while let Some(a) = args.next() {
                if *a == "--fail-under" {
                    match args.next().and_then(|n| n.parse::<u32>().ok()) {
                        Some(n) if n <= 100 => fail_under = Some(n),
                        _ => {
                            eprintln!("toto: `--fail-under` needs a percentage 0-100");
                            return ExitCode::from(3);
                        }
                    }
                } else {
                    files.push((*a).to_string());
                }
            }
            if files.is_empty() {
                eprintln!(
                    "toto: `coverage` needs a file: toto coverage [--fail-under PCT] <file.gzv>..."
                );
                return ExitCode::from(3);
            }
            coverage_files(&files, fail_under)
        }
        Some("lint") => {
            let mut deny_warnings = false;
            let mut files = Vec::new();
            for a in args.iter().skip(2) {
                if a == "--deny-warnings" {
                    deny_warnings = true;
                } else {
                    files.push(a.clone());
                }
            }
            if files.is_empty() {
                eprintln!("toto: `lint` needs a file: toto lint [--deny-warnings] <file.gzv>...");
                return ExitCode::from(3);
            }
            lint_files(&files, deny_warnings)
        }
        Some("doc") => {
            let mut out: Option<String> = None;
            let mut files = Vec::new();
            let mut rest = args.iter().skip(2);
            while let Some(a) = rest.next() {
                if *a == "--out" {
                    match rest.next() {
                        Some(d) => out = Some((*d).to_string()),
                        None => {
                            eprintln!("toto: `--out` needs a directory");
                            return ExitCode::from(3);
                        }
                    }
                } else {
                    files.push((*a).to_string());
                }
            }
            if files.is_empty() {
                eprintln!("toto: `doc` needs a file: toto doc [--out DIR] <file.gzv>...");
                return ExitCode::from(3);
            }
            doc_files(&files, out.as_deref())
        }
        Some("lsp") => lsp::serve(),
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

/// Format files in place (or, with `--check`, list the ones that
/// would change). Broken files are refused with their diagnostics
/// (exit 2): the formatter never guesses at broken syntax. Each
/// file formats independently; imports are not resolved.
fn fmt_files(files: &[String], check: bool) -> ExitCode {
    let mut changed = Vec::new();
    for path in files {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("toto: couldn't read `{path}`: {e}");
                return ExitCode::from(3);
            }
        };
        let (tokens, lex_errors) = toto_compiler::lexer::lex(&src);
        if !lex_errors.is_empty() {
            let diags: Vec<toto_compiler::diag::Diag> = lex_errors
                .into_iter()
                .map(|e| toto_compiler::diag::Diag::new(e.code, e.message, e.span, e.help))
                .collect();
            eprint!("{}", toto_compiler::render_errors(&diags, &src, path));
            return ExitCode::from(2);
        }
        let (program, parse_errors) = toto_compiler::parser::parse(tokens, &src);
        if !parse_errors.is_empty() {
            eprint!(
                "{}",
                toto_compiler::render_errors(&parse_errors, &src, path)
            );
            return ExitCode::from(2);
        }
        let formatted = toto_compiler::fmt::format_program(&program);
        if formatted != src {
            changed.push(path.clone());
            if !check && let Err(e) = std::fs::write(path, formatted) {
                eprintln!("toto: couldn't write `{path}`: {e}");
                return ExitCode::from(3);
            }
        }
    }
    if check && !changed.is_empty() {
        for path in &changed {
            println!("toto: would reformat `{path}`");
        }
        return ExitCode::from(1);
    }
    ExitCode::from(0)
}

/// Lint files (Stage 1): Tier A hygiene (E0501/E0502) on each listed
/// file's own AST, plus Tier B (E0503 unused imports) over the merged
/// program when the file checks as a program. Libraries (no `main`)
/// get Tier A only; any other check failure refuses like `fmt`
/// (exit 2). Findings print as warnings; the exit is 0 unless
/// `--deny-warnings` turns findings into a failure (exit 1).
fn lint_files(files: &[String], deny_warnings: bool) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let dep_entries = match build_cmd::run_deps(&cwd) {
        Ok(d) => d,
        Err(code) => return code,
    };
    let mut any_findings = false;
    for path in files {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("toto: couldn't read `{path}`: {e}");
                return ExitCode::from(3);
            }
        };
        let (tokens, lex_errors) = toto_compiler::lexer::lex(&src);
        if !lex_errors.is_empty() {
            let diags: Vec<toto_compiler::diag::Diag> = lex_errors
                .into_iter()
                .map(|e| toto_compiler::diag::Diag::new(e.code, e.message, e.span, e.help))
                .collect();
            eprint!("{}", toto_compiler::render_errors(&diags, &src, path));
            return ExitCode::from(2);
        }
        let (program, parse_errors) = toto_compiler::parser::parse(tokens, &src);
        if !parse_errors.is_empty() {
            eprint!(
                "{}",
                toto_compiler::render_errors(&parse_errors, &src, path)
            );
            return ExitCode::from(2);
        }
        let mut hits = toto_compiler::lint::lint_file(&program);
        let has_own_main = program.items.iter().any(|item| {
            matches!(&item.kind, toto_compiler::ast::ItemKind::Fn(decl) if decl.name == "main")
        });
        match toto_compiler::project::load_and_check(
            path,
            &src,
            &|p| build_cmd::read_source(&cwd, p),
            &dep_entries,
        ) {
            Ok((project, _checked)) => {
                hits.extend(toto_compiler::lint::unused_imports(
                    &project,
                    &|p| build_cmd::read_source(&cwd, p),
                    &dep_entries,
                ));
            }
            Err(failed) => {
                // A main-less file is a library, not a broken program:
                // Tier A stands on its own. Any other check failure
                // invalidates the analysis, so refuse like `fmt`.
                let only_missing_main = !has_own_main
                    && failed
                        .errors
                        .iter()
                        .flat_map(|fd| fd.diags.iter())
                        .all(|d| d.code == "E0312");
                if !only_missing_main {
                    for fd in &failed.errors {
                        let f = &failed.files[fd.file];
                        eprint!(
                            "{}",
                            toto_compiler::render_errors(&fd.diags, &f.src, &f.path)
                        );
                    }
                    return ExitCode::from(2);
                }
            }
        }
        if !hits.is_empty() {
            any_findings = true;
            eprint!("{}", toto_compiler::render_warnings(&hits, &src, path));
        }
    }
    if deny_warnings && any_findings {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

/// Measure statement + branch coverage (Wave 5 Stage 3).
/// Each listed file runs as a program root through
/// the checked tree-walking interpreter; LCOV goes to stdout, the
/// human summary to stderr, and the covered program's own stdout is
/// suppressed (inspect it with `toto run`). Unreadable files exit 3;
/// broken (lex/parse/check) or concurrent programs are refused with
/// exit 2 — concurrent programs execute lowered MIR on the runtime
/// executor, which this tool cannot attribute honestly.
fn coverage_files(files: &[String], fail_under: Option<u32>) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let dep_entries = match build_cmd::run_deps(&cwd) {
        Ok(d) => d,
        Err(code) => return code,
    };
    let mut any_failed = false;
    for path in files {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("toto: couldn't read `{path}`: {e}");
                return ExitCode::from(3);
            }
        };
        let (tokens, lex_errors) = toto_compiler::lexer::lex(&src);
        if !lex_errors.is_empty() {
            let diags: Vec<toto_compiler::diag::Diag> = lex_errors
                .into_iter()
                .map(|e| toto_compiler::diag::Diag::new(e.code, e.message, e.span, e.help))
                .collect();
            eprint!("{}", toto_compiler::render_errors(&diags, &src, path));
            return ExitCode::from(2);
        }
        let (_program, parse_errors) = toto_compiler::parser::parse(tokens, &src);
        if !parse_errors.is_empty() {
            eprint!(
                "{}",
                toto_compiler::render_errors(&parse_errors, &src, path)
            );
            return ExitCode::from(2);
        }
        let (project, checked) = match toto_compiler::project::load_and_check(
            path,
            &src,
            &|p| build_cmd::read_source(&cwd, p),
            &dep_entries,
        ) {
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
        if checked.uses_concurrency {
            eprintln!(
                "toto: `{path}` uses concurrency: coverage runs sequential programs only \
                (the runtime executor lowers to MIR, which this tool cannot attribute); \
                refusing instead of reporting smoothed numbers \
                (tracked: docs/phases/post-wave5-backlog.md)"
            );
            return ExitCode::from(2);
        }
        let (outcome, hits) = toto_compiler::run_program_cov(&project.program, &checked.externs);
        let files_for_report: Vec<(String, String)> = project
            .files
            .iter()
            .map(|f| (f.path.clone(), f.src.clone()))
            .collect();
        let inv = toto_compiler::coverage::collect_inventory(&project.program);
        let (lcov, summary) = toto_compiler::coverage::render_lcov(&files_for_report, &inv, &hits);
        print!("{lcov}");
        eprintln!(
            "toto: coverage for `{path}`: {}% lines ({}/{}), {}% branches ({}/{}), {}/{} functions",
            summary.line_pct(),
            summary.lines_hit,
            summary.lines_found,
            summary.branch_pct(),
            summary.branches_hit,
            summary.branches_found,
            summary.fns_hit,
            summary.fns_found,
        );
        match outcome {
            (Ok(_), _) => {}
            (Err((diag, _trace)), _) => {
                eprintln!("{}", toto_compiler::render_errors(&[diag], &src, path));
                any_failed = true;
            }
        }
        if let Some(pct) = fail_under
            && summary.line_pct() < u64::from(pct)
        {
            eprintln!(
                "toto: coverage {}% below --fail-under {pct}",
                summary.line_pct()
            );
            any_failed = true;
        }
    }
    if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

/// Render API docs as Markdown (Stage 4): parse only, one file at a
/// time like `fmt` (imports are not resolved — each listed file is
/// one module named by its file stem). Stdout mode prints the index
/// plus every module as a single document; `--out DIR` writes
/// `<module>.md` per file plus `index.md`. Broken files are refused
/// with their diagnostics (exit 2); unreadable files are exit 3.
fn doc_files(files: &[String], out: Option<&str>) -> ExitCode {
    let mut modules: Vec<(String, String, String)> = Vec::new();
    for path in files {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("toto: couldn't read `{path}`: {e}");
                return ExitCode::from(3);
            }
        };
        let (tokens, lex_errors) = toto_compiler::lexer::lex(&src);
        if !lex_errors.is_empty() {
            let diags: Vec<toto_compiler::diag::Diag> = lex_errors
                .into_iter()
                .map(|e| toto_compiler::diag::Diag::new(e.code, e.message, e.span, e.help))
                .collect();
            eprint!("{}", toto_compiler::render_errors(&diags, &src, path));
            return ExitCode::from(2);
        }
        let (program, parse_errors) = toto_compiler::parser::parse(tokens, &src);
        if !parse_errors.is_empty() {
            eprint!(
                "{}",
                toto_compiler::render_errors(&parse_errors, &src, path)
            );
            return ExitCode::from(2);
        }
        let name = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.to_string());
        let summary = program.doc.first().cloned().unwrap_or_default();
        let md = toto_compiler::doc::render_module(&name, &program);
        modules.push((name, summary, md));
    }
    let entries: Vec<(&str, &str)> = modules
        .iter()
        .map(|(n, s, _)| (n.as_str(), s.as_str()))
        .collect();
    match out {
        None => {
            print!(
                "{}",
                toto_compiler::doc::render_index(&entries, &|n| format!("#{n}"))
            );
            for (_, _, md) in &modules {
                println!();
                print!("{md}");
            }
            ExitCode::from(0)
        }
        Some(dir) => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("toto: couldn't create `{dir}`: {e}");
                return ExitCode::from(3);
            }
            let root = std::path::Path::new(dir);
            let index = toto_compiler::doc::render_index(&entries, &|n| format!("{n}.md"));
            if let Err(e) = std::fs::write(root.join("index.md"), index) {
                eprintln!("toto: couldn't write `{dir}/index.md`: {e}");
                return ExitCode::from(3);
            }
            for (name, _, md) in &modules {
                if let Err(e) = std::fs::write(root.join(format!("{name}.md")), md) {
                    eprintln!("toto: couldn't write `{dir}/{name}.md`: {e}");
                    return ExitCode::from(3);
                }
            }
            ExitCode::from(0)
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Packaged programs run through the interpreter too: resolve
    // the manifest found from the working directory (empty when
    // there is no manifest or no `[dependencies]`).
    let dep_entries = match build_cmd::run_deps(&cwd) {
        Ok(d) => d,
        Err(code) => return code,
    };
    let (project, checked) = match toto_compiler::project::load_and_check(
        path,
        &src,
        &|p| build_cmd::read_source(&cwd, p),
        &dep_entries,
    ) {
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
