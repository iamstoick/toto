# Phase 2 report: type checker + shared typed IR (HIR/MIR) + D8 freeze

- Branch: `phase-2/checker-ir` off `main` (`c7f9135`).
- PDF verified: title page reads **v2.3**, 29 pages
  (`toto-implementation-roadmap-v2-1.pdf`; the `-1` suffix is a download
  artifact, content is v2.3).
- Gate: `scripts/check.sh` green (fmt + `clippy -D warnings` + tests).
- Verdict: **all Phase 2 exit criteria met, proceed to Phase 3.**

## Stage 0 — repo hygiene + reconciliations

- `phase-1/interpreter` merged to `main` with merge commit `7c75926`
  (preserved history, no force-push), pushed to `origin/main`. CI green
  on `main`: run `43554655739` (also `43553272328` on the phase-1
  branch). Default branch is already `main` (verified).
- README: status line reads Phase 1 complete with roadmap v2.3
  reference (`c7f9135`); updated again here for Phase 2 (see below).
- Reconciliation 1 — unit-test count: the Phase 1 "23" was a miscount.
  Focused unit tests are **22** (lexer 7, parser 4, interp 8, cli 3 —
  sums to 22, not 23). Spike tests are 39 (spike1 7+1, stack1b 6,
  reload 10, jit 8, ffi 7). Current `cargo test --workspace` total is
  **65**: 39 spike + 22 focused + corpus runner 1 + differential 1 +
  MIR dumps 1 + Tier-1 conformance 1. All pass.
- Reconciliation 2 — E0212 fall-off: confirmed in spec (§5.2 as amended:
  falling off a `-> unit` function returns `()`; falling off any other
  function without returning is `E0212`). Both interpreters implement
  exactly this. No STOP needed.

## Stage 1 — type checker

Pipeline: lex → parse → **check** → interpret. `toto run` checks first;
any `E03xx` diagnostic exits 2 and nothing executes. Scope: Phase 1 core
subset plus `Result`/`Option`/`?` (checked AND executed); built-in
generics `Array<T>`, `Map<K,V>`, `Result<T,E>`, `Option<T>` checked and
monomorphization-ready. `chan<T>`, `go`, `<-`, FFI blocks, and user
generics/traits remain parser-rejected (`E0151`/`E0152`/`E0154`/`E0155`
still covered by `parse03/04/06/07`); `E0153`/`E0206` are retired
(`?`/constructors parse, check, and execute — spec §3.3/§11 updated).

Rules enforced (spec §12.1, the single authority): explicit fn-boundary
annotations, `let` types from initializers, no HM, reassignment to `let`
is `E0313`, shadowing allowed, struct/array element checking, strict
bool, if-branch agreement, `return` matches, cross-type `==` is `E0303`
(the Phase 1 runtime `E0203` survives only as an unreachable backstop —
no checked program can reach it, since every `==` operand pair is
same-typed), unused `Result` is `E0307` (an unused `Option` is legal: it
is not a result), `?` unwraps with identity error propagation for
identical error types and `E0308` otherwise (there is no `Error`
supertype and no other conversion in v1), frozen 25 builtins with fixed
signatures (builtins are callable by name only — `E0305` otherwise).
Constructors need context, never guessing: `Ok`/`Err` need `Result`
context, `None` needs `Option` context, `[]` needs an annotation, and
array literals do not propagate context inward (`[Ok(1)]` is `E0309`).
Recursive struct types are rejected (`E0302`): fields must resolve
against already-declared structs. Function types spell `fn(T1, T2) -> R`
— **provisional**, confirm or rename (isolated to the type parser).
Diagnostics use Phase 1 format with fresh `E03xx` codes, expected-vs-found
naming, suggestions, and multi-error preservation (capped at 16).

Corpus: all 48 surviving Phase 1 programs pass (2 retired, see below);
**36** negative type-error goldens in `tests/corpus/typing/` (every code
`E0301`–`E0313` covered, each output reviewed before blessing); **13**
positive `Result`/`Option`/`?` programs in `tests/corpus/results/`
(ok/err paths, `?` chains preserving `root-cause` through two levels,
Option paths, struct/array/closure contexts, discarded Option). Corpus
total: **97** programs (36 positive, 61 negative).

Retired/amended (premise changed by the phase, not by failure):
`diagnostics/diag09_phase2_ok.*` and `parsing/parse05_reject_result.*`
tested "Phase 2 constructs are rejected" — reborn as `typing/`
negatives; `arr02`/`clo04`/`eq04` amended for checked code (empty-array
annotation, `fn(int) -> int` parameter, homogeneous nesting — the
self-push cycle program is untypable, so the cycle guard is now a
runtime backstop tested below the checker); five static-error goldens
re-blessed for the check-first messages (reviewed, all correct);
`cyclic_display_terminates` runs the tree-walker directly (unchecked);
the parser `E0153` assertion became a parses-cleanly assertion.

## Stage 2 — HIR + MIR + MIR interpreter + differential harness

- HIR (`hir.rs`): typed, name-resolved AST. Every expression carries its
  type; every use carries its resolution (`VarRef` with binding depths);
  closures carry computed capture lists.
- Shared value semantics (`ops.rs`): arithmetic, div/mod, neg/not,
  equality, ordering, index/field read/write, `?` propagation — one
  implementation used by BOTH executors, extracted verbatim from the
  tree-walker (goldens prove no drift). The interpreter generalizes
  `ClosureVal` with a code enum (AST vs MIR bodies); each interpreter
  executes only its own variant.
- MIR (`mir.rs`): explicit CFG with `divcheck` before every integer
  `/`/`%`, `safepoint entry/loop/call/alloc` markers, `satincr` +
  explicit end-check for ranges (mirroring the saturating increment),
  array snapshots for `for`, closure conversion with explicit captures.
  Printable dumps with 6 goldens (`tests/mir_cases/`).
- MIR interpreter (`mir_interp.rs`): executes MIR with identical exit
  codes, stdout, panic codes/messages/spans, trace shape, exit-code
  handling, and the 8,192-frame limit. Reuses `Value`, `ops`, builtins,
  and the display machinery — no second value model.
- Differential harness (`tests/differential.rs`): all **36** positive
  corpus programs through both interpreters — exit code, stdout, and
  panic rendering identical. Green.
- Tier-1 ABI conformance (`tests/mir_conform.rs`): structural checks
  over dump cases + full positive corpus (divcheck guards, safepoint
  placement incl. dominator-based back-edge detection, entry markers).
  Green. (One real signal during development: the first back-edge
  heuristic misclassified `break`; dominators fixed the test, not MIR.)

## Stage 3 — D8 freeze + trap decision + conformance list

`docs/decisions/adr-008-runtime-abi.md` is **FROZEN**: untagged native
values with handle codec, platform-vs-internal layers, per-family
allocators + `toto_panic` added to Spike 3's nine (justified by the trap
decision), shadow-stack discipline with codegen auto-generation,
Spike 1b switch contract, ctx-first calls, collection-only-at-safepoints
rule. Reserved Phase 3 names (`yield/resume/spawn/chan_*`) with
unfrozen signatures. Tier 2 (representation-level) specified for Phase 3.

**Trap decision: explicit checks in codegen.** Spike 3's SIGFPE path
(exit 136, no span/trace/recovery) violates the §6 panic contract and
Cranelift offers no portable recovery; signals differ per OS/arch while
branches behave identically (differential testing requires this);
checks are predictable cold-path branches with no trap-table machinery.
MIR reflects it (`divcheck` ops, Tier-1 pinned). Reopen only with
measured justification AND a portable recovery scheme (stop-and-ask).

## Deviations and judgments

1. `?` conversion is identity-on-identical-error-types (spec §12.1) —
   the only sound reading with no `Error` supertype; recorded, not silent.
2. No inward context propagation through array literals (spec §12.1,
   reopens in spec if unergonomic).
3. `Propagate` is one explicit MIR op rather than split control flow
   (documented in `mir.rs`).
4. Non-semantic spec amendments only: §12 (type rules + codes), §3.3/§11
   retirement notes, CLI check-first wording (Phase 1 precedent).

## Carry-forwards to Phase 3

Confirm or rename `fn(T) -> R`; recursive types (currently `E0302`);
`Map` values/constructors; Tier-2 conformance execution; coroutine-stack
JIT (Phase 3 gate); `crates/toto-runtime/` still a scaffold (no GC/
scheduler integration in this phase — untouched).

## Counts

- `cargo test --workspace`: **65 passed, 0 failed** (39 spike + 22
  focused + 1 corpus + 1 differential + 1 mir dumps + 1 conformance).
- Corpus: **97** programs — 36 positive / 61 negative
  (diagnostics 11, interp 24, lexing 5, parsing 8, results 13,
  typing 36).
- `scripts/check.sh`: fmt clean, `clippy -D warnings` clean.
