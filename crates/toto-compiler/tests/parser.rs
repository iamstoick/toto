//! Focused parser tests: rejections, recovery, and suggestion codes.

use toto_compiler::lexer::lex;
use toto_compiler::parser::parse;

fn codes(src: &str) -> Vec<&'static str> {
    let (toks, lex_errors) = lex(src);
    assert!(lex_errors.is_empty());
    let (_, errors) = parse(toks, src);
    errors.iter().map(|e| e.code).collect()
}

fn prog(src: &str) -> toto_compiler::ast::Program {
    let (toks, lex_errors) = lex(src);
    assert!(lex_errors.is_empty());
    let (program, errors) = parse(toks, src);
    assert!(errors.is_empty(), "unexpected parse errors: {errors:?}");
    program
}

#[test]
fn bare_if_rejected_in_let_but_allowed_as_statement() {
    assert!(
        codes("fn main() -> int {\n let x = if true { 1 };\n return 0;\n}\n").contains(&"E0107")
    );
    // Statement position: no error.
    assert!(codes("fn main() -> int {\n if true { }\n return 0;\n}\n").is_empty());
    // If-else as tail: no error.
    assert!(codes("fn main() -> int {\n if true { 1 } else { 2 }\n}\n").is_empty());
}

#[test]
fn method_call_rejected_with_phase_hint() {
    let (toks, _) = lex("fn main() -> int {\n let a = [1];\n a.push(2);\n return 0;\n}\n");
    let src = "fn main() -> int {\n let a = [1];\n a.push(2);\n return 0;\n}\n";
    let (_, errors) = parse(toks, src);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E0158");
    assert!(errors[0].help.as_ref().unwrap().contains("functions only"));
}

#[test]
fn out_of_subset_names_its_phase() {
    // `go` is a Phase 3 statement: statement position parses, expression
    // position is E0101 (expected expression).
    assert!(codes("fn main() -> int {\n go { }\n return 0;\n}\n").is_empty());
    assert!(codes("fn main() -> int {\n let x = go { };\n return 0;\n}\n").contains(&"E0101"));
    // `?` is no longer out of subset: it parses since Phase 2 (spec §11).
    assert!(
        codes(
            "fn main() -> int {\n let x = f()?;\n return 0;\n}\nfn f() -> int {\n return 1;\n}\n"
        )
        .is_empty()
    );
    assert!(
        codes("fn id<T>(x: T) -> T {\n return x;\n}\nfn main() -> int {\n return 0;\n}\n")
            .contains(&"E0154")
    );
    assert!(codes("fn main() -> int {\n match x { }\n return 0;\n}\n").contains(&"E0156"));
}

#[test]
fn missing_return_type_is_an_error() {
    assert!(codes("fn main() {\n return 0;\n}\n").contains(&"E0101"),);
}

#[test]
fn import_and_pub_top_level_items() {
    use toto_compiler::ast::ItemKind;
    use toto_compiler::parse_and_check;

    // A well-formed import parses to an Import item carrying the path.
    let src = "import \"lib/util.gzv\";\nfn main() -> int {\n return 0;\n}\n";
    let (toks, _) = lex(src);
    let (program, errors) = parse(toks, src);
    assert!(errors.is_empty());
    assert_eq!(program.items.len(), 2);
    match &program.items[0].kind {
        ItemKind::Import(decl) => assert_eq!(decl.path, "lib/util.gzv"),
        other => panic!("expected Import item, got {:?}", other),
    }
    // `pub` is accepted on top-level items and has no effect.
    assert!(codes("pub fn main() -> int {\n return 0;\n}\n").is_empty());
    assert!(codes("pub struct P { x: int }\nfn main() -> int {\n return 0;\n}\n").is_empty());
    assert!(codes("pub import \"a.gzv\";\nfn main() -> int {\n return 0;\n}\n").is_empty());
    // A non-string import path is E0101; a missing `;` is E0102.
    assert!(codes("import foo;\nfn main() -> int {\n return 0;\n}\n").contains(&"E0101"));
    assert!(codes("import \"a.gzv\"\nfn main() -> int {\n return 0;\n}\n").contains(&"E0102"));
    // `mod` / `use` stay rejected; `import` is top-level-only.
    assert!(codes("mod m;\nfn main() -> int {\n return 0;\n}\n").contains(&"E0157"));
    assert!(codes("use \"a.gzv\";\nfn main() -> int {\n return 0;\n}\n").contains(&"E0157"));
    assert!(codes("fn main() -> int {\n import \"a.gzv\";\n return 0;\n}\n").contains(&"E0157"));
    // A surviving import reaches the checker as E0401 (single-file mode).
    let Err(diags) = parse_and_check("import \"a.gzv\";\nfn main() -> int {\n return 0;\n}\n")
    else {
        panic!("expected E0401 for a single-file import");
    };
    assert!(diags.iter().any(|d| d.code == "E0401"));
}

#[test]
fn doc_lines_attach_to_following_fn_and_struct() {
    use toto_compiler::ast::ItemKind;
    let src = concat!(
        "/// Adds one.\n",
        "/// Second line.\n",
        "fn f(x: int) -> int {\n",
        " return x;\n",
        "}\n",
        "/// A point.\n",
        "struct Pt { x: int }\n",
    );
    let p = prog(src);
    assert!(p.doc.is_empty());
    assert_eq!(p.items.len(), 2);
    assert_eq!(p.items[0].doc, vec!["Adds one.", "Second line."]);
    assert!(matches!(p.items[0].kind, ItemKind::Fn(_)));
    assert_eq!(p.items[1].doc, vec!["A point."]);
    assert!(matches!(p.items[1].kind, ItemKind::Struct(_)));
    // Undocumented items carry an empty doc.
    let p = prog("fn f() -> int {\n return 0;\n}\n");
    assert!(p.items[0].doc.is_empty());
}

#[test]
fn file_header_block_becomes_module_doc() {
    // `//!` lines at the top are the module doc; a `///` block
    // after them still belongs to the first item.
    let src = concat!(
        "//! Math helpers.\n",
        "//! Second line.\n",
        "/// First fn's own doc.\n",
        "fn f() -> int {\n",
        " return 0;\n",
        "}\n",
    );
    let p = prog(src);
    assert_eq!(p.doc, vec!["Math helpers.", "Second line."]);
    assert_eq!(p.items[0].doc, vec!["First fn's own doc."]);
    // Marker rule, not position rule: a `///` block at the very top
    // (no `//!` header) documents the first item, not the module.
    let src = concat!(
        "/// First fn's own doc.\n",
        "fn f() -> int {\n",
        " return 0;\n",
        "}\n",
    );
    let p = prog(src);
    assert!(p.doc.is_empty());
    assert_eq!(p.items[0].doc, vec!["First fn's own doc."]);
}

#[test]
fn misplaced_module_doc_is_an_error() {
    // `//!` past the file top is never an item doc: E0101 names the fix.
    let src = concat!(
        "fn a() -> int {\n",
        " return 0;\n",
        "}\n",
        "//! Oops, meant ///.\n",
        "fn b() -> int {\n",
        " return 1;\n",
        "}\n",
    );
    let (toks, _) = lex(src);
    let (_, errors) = parse(toks, src);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E0101");
    assert!(errors[0].message.contains("`//!`"));
    assert!(errors[0].help.as_deref().unwrap_or("").contains("`///`"));
}

#[test]
fn stray_docs_inside_bodies_are_dropped() {
    // Leading, trailing, and mid-body doc lines parse cleanly,
    // whatever the marker — bodies attach docs to nothing.
    let src = "fn f() -> int {\n/// leading note\n let x = 1;\n//! mid note\n return x;\n/// trailing note\n}\n";
    assert!(codes(src).is_empty());
    let p = prog(src);
    assert!(p.items[0].doc.is_empty());
}

#[test]
fn docs_interleave_with_annotations_and_attach_to_any_item() {
    use toto_compiler::ast::ItemKind;
    // Docs before/after `@...` and `pub` all land on the item.
    let src = concat!(
        "/// Doc first.\n",
        "@deprecated(\"old\")\n",
        "/// Doc second.\n",
        "pub fn f() -> int {\n",
        " return 0;\n",
        "}\n",
    );
    let p = prog(src);
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].doc, vec!["Doc first.", "Doc second."]);
    // Item docs are not fn-only: a doc above an import attaches to it.
    let src = concat!(
        "/// Why we import this.\n",
        "import \"a.gzv\";\n",
        "fn main() -> int {\n",
        " return 0;\n",
        "}\n",
    );
    let p = prog(src);
    assert_eq!(p.items[0].doc, vec!["Why we import this."]);
    assert!(matches!(p.items[0].kind, ItemKind::Import(_)));
}
