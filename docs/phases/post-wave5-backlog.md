# Post-Wave-5 backlog disposition (2026-11-26; architect-owned)

Wave 5 (developer tooling) and the hover-docs follow-up are
closed. This note disposes the remaining backlog: what was
done, what was spiked, what is deferred and why. No code here
beyond a refusal-message pointer; no wave reopened.

## 1. Local toolchain — DONE

`rustup component add rustfmt clippy` succeeded (the network
route is open after all): rustfmt 1.11.0-stable + clippy
0.1.100, matching rustc 1.100.0. The CI-oracle tax is dead —
future waves verify fmt + clippy locally before pushing.
Caveat recorded here: CI floats `@stable` and may run newer
versions, so local is a strong approximate oracle and CI
stays final. This supersedes the no-oracle advisory written
into `docs/phases/lsp-hover-docs.md` (true when written;
kept as history).

## 2. Concurrent coverage — SPIKED, verdict: wave-scale, NOT started

Why `toto coverage` refuses (recap, verified in tree): the
tree-walking `Interp` explicitly rejects `go`/`select`
(`interp.rs:849`) and send/recv/channel construction
(`interp.rs:922-923`); concurrent programs execute through
`toto_runtime` (M:N work-stealing scheduler + MIR executor),
which has no coverage hooks. The refusal ("refusing instead
of reporting smoothed numbers") stands and now points here.

What the spike found FOR it:

- Deterministic mode exists: `RuntimeConfig.workers = 1`
  (`crates/toto-runtime/src/lib.rs`) is documented as
  deterministic single-threaded mode — one FIFO queue,
  fixed-seed `rng_seed` for select tie-breaks (Stage 1
  scheduler, `sched.rs:5`).
- Spans flow through lowering: MIR nodes carry `Span`
  (`mir.rs`), so attribution is representable.
- `CovHits` is span-keyed (`(start, end)` + arm) and
  executor-agnostic: the inventory, LCOV render, and gates
  are reusable as-is by any executor that calls
  `hit_stmt`/`hit_branch`/`hit_fn`.

What blocks it (verdict, recorded here — do not start this
as a follow-up):

- Instrumentation surface: `exec.rs` (3,808 lines) executes
  MIR (`MInstr` via `exec_instr`), not AST; every statement
  and branch point needs span-accurate hooks plus scheduler
  task accounting.
- Semantic fork risk: multiple MIR instructions back one
  AST statement, so MIR-mapped hits inflate `DA` execution
  counts — a second dialect of "lines" beside the
  tree-walker's exact one. Resolving that (e.g., lowering
  marks statement boundaries) is a design decision, not a
  detail.
- End-to-end determinism of workers=1 remains to be proven
  (timers, OS interactions), not assumed.
- Size: days to a week with open semantic risk. This is a
  Wave 6 candidate with its own design turn, not a
  follow-up. Narrowed alternative considered and REJECTED:
  fn-level-only degraded mode (two report shapes — no
  degraded modes ship).

## 3. Registry / versioned publishing — DEFERRED, needs product decisions

Phase 7 headline; not started — building it now would be
inventing product. Deferred pending user-owned decisions:

- Hosting model: central service vs local/git-based
  (cross-system architecture).
- Versioning + resolution semantics against `toto.lock`
  (data contract).
- Auth and identity for publishing; namespace ownership.
- Relationship to what exists: `toto publish` (local-only
  pack) and `toto add --git/--path` already work.

Architect observation recorded here: git/path deps plus the
lockfile may dissolve the registry question into "versioned
git deps" (much smaller than a registry). What is missing is
a demand signal — who needs to share packages? Name the
demand and the five decisions above get asked in one round.

## 4. Deliberately not pursued

Completion-item doc prose (marginal polish on a complete
feature); trailing-note attachment (by design, see Wave 5
brief); anything that reopens a closed wave.

## Validation (CI green 2026-11-26, run 45804775707)

Run 45804775707 on the backlog head (06dce97) concluded
SUCCESS: check + release-gates + bench + macOS/Linux-ARM
probes green (Windows probe the known informational
failure). This green covers the refusal-message pointer and
confirms the tree state the spike was verified against. No
open items remain on this note.
