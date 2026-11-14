//! Focused lexer tests: full v1 token shapes the corpus only exercises
//! through runnable programs.

use toto_compiler::lexer::lex;
use toto_compiler::token::TokenKind;

fn kinds(src: &str) -> Vec<TokenKind> {
    lex(src).0.into_iter().map(|t| t.kind).collect()
}

#[test]
fn channel_operator_lexes_as_one_token() {
    // `ch <- val`: no whitespace — one `<-` token (spec §2.3).
    assert!(matches!(kinds("ch <- val")[1], TokenKind::LtMinus));
}

#[test]
fn less_than_with_spaced_negation_stays_separate() {
    // `a < -b`: whitespace — `<` then unary minus.
    let k = kinds("a < -b");
    assert!(matches!(k[1], TokenKind::Lt));
    assert!(matches!(k[2], TokenKind::Minus));
}

#[test]
fn range_vs_float() {
    let k = kinds("0..10");
    assert!(matches!(k[0], TokenKind::Int(0)));
    assert!(matches!(k[1], TokenKind::DotDot));
    assert!(matches!(k[2], TokenKind::Int(10)));
    let k = kinds("1.0");
    assert!(matches!(k[0], TokenKind::Float(_)));
}

#[test]
fn trailing_dot_is_not_a_float() {
    // `1.` lexes as `1` then `.` (spec §2.4); the parser rejects the dot.
    let k = kinds("1.0 1.");
    assert!(matches!(k[0], TokenKind::Float(_)));
    assert!(matches!(k[1], TokenKind::Int(1)));
    assert!(matches!(k[2], TokenKind::Dot));
}

#[test]
fn bad_characters_suggest_fixes() {
    let (_, errors) = lex("a & b");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E0001");
    assert!(errors[0].help.as_deref() == Some("did you mean `&&`?"));
    let (_, errors) = lex("'x'");
    assert_eq!(errors.len(), 2);
    assert!(errors[0].help.as_deref() == Some("strings use double quotes: `\"x\"`"));
    let (_, errors) = lex("# c");
    assert!(errors[0].help.as_deref() == Some("comments start with `//`"));
}

#[test]
fn int_overflow_is_a_lexer_error_not_a_panic() {
    let (toks, errors) = lex("99999999999999999999999");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E0004");
    assert!(matches!(toks[0].kind, TokenKind::Int(0)));
}

#[test]
fn string_escapes_and_errors() {
    let (toks, errors) = lex(r#""a\nb\x""#);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E0003");
    match &toks[0].kind {
        TokenKind::Str(s) => assert_eq!(s, "a\nb"),
        other => panic!("expected string, got {other:?}"),
    }
    let (_, errors) = lex("\"unterminated");
    assert!(errors.iter().any(|e| e.code == "E0002"));
}
