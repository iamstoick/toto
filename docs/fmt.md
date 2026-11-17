# `toto fmt` — canonical formatting (Phase 5, Stage 4)

`toto fmt [--check] <file.gzv>...` parses each file and re-emits it
in the canonical style. Each file formats independently; imports are
not resolved. `--check` lists the files that would change and exits
1 without writing. Broken files are refused with their diagnostics
(exit 2) — the formatter never guesses at broken syntax.

## Guarantees

- **Idempotent:** `fmt(fmt(x)) == fmt(x)` on every corpus file
  (pinned by `crates/toto-compiler/tests/fmt.rs`).
- **Semantics-preserving:** formatted sources run identically —
  same exit code and stdout on all 40 runnable sequential programs,
  same panic code + message on the panicking ones, and the 19
  concurrent programs plus all multi-file projects equivalent under
  the GC-pressure executor (`crates/toto-runtime/tests/fmt_equiv.rs`).
  Panic *locations* legitimately move; only the code + message are
  compared for panics.

## Canonical style

- 4-space indentation; blank line between top-level items; no blank
  lines inside blocks; trailing newline at end of file.
- One import per line; struct declarations single-line
  (`struct P { x: int, y: int }`); extern declarations one per line
  inside `unsafe extern "C" { ... }`.
- `else` holding exactly one trailing `if` prints as `else if`
  (same AST as the braced form).
- Binary operators spaced, precedence-parenthesized only when the
  reparse would differ; a redundant pair is preferred over a missing
  one. Statement `if` never takes `;`; everything else that needs
  one keeps it.

## Lossy by design

- **Comments vanish** (the lexer drops them; there is no comment
  attachment yet).
- **`pub` drops** (meaningless in the flat namespace).
- **Literals normalize:** hex ints become decimal, `1.` is not a
  float (spec), floats print shortest-round-trip (`1e300` stays
  exponential). String escapes canonicalize to the five valid ones;
  any other raw byte passes through verbatim.
- **No line-length limit:** long literals and signatures never wrap
  in v1. Annotation arguments print bare when they reparse to the
  same argument, quoted otherwise.
