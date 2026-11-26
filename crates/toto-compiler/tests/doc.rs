//! `toto doc` goldens: per-module Markdown plus the index.

use toto_compiler::doc::{render_index, render_module};
use toto_compiler::{lexer, parser};

fn parse(src: &str) -> toto_compiler::ast::Program {
    let (tokens, lex_errors) = lexer::lex(src);
    assert!(lex_errors.is_empty());
    let (program, parse_errors) = parser::parse(tokens, src);
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    program
}

#[test]
fn module_renders_sections_signatures_and_prose() {
    let src = "//! Math helpers.\n//!\n/// Adds one.\n/// Second line.\nfn inc(x: int) -> int {\n    return x + 1;\n}\n\nfn undocumented() -> int {\n    return 0;\n}\n\n/// A point.\nstruct Pt { x: int, y: int }\n";
    let prog = parse(src);
    let md = render_module("math", &prog);
    let expected = "# math\n\
        \n\
        Math helpers.\n\
        \n\
        \n\
        ## Functions\n\
        \n\
        ### inc\n\
        \n\
        ```toto\n\
        fn inc(x: int) -> int\n\
        ```\n\
        \n\
        Adds one.\n\
        Second line.\n\
        \n\
        ### undocumented\n\
        \n\
        ```toto\n\
        fn undocumented() -> int\n\
        ```\n\
        \n\
        ## Structs\n\
        \n\
        ### Pt\n\
        \n\
        ```toto\n\
        struct Pt { x: int, y: int }\n\
        ```\n\
        \n\
        A point.\n";
    assert_eq!(md, expected);
}

#[test]
fn empty_module_renders_title_only() {
    let prog = parse("fn main() -> int {\n    return 0;\n}\n");
    let md = render_module("main", &prog);
    assert_eq!(
        md,
        "# main\n\n## Functions\n\n### main\n\n```toto\nfn main() -> int\n```\n"
    );
}

#[test]
fn index_links_modules_with_summaries() {
    let modules = [("math", "Math helpers."), ("strings", "")];
    let files = render_index(&modules, &|n| format!("{n}.md"));
    assert_eq!(
        files,
        "# Index\n\n- [math](math.md) - Math helpers.\n\n- [strings](strings.md)\n"
    );
    let anchors = render_index(&modules, &|n| format!("#{n}"));
    assert_eq!(
        anchors,
        "# Index\n\n- [math](#math) - Math helpers.\n\n- [strings](#strings)\n"
    );
}
