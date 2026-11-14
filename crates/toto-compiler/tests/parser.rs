//! Focused parser tests: rejections, recovery, and suggestion codes.

use toto_compiler::lexer::lex;
use toto_compiler::parser::parse;

fn codes(src: &str) -> Vec<&'static str> {
    let (toks, lex_errors) = lex(src);
    assert!(lex_errors.is_empty());
    let (_, errors) = parse(toks, src);
    errors.iter().map(|e| e.code).collect()
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
    assert!(codes("fn main() -> int {\n go { }\n return 0;\n}\n").contains(&"E0151"));
    assert!(
        codes(
            "fn main() -> int {\n let x = f()?;\n return 0;\n}\nfn f() -> int {\n return 1;\n}\n"
        )
        .contains(&"E0153")
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
