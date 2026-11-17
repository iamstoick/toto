# Phase 6 — State-preserving hot reload (GATED; Stage 1 evaluation)

Branch `phase-6/state-preserving-reload` off `main` (post-merge
`79797f9`). Gate green throughout (`scripts/check.sh` exit 0).

> **Stage 1 verdict: GO — with the design below. Stages 2–4 NOT
> started; implementation awaits explicit user authorization.**
> A NO-GO would have ended the phase here; the evidence
> justifies proceeding. Details in §Stage 1 evaluation.

Note on sources: no PDF accompanied the kickoff brief, so the
v2.4/37-page verification could not be re-performed this turn. The
title page read v2.4, 37 pages at the Phase 4 kickoff and every
phase since restated it; all D1–D8 work below proceeds on that
basis. If the attached PDF differs, STOP and say so.

## Stage 0 — repo hygiene + Phase 5 flag close-outs

- `phase-5/tooling-stdlib-ffi` (`127dfbb`) merged to `main` as
  `e2380a9` (merge commit, history preserved), hygiene fix
  `79797f9` on top. Never force-pushed; never rewrote history.
  Pushed; CI green on `main` for both commits (runs `43991148168`
  merge, `43991227918` hygiene — both `success`).
- README: status line to Phase 5 complete / v1 core complete;
  roadmap link kept at v2.4. `docs/stdlib.md`: "Eight modules"
  → nine.
- Flags (each recorded; gate green, all suites pass):
  - (a) `fmt` pub-drop: explained in `docs/fmt.md` (parser
    discards top-level `pub` before the AST exists —
    `parse_item`: "no effect (flat namespace)" — so no
    downstream pass can observe it) and pinned by
    `fmt_drops_pub_without_changing_meaning` (formats every
    file of pub-using `proj02_transitive`, asserts `pub` gone
    everywhere, formatted tree still loads and checks).
  - (b) `fmt` counts: threshold asserts replaced with exact
    pins — 113 formatted / 15 refused / 40 executed — plus the
    exact refused set asserted by name. The 15 are all
    lex/parse-level failures: E0001 ×4 (`diag01`, `diag02`,
    `diag03`, `lex05`), E0101, E0102 ×2 (`diag04`,
    `parse02`), E0103, E0104, E0106 ×2 (`diag12`, `parse09`),
    E0107, E0154, E0156, E0158. Check-time (type) errors all
    parse and therefore format. Any corpus addition changes
    these pins loudly.
  - (c) Build equivalence: stated sample of 7 corpus programs
    built with `toto build` and compared against interpreter
    goldens — `arith`, `closure` (higher-order, ex-differential
    case), `structs`, `loops`, `divzero` (status-1: same
    `panic[E0202]`, exit 1), `pingpong` (concurrent under AOT),
    `stdlibproj` (multi-file + stdlib imports). 7/7 pass,
    including the AOT panic path. (`apps/wordfreq` equivalence
    was already gated in `check.sh`.)
  - (d) 100-edit harness: committed as
    `scripts/devloop_soak.sh` (81 generations, 20 error
    notices, exact output sequence; preserves `dev.log` on
    failure, `PASS`-gated cleanup). NOT in `check.sh`:
    ~3-minute wall time with poll-based waits; run manually
    per release. Rationale recorded here, not silent.
  - (e) 154→155 reconciliation: full `cargo test --workspace`
    green, **58 result lines / 155 passed / 0 failed** (the +1
    is the new fmt pub pin). Test-by-test: spike-1 8 (lib 1 +
    `spike1` 7), spike-1b 6, spike-2 10, spike-3 8, ffi-smoke 7
    (= 39 spike tests, unchanged); toto-cli 22 (bin 3 + cli
    15 + corpus 1 + lsp 3); toto-compiler 35 (differential 1 +
    fmt 2 + interp 8 + lexer 7 + mir 1 + mir_conform 1 +
    parser 5 + project 10); toto-jit 38 (dev 11 +
    differential 1 + diff-concurrent 1 + diff-task 1 + gate 8
    + mn_jit 6 + opt 3 + panics 1 + race 2 + stack_jit 2 +
    timings 1 + yield_jit 1); toto-runtime 21 (blocking 4 +
    concurrent 3 + fmt_equiv 1 + gc 6 + mn 5 + sequential 1 +
    stack 1); doc-tests 8 × 0; remaining bin/lib unit
    suites 0. Plus `toto test` projects: stdlib 8/8, wordfreq
    1/1 (both in `check.sh`).

## Stage 1 — GO/NO-GO evaluation

### Evidence review (tree, not memory)

**Spike 2** (`docs/spikes/spike-2-hot-reload.md`, tests
`spikes/spike-2-hot-reload/tests/reload.rs`, 10 pass): proved
the retention MECHANICS — quiescence query (`frames == 0`),
retire-if-quiescent, drain loop with timeout, cancel-at-boundary,
full-restart fallback, version-tagged layouts with
tag-before-walk, five-row classification as a pure function. Did
NOT prove: real codegen (versions were data; "no code unloading
exists yet"), or real GC reachability ("callable reachability is
tracked by explicit acquire/release in task code — a simulation
stand-in for GC reachability, which the real system must read
from the collector"; "Full lifecycle proof under real codegen
remains a Phase 6 requirement"). Verdict on Spike 2: motivates
the shape, justifies nothing by itself.

**Phase 4** (carries over unchanged): module-per-generation
retirement (`Generation` owns its `Jit` + `JitSession` — "owns
the generation's JIT module so the compiled code provably
outlives the run", `dev.rs:247-265`); strictly sequential
generations (`next_gen`, "mixed-level generations would break",
`dev.rs:403-419`); drain/cancel/settle with 1 ms ticks and
cancel grace (`dev.rs`, `SettleOutcome`); parked-recv
cancellation (proof in `mn_jit` + `dev` tests); reload during
suspension (drain path); structural MIR classifier with the
fail-safe property ("any misclassification stays correct —
both paths isolate generations fully", `dev.rs:46-90`).
What MUST change for preservation: classifier granularity
(3 outcomes → per-row dispositions), generation coupling (state
carried across the retire/spawn boundary), and value identity
for anything version-linked (below).

**Discovery this turn (load-bearing for Stage 3): output
streams on retire.** `toto dev` buffers a generation's stdout
and emits it when the NEXT rebuild retires that generation
(`ingest` flushes `ended.stdout` on `Rebuilt`; verified live:
second save flushed `42` + `generation 1 exited (0)`).
Consequences: (1) the last generation's output is
unobservable without a further edit — the soak harness
asserts accordingly (gens 1–80, documented, not a skip);
(2) soak assertions must key on retire markers, never on
spawning output; (3) no product change — atomic
per-generation output is the intended design.

### Multi-file recommendation

**Recommend: scope Phase 6 proofs to single-file programs;
multi-file `dev` as a follow-up, explicitly not a
prerequisite.** Rationale: (1) all five matrix rows
(body/signature/struct/channel/global) are expressible in one
file, and every novel mechanism (retention, relink, migration,
matrix enforcement, stale detection) is version-scoped, not
file-scoped; (2) the flat namespace (`parse_item`: imports
merge, duplicates `E0311`) means file boundaries carry almost
no semantics — a project reloads atomically through the proven
Phase 5 loader (`project::load_and_check`), so single-file
proofs transfer directly; (3) multi-file `dev` is real work
(watch N files, project load per change) but separable, with
its own risks, and was already a recorded carry-forward. The
follow-up is named, not dropped.

### Architecture answers

**Q1 — GC-reachability-based retention: YES, mechanism
exists.** Retention set = code modules (whole `Generation`s)
with ≥1 live referencing closure, plus any module with
executing frames (existing quiescence). Liveness source: heap
mark bits (`Header.marked`, `heap.rs`) read after a forced
collection, plus an allocation watermark for objects born
since the last GC (new tiny `Heap::is_live` query — additive,
no design change). Registration: `alloc_closure` knows its
creating session/module, so the runtime side table
(`GcRef → module`) is filled at creation, pruned lazily on
retire checks via `is_live`. No acquire/release stand-ins;
no collector redesign. (Stage 2a: prove this slice first —
it is the riskiest.)

**Q2 — Epoch tags without ADR-008 change: NOT REQUIRED — and
that is the answer.** Stale vectors, each covered without
touching value traffic (`(tag, word)` pairs, entry points,
root publication, and the `2^32` discrimination rule all
stay):
- Heap closures: retained modules stay callable, so no live
  holder ever names a dead module; retention (Q1) makes
  retirement-while-referenced impossible. Genuine bugs still
  fail loudly through the existing generational `GcRef`s
  (wrong generation → `None` → `StructMiss::Stale`-style
  misses, as `task_closure_body`/`task_struct_init` already
  report).
- Bare `fn` values (`Func(usize)`, no version bits):
  NAME-RELINK at reload — the old `Generation` retains its
  `Program` (fn table with names+signatures); each preserved
  `Func(i)` remaps by (name, signature) into the new table,
  else the whole preservation commits atomically to
  restart-without-preservation (never partial, never silent).
- Struct layouts: values are self-describing
  (`Payload::Struct { name, fields }`, read through
  name-based snapshots — `struct_snapshot`,
  `task_struct_init` match fields BY NAME with stale
  detection). Layout check = name + field-set compare before
  any field is touched: the Spike 2 tag-mismatch discipline
  falls out of the existing representation.
- Payload internals (e.g. `Closure.body: u32`) sit behind
  the `toto_*` helper boundary (51 import sites in
  `compile.rs`; JIT reads payloads only via
  `task_closure_body`/`cap`, `toto_struct_get/set`,
  allocators) — internal layout evolution is not an ABI
  change. No frozen rule is touched by any of the above.
  Epoch tags as a separate mechanism are NOT proposed:
  retention + relink + generational refs cover every stale
  vector with fewer moving parts.

**Q3 — Migration machinery.** Declared state is identified by
the new `state {}` block (proposal below; checker-known).
At reload, per preserved value: (1) type-check against the
new version's declaration (whitelist + exact type match —
mismatch → atomic restart-without-preservation); (2) struct
values whose layout changed route to the user `migrate`
function for that struct (runs in the NEW version on the old
VALUE — name+fields snapshot in, new value out); (3) migrate
panic/failure → atomic restart-without-preservation (runs
BEFORE commit; no partial state ever installs). No blanket
preservation of stacks or pools, ever.

**Q4 — Whitelist: channels + plain data. Nothing else.**
No timer API exists in v1 (no sleep/timeout/clock builtins;
`time.gzv` is pure durations) — per the brief, none is
invented. Plain data = int/float/bool/string/structs/arrays/
Range/Result/Option/unit thereof, recursively. EXCLUDED,
rejected LOUDLY at check time (new static error): `fn` types,
closures (inferred), `*T` foreign pointers. Queued values
inside a preserved channel are re-validated at reload — a
queued callable (or any non-whitelisted value) fails the
whole preservation atomically to restart-without-preservation
(fail-safe direction preserved from the old classifier).
Channel rebinding checks the element type matches, else the
same atomic fallback.

**Q5 — Fallback integrity.** Drain-and-restart stays the
fallback for every incompatible change AND every preservation
failure, via the existing settle/cancel path. Loudness is
structural: unknown classification → restart WITHOUT
preservation (extends the old "both paths isolate fully"
property into "all unknown paths preserve nothing");
static violations (non-whitelisted declared types, missing
migrate fn, relink failure) are check-time errors in the NEW
version before anything installs. Production builds never
hot-reload (unchanged).

### Design proposal (for user approval — not unilateral syntax)

```gzv
// Top level, at most one per program (second block: new error).
state {
    counter: int = 0;      // plain data: preserved (+ migrate if layout changes)
    log: chan<string>;     // channel: rebound if element type matches, else atomic restart
    // handler: fn(int) -> int;   // REJECTED: callables never preserved (new E-code)
}
```
- First run: initializers run (`= v` required for data;
  bare `name: chan<T>;` creates the channel). Reload: values
  carried subject to Q3/Q4; initializers do NOT re-run for
  carried values.
- `migrate` functions (proposal): `migrate fn(old: S) -> S`
  per struct with a layout change; runs in the new version;
  must be total over the old field set (missing-field access
  is a check-time error in the migration fn itself).
- `state` in v1-old programs: absent → nothing preserved →
  exactly today's drain-and-restart. Bootstrap is clean by
  construction.
- Matrix enforcement (extends `classify_change`; all five
  rows tested): fn-body → preserve+drain; fn-signature →
  code restarts, state preserved iff state types re-validate;
  struct add/remove → migrate-or-atomic-restart (never
  reinterpret); channel-type → rebind-or-atomic-restart;
  global-initializer → globals always fresh; state referencing
  a changed global re-validates.
- "Declared" = named in `state{}` and whitelist-valid.
  "Undeclared" = everything else, including ALL callables:
  dropped with the old generation, never dispatched
  post-reload. The D7 held-callable case is served by
  retention (old module + old frames keep dispatching during
  drain, reclaimed after — the Spike 2 lifetime rule in real
  codegen) and by relink/retention for preserved holders —
  never by silent reinterpretation.

### Risk register (explicit, not hidden)

1. Cross-generation calls (new task invoking a retained old
   closure): contexts, stacks, and root publication are
   version-agnostic (frozen protocol, unchanged); struct
   access is name-based (sound across layouts). Residual risk:
   old code's ASSUMPTIONS about globals (always fresh per
   reload — old closures capturing globals capture VALUES
   by value at creation, spec §5.7, so no staleness).
2. Memory: one retained module per reload while holders live;
   reclaimed on release. Soak asserts heap accounting
   (no-growth across N reloads with released holders).
3. `Heap::is_live` watermark semantics: Stage 2a proves the
   query exact (forced-GC + allocation-interleaving tests)
   before anything depends on it.

### Verdict: GO (with the design above)

No frozen decision changes; no ADR-008 change (proven
per-layer above); never-silent by construction (atomic
commit, whitelist rejection, fail-safe classification);
Spike 2's deferred real-codegen proof is exactly what Stages
2–4 deliver, now with identified mechanisms instead of
stand-ins. Recommended Stage 2 order: (2a) retention/relink
proof first (riskiest slice), then syntax/checker, then
migration, then matrix — each gated on the previous.
**STOP: Stages 2–4 not started. Awaiting explicit user
authorization to implement.**

## Exact counts (Stage 0–1)

- Corpus: unchanged this phase (no program added, removed, or
  re-goldened). 139 entry programs (55/12/72), 148 `.gzv`
  files, pins 113/15/40 hold with the exact refused set.
- Cargo: 58 result lines / 155 passed / 0 failed (154 +
  the fmt pub pin). 39 spike tests unchanged.
- `toto test`: stdlib 8/8, wordfreq 1/1. Build sample (flag
  c): 7/7. Soak harness: 2 consecutive PASS runs post-fix
  (plus the pre-fix flake + stream-on-retire investigation,
  both recorded above — the harness is stronger for them).

## Deviations, assumptions, open questions

- `cargo fmt` does not cover `scripts/devloop_soak.sh`
  (shell); style kept Bourne-clean by hand.
- The soak's settle delay (0.5 s > 100 ms poll) trades a
  little wall time for determinism; rapid-overwrite
  preemption stays covered by Phase 4 drain tests.
- Assumption re-verified live: `toto dev` generation output
  streams on retire (atomic per-generation output) — now
  documented here because Stage 3 soak assertions depend on
  it.
- Open (for Stage 2): exact `Heap::is_live` watermark
  semantics; `state{}` block placement rules (one per
  program proposed); new static error codes for whitelist
  violations (fresh range, recorded in spec).

## Carry-forwards (unchanged ownership)

Multi-file `dev` (follow-up, not prerequisite); span-level
race attribution (Enterprise); LLVM AOT / production AOT
(Phase 7); package manager + registry (Phase 7); richer LSP,
linter, doc generator, coverage (Enterprise Wave 5); user
generics/traits; general optimization; benchmarks; anything
from the Part 3 Enterprise Track.
