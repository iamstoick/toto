//! MIR dump goldens (Phase 2, Stage 2): representative programs lower to
//! the printed MIR in `mir_cases/*.mir`. Dumps pin the instruction set,
//! the explicit checks (`divcheck`), safepoint markers, and closure
//! conversion for Phase 3 codegen.

use std::path::{Path, PathBuf};

use toto_compiler::{check, lexer, mir, parser};

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("mir_cases")
}

#[test]
fn mir_dumps() {
    let dir = cases_dir();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("mir_cases readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no MIR dump cases found");
    let mut failures = Vec::new();
    for case in &files {
        let src = std::fs::read_to_string(case).expect("read case");
        let (tokens, lex_errors) = lexer::lex(&src);
        assert!(lex_errors.is_empty(), "{}: lex errors", case.display());
        let (program, parse_errors) = parser::parse(tokens, &src);
        assert!(parse_errors.is_empty(), "{}: parse errors", case.display());
        let (checked, check_errors) = check::check(&program);
        assert!(
            check_errors.is_empty(),
            "{}: check errors: {check_errors:?}",
            case.display()
        );
        let dump = format!("{}", mir::lower(&checked.expect("checked")));
        let golden_path = case.with_extension("mir");
        let golden = std::fs::read_to_string(&golden_path).unwrap_or_default();
        if dump != golden {
            failures.push(format!(
                "--- {}\n got:\n{dump}\n want:\n{golden}",
                case.display()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} MIR dump(s) diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
