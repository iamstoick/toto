# Enterprise Wave 5 brief — developer tooling

Scope approved by the user 2026-11-24: all four tooling surfaces,
in order — Stage 1 linter, Stage 2 richer LSP, Stage 3 coverage,
Stage 4 doc generator. Each stage gated; no stage re-opens a
settled decision.

Status: STAGE 0 VERIFIED (run #118 green on the starting
tree — see Stage 0); STAGE 1 IMPLEMENTED (awaiting CI);
STAGE 2 CAPABILITIES IMPLEMENTED + proven against an
independent client stack (GUI editor session outstanding);
STAGE 3 DESIGN APPROVED, IMPLEMENTATION PENDING.

Branch: `enterprise/wave-5-tooling`, from `main` at `2c7a456`.
Never force-push; stage named files only (`git add`, never `-A`).
No merge until the wave gates green.

## Why this order (recorded)

1. Linter first: it builds the diagnostic infrastructure and the
   enterprise code-standards story, and Stage 2's code actions
   wrap the same diagnostics — lint first, surface second.
2. Richer LSP second: the adoption unlock. Completion,
   references, rename, and code actions are what make
   developers willing to live in the language all day.
3. Coverage third: it extends the gate culture (counts, gates,
   green proofs) into release readiness — enterprises gate
   releases on coverage.
4. Doc generator last: its value compounds once lint + LSP have
   stabilized how code is written and read.

## Stage 0 — starting state + carry-forward disposition

Starting state: `main` at `2c7a456` (Wave 4 hygiene: run #117
recorded, stale flake-addendum (d) fixed). Last CI-confirmed
green: run 45379738715 (#117, head 526660d) — check +
release-gates + bench + macOS/Linux ARM64 probes green,
Windows probe informational under continue-on-error. Run
45399390212 (#118, head 2c7a456) concluded SUCCESS
(run conclusion success, updated 2026-11-24T13:49:53Z) — per
job, all completed: check success; release-gates success; bench
success; tier1-probe macos-latest success; tier1-probe
ubuntu-24.04-arm success; tier1-probe windows-latest failure,
informational under continue-on-error, the ONLY failing job.
Stage 0 verification is therefore COMPLETE: the starting tree
is CI-confirmed green, and Stage 1 work may be staged.
Lag-by-one noted: pushing this brief (commit ac4a9f9) triggers
a further CI run on the new HEAD; that run's outcome belongs in
the next record, not here — the new HEAD is NOT claimed as
confirmed.

Standing authority carried in: D1–D8 FROZEN; 27 builtins frozen
(zero new builtins this wave); Phase 5 module model APPROVED;
ADR-009 + spec §17/§18 APPROVED as written 2026-11-21; Wave 0
policies APPROVED 2026-11-19; Wave 2 ACCEPTED-WITH-FLAGS and
Wave 3 ACCEPTED-WITH-FLAGS stand as written. Windows DEFERRED
WITH EVIDENCE; SUPPORTED platforms only (Linux x86_64, macOS
ARM64, Linux ARM64). `toto dev` single-file-only stays a known
open limitation (dev-loop workstream, not this wave).

Carry-forward dispositions (nothing disappears silently):

- LSP current state ([docs/lsp.md](/Users/gerald/Apps/toto/docs/lsp.md)):
  hover, go-to-definition, and publishDiagnostics over stdio
  with a dependency-free hand-rolled JSON layer — proven
  headless/protocol-only (3 tests over piped stdio), NOT in a
  real editor. Stage 2 must prove the server inside a real
  editor session with the editor + version recorded (mirror of
  Wave 4's "proven against a real Jaeger collector" rule).
  Known limits carried until their stage: UTF-16 column
  mismatch, one-document-at-a-time modeling, `file://` URIs
  only, no hover/definition notion of comments.
- Span-level race attribution: deferred Enterprise scope,
  NOT this wave (profiler work, not tooling UX).
- E0216 root cause, 1-thread pool wedge, recv/send unbounded
  peer waits, SIGBUS per-program log-text confirmation, TOML
  empty-table reporting, YAML unquoted-trailing-colon re-parse,
  MySQL `db_exec` empty-misuse error string: carried, not this
  wave's scope.
- Cross-process / cross-service propagation format (ADR-009
  Wave 5+): explicitly OUT — Wave 5 is tooling surfaces, not a
  runtime wave.
- Package manager + registry, LLVM/production AOT (Phase 7);
  user generics/traits; general optimization; HTTP client
  (unneeded); benchmarks suite: carried, not this wave.

Report hygiene (binding, lesson from the Wave 3/4 merges):
this file is append-only and earlier text is preserved verbatim
— but every correction this wave states explicitly whether it
was "recorded in dated notes" or "fixed in place". No section
claims an in-place fix that did not happen.

## Stage 1 — linter (DESIGN APPROVED 2026-11-24; no code yet)

Goal: `toto lint` over whole programs: a seeded rule set for
enterprise code standards with spanned diagnostics in stable,
documented codes, runnable in CI as a gate.

Design decisions (user-approved 2026-11-24):

- Rule scope: hygiene only. Dead code, unused bindings,
  unreachable code. Zero false-positive tolerance on the
  repo's own corpus (272 `.gzv` files: stdlib + tests +
  bench). Style rules and suspicious-idiom rules are NOT in
  the seed set — proposed as named follow-ups, not silent
  extensions.
- Code scheme: fresh E05xx block (entirely free — in use are
  E00xx, E01xx incl. E015x, E02xx, E03xx, E04xx, E0601).
  Reuses `Diag` (code + message + span + help) and the spec
  §9.1 rendering unchanged, and flows into the existing LSP
  `publishDiagnostics` path with no fork. No L-prefix family.
- CLI shape: mirrors `toto fmt` (hand-rolled dispatch in
  `crates/toto-cli/src/main.rs`, no framework): `toto lint
  [--deny-warnings] <file.gzv>...`. Exit nonzero on findings;
  `--deny-warnings` upgrades warnings for CI gating. No
  project-wide bare mode (the CLI has no project-root
  resolution yet — owned by a later turn, not this stage).

Acceptance: seed rules fire on purpose-built positive cases;
the full 272-file corpus lints with zero false positives;
every diagnostic carries a span and a stable E05xx code;
timing budget stated and met (measured against the bench
corpus); at least one rule records an auto-fixable shape for
Stage 2 code actions. Added to the CI gate only after the
corpus-clean proof lands.
Non-goals: formatting (no `toto fmt` here — it exists),
editor integration (Stage 2 surfaces these diagnostics),
project-wide mode.

Implementation record (Stage 1 built as designed, with two
evidence-driven narrowings — no silent drift):
- Codes: E0501 unreachable statement/expression, E0502 unused
  binding, E0503 unused import. Findings reuse `Diag` and
  render as `warning[E05xx]` (same spans/snippets as errors);
  exit 0 with findings, exit 1 only under `--deny-warnings`.
  This resolves the design's "exit nonzero on findings" line:
  nonzero happens under the CI flag. Refusals (unreadable,
  unlexable, unparseable, uncheckable files) stay exit 2/3,
  mirroring `toto fmt` and `toto run`.
- E0502 scope narrowed by corpus evidence: function/closure
  parameters and `for` variables are NOT flagged. Parameters
  are interface conformance (every HTTP handler in the corpus
  keeps an unused `req`); index-ignoring counting loops are
  idiomatic (`for i in 0..n`). E0502 covers plain `let` and
  `select`-receive bindings (including receive-initialized
  lets: the binding is checked, the receipt still happens).
  Shadowing is tracked per scope (spec §5.6 allows it); names
  starting with `_` are the documented opt-out. E0501 is
  unchanged (statements AND block tails after terminators).
- Tiers: Tier A runs on each listed file's own AST (no type
  info needed), so libraries lint the same as programs. Tier B
  (E0503) runs over the merged checked program: a listed
  file's import edge is dead only when no item reachable
  through it is referenced anywhere; need flows transitively
  through conduit files; files carrying `state`/`migrate`
  items are exempt (loading runs initializers); unresolvable
  edges are skipped, never flagged. A main-less file whose
  only check failures are missing-`main` E0312 gets Tier A
  only; any other check failure refuses like `fmt`.
- Corpus proof: `corpus_tier_a_clean` (in `lint.rs`) lints
  every parseable repo program outside the error-goldens —
  144 files silent, 0 findings, 0.11 s in-process; CLI sweep
  over all 263 files: 167 exit 0, 96 exit 2, and every
  refused file verified genuinely broken (zero status-0
  goldens among them). 11 files excluded via
  `tests/lint-exceptions.txt`, each a TRUE positive on
  intentional scaffolding with its reason in that file
  (dead-send fixture, drains/holds, validation-by-call `sn`,
  leftover test scaffolding, 2 broken support fixtures).
  `toto lint` on 3 large stdlib files: 67 ms wall.
- Gate: `scripts/check.sh` runs `toto lint --deny-warnings`
  over the same file set (error-goldens skipped, same
  exceptions file — proof and gate cannot drift). 8 CLI
  tests (`crates/toto-cli/tests/lint.rs` + fixtures) cover
  each rule, shadowing, the `_` opt-out, library mode,
  used/unused imports, refusal, and exit codes.
- Stage 2 hooks recorded, not built: E0501's fix shape is
  "remove it"; E0502's is "remove it or prefix `_`"
  (call-preserving deletion for effectful initializers stays
  Stage 2 design work).

## Stage 2 — richer LSP (DESIGN APPROVED 2026-11-25; no code yet)

Goal: completion, references, rename, and code actions on the
existing stdio server, multi-document modeling, wrapping
Stage 1 diagnostics at minimum.

Design decisions (user-approved 2026-11-25):

- Landing order: references → rename → completion → code
  actions. Dependency order, each tested before the next:
  rename needs the reference finder; code actions need lint
  surfacing built on the same program model. (Server ground:
  `OpenDoc` already holds the merged `project` + `checked`
  program with per-file sources; `resolve_at`/`resolve_global`
  give the definition model the finder inverts.)
- Column encoding: UTF-16 units, per the LSP standard. This
  changes existing hover/definition columns on non-ASCII
  files (disclosed here, covered by tests); scalar counting
  stays only as the documented past behavior in `lsp.md`.
- Lint in LSP: E05xx findings surface as warning-severity
  `publishDiagnostics` (the `starts_with('E') → error`
  severity mapping is fixed: E05xx are warnings), and code
  actions fix them in place ("remove it", "prefix `_`",
  "remove the import" — the Stage 1 recorded fix shapes).
- Document model: unchanged (one project root per open file,
  cross-file resolution from disk with the open buffer
  overriding; `file://` URIs only stays). Rename returns a
  multi-file `WorkspaceEdit`; unopened files resolve from
  disk at request time — documented limitation, not silent.
- Proof vehicle: VS Code (installed on this machine;
  exact version recorded at implementation) driven through
  its LSP APIs, exercising all four capabilities in one
  recorded session. The existing piped-stdio tests stay as
  regression cover, not as the proof.

Acceptance: the four capabilities in order, each with
protocol tests; UTF-16 columns proven on non-ASCII fixtures;
E05xx warnings visible with severity 2 and fixable via code
actions; real-editor session recorded (editor + version);
broken-file behavior keeps last-good data out (established
`lsp.md` contract holds). Non-goals: debugging protocol,
untitled-buffer support, formatting (no `toto fmt` here).

Implementation record — references + UTF-16 (rename next):
- UTF-16 first (all new ranges depend on it):
  `offset_to_position`/`position_to_offset` count UTF-16 code
  units; mid-pair requests resolve to the following boundary
  (documented; clients must send code-point boundaries).
  `docs/lsp.md` updated (scalars stay as the documented past
  behavior). Proven by an astral-character protocol test that
  fails on scalar counting.
- `textDocument/references` with `includeDeclaration`:
  capability advertised; null-doc/unresolvable/builtin yield
  null/empty (builtins `[]`). Finder reuses `resolve_at` for
  the target, then walks every project file with the same
  scoping rules: locals match by declaration-span identity
  (shadowing exact, cross-function leaks impossible);
  functions/externs/state match by name only where no local
  shadows; structs add literal names, `Named` type refs, and
  the tied `migrate fn` name; fields match by name only when
  exactly one struct declares the field, else empty
  (rename safety — never a wrong set). Field use spans are
  whole `base.name` expressions for now; rename refines them
  to name spans. Unparseable files are skipped, never fatal.
- Declaration-site fix (needed for rename-from-declaration):
  `resolve_at` now resolves `fn` parameter names to
  `Target::Local`, like closure parameters already did — this
  also fixes hover there (no existing test covered it).
  Migrate bodies stay position-unresolvable (existing hover
  gap, unchanged); their uses still count as references.
- Tests: 4 new protocol tests (shadowing, cross-file,
  struct/field/param-decl, UTF-16) + capability assert; 8/8
  `lsp` tests green. Neighbors unchanged: `cli` suite same
  13/5 split as the untouched base (5 pre-existing
  build/dist network failures); `fmt` clean; zero new
  compiler warnings. Clippy rides on CI (no local component:
  static.rust-lang.org unresolvable on this machine).

Implementation record — rename (completion next):
- `textDocument/rename` returns a `WorkspaceEdit`
  (`renameProvider: true`); refusals are protocol errors,
  never partial edits: unknown target and builtins (-32803),
  invalid identifier or keyword new name (-32602), duplicate
  top-level or field name, and capture (-32803). Same-name
  rename succeeds with empty changes. No `prepareRename`
  (clients fall back to word range; documented).
- Exact spans everywhere: field uses now carry name spans
  (the parser merges the whole `base.name` span through the
  name token, so the name is exactly the trailing span —
  verified against the parser, not assumed). The references
  finder shares the representation (field locations tightened
  accordingly).
- Soundness rules, each tested: a local rename is refused
  when the new name is bound in the same function or used
  as a global there (shadowing + capture, function-level
  precision so other functions' locals do not block it); a
  global rename is refused on top-level duplicate and when
  the new name is bound as a local in any edited file; a
  struct rename needs only top-level uniqueness (type and
  literal positions are syntax-determined); a field rename
  needs field-name uniqueness (else refused) plus no clash
  in the owning struct. New names must be valid identifiers,
  non-keywords, and (outside fields) non-builtins.
- Tests: 5 new protocol tests (local incl. no-op, cross-file
  fn, field name-span edits, 5 refusal kinds, local→global
  capture); 13/13 `lsp` tests green. Neighbors unchanged
  (same `cli` 13/5); `fmt` clean; zero new warnings.

Implementation record — completion (code actions next):
- `textDocument/completion` with `triggerCharacters: ["."]`:
  unqualified names in scope (visible locals + params,
  project globals, builtins, keywords) with kind, detail,
  sortText, and prefix-replacing UTF-16 text edits; member
  position (`base.`) offers the struct's fields with
  `field: Type` details. Unknown base, strings, and comments
  yield `[]`, never wrong items.
- Member types come from declarations, not inference:
  annotated struct types and struct-literal initializers
  (both AST-only, so completion works on broken files).
  A trailing dot never parses, so a synthetic placeholder
  is inserted at the cursor for parsing only (spans before
  the cursor are unaffected). Call results, loop variables,
  and unresolvable bases yield nothing — documented, not
  guessed.
- Scope discipline mirrors `resolve_*`: statements starting
  after the cursor are not walked, so later bindings and
  untaken-branch bindings never leak in (dedup keeps the
  innermost binding on shadowing).
- Tests: 5 new protocol tests (unqualified incl. shadow
  exclusion, member from literal and from annotation, unknown
  base empty, capability); 18/18 `lsp` tests green.
  Neighbors unchanged (same `cli` 13/5); `fmt` clean; zero
  new warnings.

Implementation record — code actions (editor proof next):
- `textDocument/codeAction` returns quickfixes for the open
  file's published lint findings overlapping the request
  range (recomputed nowhere: the actions read the same
  stored set `publishDiagnostics` reported). E0501 removes
  the unreachable code, E0502 prefixes the binding with `_`,
  E0503 removes the unused import; refusals are simply no
  actions (clean range, unknown target kind). Deletions are
  line-aware: a statement standing alone on its lines takes
  the whole lines, a shared-line statement only its span.
- Lint now surfaces in `publishDiagnostics`: Tier A on the
  open buffer plus Tier B over a successful load (same
  library rule as the CLI: missing-`main`-only gets Tier A,
  other breakage gets check errors only). A shared
  `diagnostic_json` maps E05xx to severity 2, other `E` codes
  to 1.
- Tests: 2 new protocol tests (all three fix kinds with
  exact ranges/newText, severity-2 diagnostics, clean-range
  empty, cross-file import removal); 20/20 `lsp` tests green.
  Neighbors unchanged (same `cli` 13/5); `fmt` clean; zero
  new warnings. `codeActionProvider` advertised.

Implementation record — reference-client proof (2026-11-25):
- All four capabilities plus diagnostics proven end to end
  against an INDEPENDENT client stack: Microsoft's
  `vscode-jsonrpc` 10.0.1 (message framing + JSON-RPC) and
  `vscode-languageserver-types` 3.18.6 on node v25.8.1,
  driving the built `toto` 0.1.0 server over stdio — the
  same protocol stack VS Code extensions use, with none of
  the test-harness framing code. Harness (throwaway, kept at
  `/tmp/toto-lsp-proof/node/proof.js` for re-runs, not
  committed): fixture project plus trailing-dot and unknown
  cases; 31/31 checks green — capability advertisement
  (all six incl. trigger `.`), E0501/E0502/E0503 published
  as severity-2 warnings, hover signature, cross-file
  definition, references (lib decl + main use),
  cross-file rename (`doubled` in both files), unqualified
  completion (locals, globals, builtin, keyword, textEdit
  range), member completion (`x`, `y`, `field: int`),
  all three code-action fixes, empty result on clean code,
  and UTF-16 resolution of an astral-adjacent use.
- What this is NOT: a GUI session. VS Code 1.147.0 is
  installed on this machine (editor version on record), but
  this sandbox has no display, so no window could be opened.
  The GUI run — same four capabilities exercised live in
  VS Code, version recorded — remains the final acceptance
  step and needs a display (e.g. run the harness scenarios
  in the user's own VS Code). Nothing in the protocol
  record depends on it: every server behavior it could
  observe is already pinned by the 20 repo tests plus the
  31 reference-stack checks.

# Dated addendum — CI on the Stage 1 tree (2026-11-25, on main; append-only)
- Run 45553139355 (#119, head 83952b6): check FAILED on one
  `cargo fmt --check` hunk in `lint.rs` (the local rustfmt
  1.87 wrapped a line the CI toolchain keeps joined);
  everything else in the gate did not run (`set -eu` stops
  at the first failure). Diagnosis from the CI log (fetched
  with `gh`, no 403 this time). Fixed in place in commit
  09bf9db (`Wave 5 Stage 1: match CI rustfmt...`): the exact
  CI-requested shape, verified byte-identical against the
  logged diff. Windows probe failed informationally as usual
  (net-FFI mir_conform, continue-on-error). This commit
  triggers a fresh run that exercises clippy, the full
  suite, and the new lint gate for the first time; its
  outcome belongs in the next record, not here.

## Stage 3 — coverage (DESIGN APPROVED 2026-11-25; no code yet)

Goal: statement/branch coverage for `.gzv` programs with a
machine-readable report plus a gate integration.

Design decisions (user-approved 2026-11-25):

- Units: statements + branches. Every statement plus `if` /
  `while` / `select` arm outcomes, attributed exactly in the
  tree-walking interpreter (`exec_stmt` and branch paths are
  the hook points — AST spans, no mapping loss). Function
  totals fall out as free aggregation over the same data.
- Executor scope: sequential interpreter only (`toto run`
  path). Concurrent programs execute lowered MIR on the M:N
  scheduler, where source-line mapping is approximate and
  counts vary run to run — the runtime-executor follow-up
  is named here, not silently dropped. `toto coverage`
  refuses concurrent programs loudly (same honesty rule as
  the JIT probe's single-file limit), documents why, and
  exits non-zero; it never reports smoothed numbers.
- Report + CLI: `toto coverage [--fail-under PCT]
  <file.gzv>...`, mirroring `toto lint` (hand-rolled
  dispatch, per-file program roots, broken files refused
  exit 2 like `fmt`). Writes standard LCOV
  (`SF:`/`DA:`/`BRDA:` records, `end_of_record`) plus a
  one-line human summary; line-based text needs no new
  dependencies and works with coveralls/codecov tooling.
  `--fail-under` exits 1 below the threshold; the gate
  demonstration runs it against a corpus program.
- Overhead: measured on the bench corpus (compute/gc/stdlib
  workloads) and recorded here at implementation; budget met
  or the stage does not close. Self-measurement: the corpus
  program's own LCOV is golden-tested (counts pinned).

Acceptance: seed implementation with the units above;
LCOV fixed and golden-tested; self-measurement demonstrated
on a corpus program; overhead budget stated and met on the
bench corpus; `--fail-under` demonstrated failing below
threshold and passing at it. Non-goals: concurrent-program
coverage (named follow-up), branch precision beyond honest
attribution (disclosed, not smoothed), HTML reports
(LCOV consumers already exist).

## Stage 4 — doc generator

Goal: `toto doc` rendering API docs for `.gzv` sources
(stdlib first), from a documented doc-comment convention.

Acceptance: renders the full `gzv.std` surface; output
golden-tested; the comment convention recorded (it interacts
with the LSP's "no notion of comments" limit — resolved or
carried explicitly). Non-goals: a hosted site, versioned
publishing (registry work is Phase 7).

## Wave gates (no merge until all green)

- Each stage: design recorded before implementation; code +
  gate green (`./scripts/check.sh` exit 0 locally + CI green
  on the branch tip); zero new builtins; no D1–D8, ADR, module
  model, or spec change — deviations get their own dated
  deltas, never silent drift.
- Stage 2 proven in a real editor session, named and recorded.
- Report hygiene holds end to end (append-only + explicit
  recorded/fixed-in-place statements).
- Verdict recorded here at close, approved by the user with a
  date, like Wave 4.

## What this brief does NOT do

- Does NOT write code, create the branch, or run CI.
- Does NOT change D1–D8, ADR-008, the module model, the
  27-builtin catalog, ADR-009, or spec §17/§18.
- Does NOT cut a release or tag. Does NOT start implementation
  — Stage 1 begins with its own design turn.
