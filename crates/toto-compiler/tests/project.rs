//! Project loader tests (Phase 5, Stage 1): resolution, cycles,
//! duplicates, and per-file attribution, all through the in-memory
//! `read` variant (no filesystem).

use std::collections::HashMap;
use toto_compiler::project::load_and_check;

fn load(
    entry: &str,
    files: &[(&str, &str)],
) -> Result<
    (
        toto_compiler::project::Project,
        toto_compiler::hir::CheckedProgram,
    ),
    toto_compiler::project::ProjectErrors,
> {
    let map: HashMap<String, String> = files
        .iter()
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect();
    let src = map.get(entry).cloned().unwrap_or_default();
    load_and_check(entry, &src, &|p| map.get(p).cloned())
}

const MAIN_OK: &str =
    "import \"lib/a.gzv\";\nfn main() -> int {\n    println(a());\n    return 0;\n}\n";
const LIB_A: &str = "import \"shared.gzv\";\npub fn a() -> int {\n    return shared();\n}\n";
const SHARED: &str = "pub fn shared() -> int {\n    return 1;\n}\n";

#[test]
fn diamond_loads_once_and_runs_merged() {
    let r = load(
        "main.gzv",
        &[
            ("main.gzv", MAIN_OK),
            ("lib/a.gzv", LIB_A),
            ("lib/shared.gzv", SHARED),
        ],
    );
    let (project, _) = r.expect("loads");
    // Entry first (DFS pre-order), shared deduplicated despite one edge.
    assert_eq!(project.files.len(), 3);
    assert_eq!(project.files[0].path, "main.gzv");
    assert_eq!(project.entry, 0);
    assert_eq!(project.file_of_fn("main"), 0);
    let shared_idx = if project.files[1].path == "lib/a.gzv" {
        2
    } else {
        1
    };
    assert_eq!(project.files[shared_idx].path, "lib/shared.gzv");
    assert_eq!(project.file_of_fn("shared"), shared_idx);
    assert_eq!(project.file_of_fn("a"), 3 - shared_idx);
}

#[test]
fn missing_import_is_e0401_at_the_import() {
    let err = load(
        "main.gzv",
        &[(
            "main.gzv",
            "import \"nope.gzv\";\nfn main() -> int {\n    return 0;\n}\n",
        )],
    )
    .expect_err("fails");
    assert_eq!(err.files.len(), 1);
    assert_eq!(err.errors.len(), 1);
    assert_eq!(err.errors[0].file, 0);
    assert_eq!(err.errors[0].diags.len(), 1);
    assert_eq!(err.errors[0].diags[0].code, "E0401");
}

#[test]
fn circular_import_is_e0402() {
    let err = load(
        "a.gzv",
        &[
            (
                "a.gzv",
                "import \"b.gzv\";\nfn main() -> int {\n    return 0;\n}\n",
            ),
            (
                "b.gzv",
                "import \"a.gzv\";\nfn bfn() -> int {\n    return 1;\n}\n",
            ),
        ],
    )
    .expect_err("fails");
    let codes: Vec<&str> = err
        .errors
        .iter()
        .flat_map(|f| f.diags.iter().map(|d| d.code))
        .collect();
    assert_eq!(codes, vec!["E0402"]);
    // Reported in the file that closes the cycle, at the import.
    assert_eq!(err.errors[0].file, 1);
}

#[test]
fn self_import_is_e0402() {
    let err = load(
        "a.gzv",
        &[(
            "a.gzv",
            "import \"a.gzv\";\nfn main() -> int {\n    return 0;\n}\n",
        )],
    )
    .expect_err("fails");
    assert!(
        err.errors
            .iter()
            .any(|f| f.diags.iter().any(|d| d.code == "E0402"))
    );
}

#[test]
fn cross_file_duplicate_is_e0311_in_later_file() {
    let err = load(
        "main.gzv",
        &[
            (
                "main.gzv",
                "import \"lib.gzv\";\nfn helper() -> int {\n    return 1;\n}\nfn main() -> int {\n    return 0;\n}\n",
            ),
            ("lib.gzv", "fn helper() -> int {\n    return 2;\n}\n"),
        ],
    )
    .expect_err("fails");
    let mut found = false;
    for f in &err.errors {
        for d in &f.diags {
            if d.code == "E0311" {
                // The duplicate lives in the later file (lib.gzv).
                assert_eq!(err.files[f.file].path, "lib.gzv");
                found = true;
            }
        }
    }
    assert!(found, "expected E0311");
}

#[test]
fn lib_type_error_attributes_to_lib() {
    let err = load(
        "main.gzv",
        &[
            (
                "main.gzv",
                "import \"lib.gzv\";\nfn main() -> int {\n    println(f(1));\n    return 0;\n}\n",
            ),
            (
                "lib.gzv",
                "pub fn f(x: int) -> int {\n    return \"bad\";\n}\n",
            ),
        ],
    )
    .expect_err("fails");
    let mut found = false;
    for f in &err.errors {
        for d in &f.diags {
            if d.code == "E0303" {
                assert_eq!(err.files[f.file].path, "lib.gzv");
                found = true;
            }
        }
    }
    assert!(found, "expected E0303");
}

#[test]
fn lex_errors_abort_parse_without_cascade() {
    // Mirrors single-file `parse_and_check`: no follow-on parse errors.
    let err = load(
        "main.gzv",
        &[(
            "main.gzv",
            "fn main() -> int {\n    # not a comment\n    return 0;\n}\n",
        )],
    )
    .expect_err("fails");
    let codes: Vec<&str> = err
        .errors
        .iter()
        .flat_map(|f| f.diags.iter().map(|d| d.code))
        .collect();
    assert!(
        codes.iter().all(|c| *c != "E0101" && *c != "E0102"),
        "no parse cascade: {codes:?}"
    );
    assert!(!codes.is_empty());
}

#[test]
fn stdlib_prefix_resolves_canonically() {
    // `gzv.std/` imports resolve independent of the entry's directory.
    let src = "import \"gzv.std/strings.gzv\";\nfn main() -> int {\n    println(pad_left(\"7\", 3, \"0\"));\n    return 0;\n}\n";
    let (project, _checked) = load_and_check("deep/nested/main.gzv", src, &|p| {
        toto_compiler::stdlib::get(p).map(str::to_string)
    })
    .expect("stdlib import resolves");
    assert_eq!(project.files.len(), 2);
    assert_eq!(project.files[1].path, "gzv.std/strings.gzv");
}

#[test]
fn stdlib_all_modules_merge_and_check() {
    // Every embedded module loads, merges, and checks as one program.
    let mut src = String::new();
    for (path, _) in toto_compiler::stdlib::MODULES {
        src.push_str(&format!("import \"{path}\";\n"));
    }
    src.push_str("fn main() -> int {\n    return 0;\n}\n");
    let (project, _checked) = load_and_check("main.gzv", &src, &|p| {
        toto_compiler::stdlib::get(p).map(str::to_string)
    })
    .expect("all stdlib modules check");
    assert_eq!(
        project.files.len(),
        1 + toto_compiler::stdlib::MODULES.len()
    );
}

#[test]
fn stdlib_table_is_complete() {
    assert_eq!(toto_compiler::stdlib::MODULES.len(), 9);
    for (path, src) in toto_compiler::stdlib::MODULES {
        assert!(
            path.starts_with("gzv.std/") && path.ends_with(".gzv"),
            "{path}"
        );
        assert!(!src.is_empty(), "{path} empty");
    }
}
