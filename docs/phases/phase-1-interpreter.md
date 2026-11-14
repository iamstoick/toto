# Phase 1 report: vertical slice, runnable core (interpreter)

Branch: `phase-1/interpreter`. Toolchain: `rustc 1.100.0`, edition 2024.
Gate: `./scripts/check.sh` (fmt + clippy `-D warnings` + `cargo test`) green;
62/62 tests pass (39 prior spike tests green, 23 new).

## 1. Stage 0 outcome — complete

- All five spike branches pushed to `https://github.com/iamstoick/toto`:
  `spike-1/gc-scheduler`, `spike-1b/stack-switching`, `spike-2/hot-reload`,
  `spike-3/cranelift-toy-ir`, `spike-ffi/smoke-test`.
- Merged to `main` in dependency order with merge commits (no fast-forward,
  no force-push, no history rewrite).
- Dual license added: `LICENSE-MIT` + `LICENSE-APACHE` (standard texts;
  Apache fetched from apache.org and verified byte-identical),
  `license = "MIT OR Apache-2.0"` in all 8 member manifests, README note
  replacing the license TODO (plus a stale "no CI config" paragraph updated).
- `.github/workflows/ci.yml` added (push + PR, ubuntu-latest, runs
  `scripts/check.sh`). Actions run 43548412647 on `main`: **success**
  (observed via `gh`), ~66 s.
- Push notes: the first push failed on missing credentials (stopped per the
  brief); the user logged in via `gh`, `gh auth setup-git` wired git to use
  it. The `main` push was then rejected for the workflow file
  (`workflow` OAuth scope missing); the user granted the scope and the push
  succeeded. Nothing was worked around; both stops were reported first.

## 2. Spec approval record

- `docs/spec.md` drafted (~13 pp), returned for review → STOP honored, no
  implementation before approval.
- Review 2026-11-14: APPROVED WITH AMENDMENTS. Applied verbatim:
  (1) unit type added (`unit` with `()` literal; procedures declare
  `-> unit`; fall-off returns `()` for unit fns, `E0212` otherwise);
  (2a) exit-code-1 collision documented as accepted;
  (2b) capture-by-value × shared-reference stated explicitly with example;
  (2c) `if`-as-expression requires `else` (statement-only otherwise);
  (2d) 8,192-frame limit marked interpreter-only;
  (2e) Phase 2 will reject cross-type `==` statically (note added);
  (3) all eight Appendix B answers recorded as binding (builtin catalog
  FROZEN at 24 names / 25 functions; `len` counts chars; `1.` is not a
  float; exit-code split approved);
  (4a) statement termination confirmed present (§3.2, explicit `;` rule);
  (4b) gap closed: functions-only, no method syntax (`E0158`), Appendix A
  items 15–16 added.
- One implementation amendment after approval (display-level, recorded
  here, not silent): stack traces longer than 22 frames truncate to the
  innermost 20 + outermost 2 with an omission count (spec §6 updated).
  Full 8,192-frame traces are unreadable and unusable as goldens.

## 3. Acceptance criteria (§4) — pass/fail with pointers

1. Stage 0 complete — PASS (§1 above; CI green, observed).
2. Spec gate honored — PASS (draft → review → amendments → approval;
   implementation started only after approval; record in §2).
3. Lexer — PASS. Full v1 token set incl. `<-` disambiguation
   (`crates/toto-compiler/src/lexer.rs`), spanned diagnostics;
   dedicated tests (`tests/lexer.rs`: `<-` vs `< -`, `0..10` vs `1.0`,
   `1.` rule, suggestion codes).
4. Parser — PASS. Core subset parses (`src/parser.rs`, Pratt, ~1300
   lines); out-of-subset rejected with phase codes `E0151`–`E0159`
   (corpus `parsing/parse03`–`parse08`: go/select, channels, `?`,
   generics/extern, match/import, method call); multi-error recovery
   demonstrated (`parse02`: 3 errors; `diag12`: 3 errors incl. unclosed
   delimiter with opening location); suggestions `E0106` (`=` vs `==`),
   `E0103` (unclosed), quote/comment/`&&` hints (`diag01`–`04`, `parse09`).
5. Interpreter — PASS. Sequential semantics per spec, demonstrated by
   `interp/`: arithmetic + overflow panic `E0201` with span, div-zero
   `E0202`, left-to-right evaluation order (`eval01_order`), equality incl.
   NaN/structural/cross-type panic (`eq01`–`eq05`), value/reference sharing
   + shadowing (`val01_sharing`), closures incl. capture-by-value vs shared
   + recursion + higher-order (`clo01`–`clo04`), loops with
   `break`/`continue` over ranges and arrays (`loop01`–`loop03`), structs
   with shape errors (`struct01`–`02`), arrays incl. OOB panic
   (`arr01`–`02`), frozen builtins (`str01_ops`, `int01_arith`).
6. Corpus — PASS. Exactly 50 programs (5 lexing + 9 parsing + 24 interp +
   12 diagnostics); every negative case fails readable (exit 1/2 with
   `error[Exxxx]`/`panic[Exxxx]` + snippet + help), never a Rust crash:
   the runner (`crates/toto-cli/tests/corpus.rs`) compares exact
   stdout/stderr/exit code, so a host panic or abort fails the test.
7. CLI — PASS. `toto run` executes end to end (`interp/cli01_fileprog`:
   reads a file, processes lines, prints results, writes output back);
   `--help`/`--version` (`toto 0.1.0`) and exit-3 paths covered by
   `crates/toto-cli/tests/cli.rs`.
8. Tests/gate — PASS. `cargo test` deterministic (no timing assertions;
   HashMap-ordering hazard in closure capture fixed by innermost-first
   capture + a regression test); 39 spike tests green; fmt + clippy clean;
   CI runs the same gate.
9. This report — present.

## 4. Corpus count and breakdown

50 programs under `tests/corpus/`, each with `.gzv` + `.stdout` +
`.stderr` + `.status` goldens: lexing 5 (`lex01_math`,
`lex02_strings`, `lex03_range_float`, `lex04_keywords`,
`lex05_bad_char`); parsing 9 (`parse01_precedence`,
`parse02_multierror`, `parse03_reject_go_select`,
`parse04_reject_channel`, `parse05_reject_result`,
`parse06_reject_generics_extern`, `parse07_reject_match_mod`,
`parse08_method_call`, `parse09_suggest_eq_unclosed`); interp 24
(`int01`–`03`, `eval01_order`, `eq01`–`05`, `val01_sharing`,
`str01_ops`, `clo01`–`04`, `loop01`–`03`, `struct01`–`02`,
`arr01`–`02`, `fn01_arity`, `cli01_fileprog`); diagnostics 12
(`diag01`–`12`). Focused unit tests alongside: lexer 7, parser 4,
interp 8, cli 3.

## 5. Diagnostics examples

Multi-error (`parse02_multierror.gzv`, exit 2) — three diagnostics, one
run: `E0102` missing `;` with `help: add `;`...`, `E0101` expected binding
name, `E0104` invalid assignment target. Suggestion (`parse09`, exit 2):
`E0106` on `if x = 1` with `help: did you mean `==`?...` plus `E0103`
`unclosed `(` opened at 6:12`. Runtime (`struct02_shape`, exit 1):
`panic[E0205]: missing field `y` in `Point` literal` + snippet + a
two-frame stack trace.

## 6. Interpreter design notes

Tree-walking interpreter (no VM, no justification needed): the job is to
DEFINE sequential semantics for Phase 4 differential testing, and a direct
AST evaluator is the smallest thing that cannot drift from the spec.
Value model (`interp.rs`): `Int(i64)` / `Float(f64)` / `Bool` / `Unit` /
`Str(Rc<String>)` / `Array(Rc<RefCell<Vec<Value>>>)` /
`Struct(Rc<RefCell<…>>)` / `Range` / `Closure(Rc<…>)` / `Func(idx)`.
No Spike 1 GC heap, no scheduler, no JIT — self-contained by design.
Control flow via a `Signal` enum (`Return`/`Break`/`Continue`/`Panic`);
every user-reachable failure maps to a `Diag` — no `unwrap`/`expect` on
user-influenced paths. Checked arithmetic (`checked_*`, `E0201`);
`to_int` range-checked; string `+` only; deep equality with a depth-100
cycle guard falling back to identity; `Display` with a `<cycle>` guard.
Recursion limit 8,192 enforced semantically; because the evaluator
recurses on the host stack, `run_source` executes on a dedicated 256 MiB
thread so the semantic limit (not host stack size) is what fires in both
debug and release builds. For-loop over arrays iterates a snapshot so
`push`/`pop` in the body cannot invalidate iteration.

## 7. Files changed/added

- `crates/toto-compiler/src/`: `span.rs`, `token.rs`, `lexer.rs`,
  `ast.rs`, `parser.rs`, `diag.rs`, `interp.rs`, `builtins.rs`, `lib.rs`
  (rewritten from empty scaffolds; `Outcome` + `run_source` API).
- `crates/toto-cli/src/main.rs` (CLI), `tests/corpus.rs` (runner),
  `tests/cli.rs` (flags/exit-3).
- `crates/toto-compiler/tests/{lexer,parser,interp}.rs`.
- `tests/corpus/` (50 programs + 150 goldens), `docs/spec.md` (approved +
  one truncation amendment), `Cargo.toml` bumps (`toto-cli`,
  `toto-compiler`, `toto-runtime` 0.1.0), `Cargo.lock`.
- `crates/toto-runtime/` stays empty for Phase 3 (one-line scaffold
  untouched).

## 8. Deviations, assumptions, open questions

- Bugs found by reviewing generated outputs before blessing goldens
  (all fixed, all with regression coverage): statement-`if`/tail
  disambiguation; `Ident {` vs control-flow body (maximal-munch lookahead:
  struct literal iff `{Ident:` or empty `{}` in full-expression position);
  `Ok/Err/Some/None` parse as calls so `E0206` fires at runtime; trace
  captured before frame pop; missing `(` consume in method-call recovery;
  top-level `import` now `E0157`; 256 MiB interpreter thread; builtin
  frames in traces; innermost-first closure capture (determinism).
- Judgment calls (spec-silent, documented): for-loop iterates a snapshot;
  range `i64::MIN..i64::MAX` saturates instead of wrapping; empty-dict
  `split` with empty delimiter splits chars; `parse_int` trims ASCII
  whitespace; for-loop variables are immutable; duplicate top-level names
  resolve last-wins.
- Open for Phase 2: static enforcement of everything the interpreter
  currently ignores (annotations, return-type/arity checking is runtime
  only, cross-type `==`, must-use); `Range` iteration of huge ranges is
  linear (no laziness needed in v1).

## 9. Verdict and recommendation

Phase 1 exit criteria MET. Recommend proceeding to Phase 2 (checker +
shared IR + D8 freeze). No rework, no re-spike.
