# Enterprise Wave 5 brief — developer tooling

Scope approved by the user 2026-11-24: all four tooling surfaces,
in order — Stage 1 linter, Stage 2 richer LSP, Stage 3 coverage,
Stage 4 doc generator. Each stage gated; no stage re-opens a
settled decision.

Status: BRIEF APPROVED, STAGE 0 PENDING (starting-state
verification waits on the post-push CI run — see Stage 0).

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
45399390212 (head 2c7a456, the current HEAD) was in progress at
brief time; Stage 0 verification completes when it concludes
green, and its outcome is recorded here before Stage 1 work is
staged. If it fails, diagnose first (same rule as Wave 4: the
diff is docs-only, so a check failure would point at
flake/infra, not at this brief).

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

## Stage 1 — linter (design recorded before implementation; no code yet)

Goal: `toto lint` over whole programs: a seeded rule set for
enterprise code standards with spanned diagnostics in stable,
documented codes, runnable in CI as a gate.

Acceptance (brief-level; stage design makes it exact): runs on
the repo's own `.gzv` corpus (stdlib + tests) with zero false
positives on clean files; every diagnostic carries a span and a
stable code; timing budget stated and met; at least one rule
with an auto-fixable shape reserved for Stage 2 code actions.
Non-goals: formatting (no `toto fmt` here), editor integration
(Stage 2 surfaces these diagnostics).

## Stage 2 — richer LSP

Goal: completion, references, rename, and code actions on the
existing stdio server, multi-document modeling, wrapping
Stage 1 diagnostics at minimum.

Acceptance: the four capabilities proven inside a real editor
session (editor + version recorded), not headless-only; the
column-encoding decision (UTF-16 vs. scalars) recorded in the
stage design and honored; broken-file behavior keeps last-good
data out (established `lsp.md` contract holds). Non-goals:
debugging protocol, untitled-buffer support unless the design
justifies it.

## Stage 3 — coverage

Goal: statement/branch coverage for `.gzv` programs with a
machine-readable report plus a gate integration.

Acceptance: self-measurement demonstrated on a corpus program;
report format fixed and golden-tested; overhead budget stated
and met on the bench corpus; the gate can fail a run below a
configured threshold. Non-goals: branch-level precision beyond
what the interpreter/JIT can attribute honestly (limits
disclosed, not silently smoothed).

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
