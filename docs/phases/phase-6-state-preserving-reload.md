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

## Stage 2 — single-generation declared state (slices 2a–2b)

Reload-time enforcement (migration invocation, matrix, queued-value
validation) is Stage 3. Stage 2 lands execution + checking of
declared state within one generation through every executor,
with retention proven by real GC reachability. Each slice gated
on the previous; gate green throughout.

### Slice 2a — retention/relink proof (exit criteria i–iv, §2.12)

- `Heap::is_live` + `closure_refs` (`crates/toto-runtime/src/heap.rs`,
  `sched.rs`): no under-retention proven by
  `gc.rs::is_live_never_under_retains`; over-retention bounded to
  one collection by `is_live_over_retains_at_most_one_collection`;
  watermark semantics pinned by
  `is_live_rejects_stale_generations_on_reuse` and
  `is_live_covers_post_collection_allocations`. gc suite 11/11.
- Retention with REAL GC reachability in generated code
  (`crates/toto-jit/src/dev.rs`, `retain_proofs`): a carried
  closure keeps its module callable across a reload
  (`closure_retained_callable_then_reclaimed`), likewise a carried
  function (`func_retained_callable_then_reclaimed`); removal routes
  to atomic restart; a stale carry fails atomic (4 tests green).
  No acquire/release stand-ins: the collect roots ARE the carried
  values. Ownership Arc-only (no shared-state `Rc`); closed worlds
  (new→old calls only, through retained tables); JIT reads payloads
  only via the `toto_*` helper boundary. Verdict: 2a PASS — §2.12
  (i)–(iv) all evidenced; proceeding to syntax/checker.

### Slice 2b — syntax/checker + corpus + pins + conformance

- Syntax/checker: `state {}` + `migrate` items (contextual keywords —
  corpus check 2026-11-17: zero existing programs use either as an
  identifier in item position), one-block + placement rules, binding
  rules (mutable, declaration-order init, `E0319` forward/self,
  bare-channel `E0318`), migrate rules (one param, struct in = struct
  out, one per struct, `E0325` determinism incl. deferred user calls
  as user calls, `E0326` totality), `E0311` flat-namespace collisions,
  `E0316`/`E0317`, `E0321`–`E0324`. `?` unbanned (`E0308` covers —
  verified live: `?` in init fails as `E0308`, never reaches `E0320`).
- Executors: MIR `StateGet`/`StateSet` (+ `ChanNew` in init, entry
  block opens with `safepoint entry`, init allocs carry per-op guards
  — Tier-1 fix, see below), tree-walker ordered init, `mir_interp`
  state vec, runtime `state_vars` + root-set extension, JIT
  `toto_state_get/set` + task variants. Slots = declaration order on
  all five paths.
- Corpus: 29 new programs, all goldens generated from actual output
  and reviewed before blessing (never blind): 7 positives
  (`interp/state01-05`, `concurrent/state06-07`), 20 typing negatives
  (`type_state_*`, `type_migrate_*`), 2 parsing placement negatives
  (`parse10-11`, multi-error recovery preserved). 4 cascade cleanups
  re-verified single-error (init-spawn `go` shape, missing-init,
  second-block, migrate-call-direct).
- Pins moved loudly, all zero-divergence: fmt 113/15/40 → 140/17/45;
  gate differential ran 36→41; task 40→45; concurrent 19→21; timings
  41/60 → 46/67; opt 36/40 → 41/45; runtime sequential 40→45 +
  `state06-07` join the hardcoded concurrent list (both deterministic
  rendezvous chains). The 5 sequential state positives match
  tree-walker ≡ mir-interp ≡ runtime-executor ≡ gate JIT ≡ task JIT
  (stdout + exit exact; the concurrent two match runtime-executor ≡
  task JIT ≡ opt-JIT against goldens) — a genuine cross-executor
  proof of the slot contract and first-load init order.
- Real conformance gap found + fixed (not a pin): state programs
  failed Tier-1 (`main` entry not first after init prepend; init
  `ChanNew` lacked the alloc guard). Fixed in `mir.rs` (entry
  safepoint first, guard on init `ChanNew`) — same convention as
  ordinary code, no frozen-rule change. MIR dump case
  `dump_state` added + hand-verified before blessing (entry-first,
  slot order, guards).
- Spec corrections (spec-text only; implementation + goldens were
  already right): §14.1 chan-init code `E0322` → `E0318` (matched
  §14.4, the `E0318` emission, and the golden); §14.4 `E0320` row
  drops `` `?` `` (verified `E0308`). E0314 state-send verified live
  (arrays E0314, deeply-immutable ints send normally); nested channel
  construction in a data init accepted (routes to runtime executor).
- Gate: `./scripts/check.sh` EXIT=0 on two consecutive runs (58 ok
  lines, 0 failures).

### Exact counts (after slice 2b)

- Corpus: 157 entry cases in the differential census (41 status-0 +
  4 status-1 runnable, 21 concurrent, 90 static, diag10 excluded);
  `.gzv` files 148 + 29 = 177 (9 support unchanged).
- Cargo: full gate green ×2 (fmt, `clippy -D warnings`, all suites
  incl. 39 spike + 11 gc + 4 retain_proofs); `toto test` stdlib +
  wordfreq unchanged (not re-run this slice — no stdlib surface
  touched).

## Stage 3 — declared state + migration (carry planner, evaluator, driver, e2e matrix)

Reload-time enforcement is this stage: migration invocation,
reload-time totality, the matrix, and queued-value validation.
First-load checking was slice 2b; nothing there changed.

- Carry planner (`crates/toto-jit/src/preserve.rs`, `plan_carry`):
  same names + same types with no layout change carry verbatim;
  added/removed bindings abort loudly (fresh init on the atomic
  path, initializers never rerun); struct layout change runs the new
  program's `migrate` for that struct (missing migrate, failed
  reload-time totality against the OLD field set, or a migrate panic
  all abort loudly — no implicit defaults, §2.2); channel type
  change carries on an empty queue, aborts on a non-empty one;
  `Func` stability is by MIR table order/signature match (a shifted
  index would silently call the wrong function, so any mismatch
  aborts); fn-signature changes play no role for `migrate`
  (per-struct only, §2.1). Migration evaluates on a snapshot: the
  old heap is never mutated, channels/closures/functions pass
  through by handle.
- Migrate evaluator: the pure subset over the old value (no user
  calls, no IO builtins, no foreign calls, no channels/tasks/state,
  `?` already impossible via `E0308`); output validated field by
  field against the NEW types before anything is touched
  (a mistyped field aborts instead of reinterpreting).
- Unit matrix (`preserve_tests`, 14 tests green): verbatim carry,
  init-change carry, added/removed aborts, layout-with-migrate
  (incl. mistyped-field validation abort), identity-removal carry,
  no-migrate abort, migrate-panic abort, reload-time old-field
  totality, closure-in-migrate abort, channel type change with empty
  queue carries, with queued values aborts, func-table reorder with
  func state aborts, pure-subset exercise.
- Driver wiring (`crates/toto-jit/src/dev.rs`, `rebuild_with`):
  init-strip (the preserving compile drops state initializers —
  they run once, on fresh load), `PreservingSeed` (carried slots in
  declaration order + the moved channel table; the old heap moves
  whole so handles stay valid), loud atomic abort on ANY planner or
  recompile failure (fresh heap, initializers run, nothing carried).
  `prune_retired` collects the moved heap with exactly the carried
  roots on every preserving rebuild — retention stays GC-reachable
  end to end, never an acquire/release stand-in.
- E2e matrix (`preserve_proofs`, 10 tests green): body change
  preserves (row 1); init change carries old value (row 5);
  layout change with migrate end to end (row 3, port 21 carried
  then tls derived); layout change without migrate aborts to fresh
  init with the reason recorded (row 3 negative); signature change
  preserves state while code restarts (row 2, §2.1 correction);
  declared channel moves whole with counter proof (row 4);
  drained/cancelled share the carry arm; timeout abandonment
  carries nothing (fallback integrity); stateless body change
  drains exactly like Phase 4 (no carry, no abort — the fallback
  pin); 10-reload soak with heap accounting (Stage 4).
- Q4 classification (§2.15), written down: REACHABLE — empty-queue
  channel type change (e2e `channel_state_moves_whole_e2e`),
  verbatim/validated value carry (every e2e), fn/closure values in
  carried state under a stable table (unit `func_table_...`, e2e
  retain proofs). DEFENSE-ONLY (unreachable by construction, still
  asserted) — non-empty queue at settle (state channels are always
  unbuffered with no waiters post-settle; unit
  `chan_type_change_queued_values_abort` pins the abort), fn value
  under a reordered table (unit `func_table_reorder_...`). The
  atomic commit holds either way.
- Fixture fix (this stage): `migrate_end_to_end`'s gen2/gen3
  migrate bodies added `bool` to `int` (`old.port + probe`).
  Cascade noted honestly: after the `E0303`, best-effort HIR drops
  the struct-literal reads, so the walker also reported `E0326`
  (first unread field). Multi-error recovery reporting both is
  accepted behavior — the fixture was wrong, not the checker.
  Fixed as `old.port + (if probe { 1 } else { 0 })` (probe stays a
  syntactic `old.tls` read inside a dead branch, never evaluated at
  reload); asserted output unchanged (`21\n1\n`).
- Harness fix (this stage, load-bearing): `timeout_abandonment_...`
  used the production `ReexecRestart`, which re-spawns the test
  binary — under `cargo test` that re-runs the suite into an
  unbounded re-exec chain holding the harness pipe open (the
  earlier 30-minute hang: PIDs churning, `tail` never seeing EOF).
  Fix: `RecordRestart` test stub (records, never exits) + the
  shared helpers generalized over `R: RestartPolicy`; the timeout
  test now asserts exactly one recorded restart and completes
  in-process. Precedent: the integration suite already uses its own
  `MockRestart` (`crates/toto-jit/tests/dev.rs`). No test may use
  `ReexecRestart` on a path that can abandon a run (recorded on the
  stub). Known leak: the abandoned infinite-loop thread keeps
  spinning in-process after the test (its worker is dropped with
  the run); production is safe via re-exec. The other nine tests
  keep `ReexecRestart` — none can abandon, so no chain is possible.

## Stage 4 — scenario matrix, soak, fallback, gates

- Scenario matrix: every matrix row has the correct outcome
  asserted at two levels — unit (`preserve_tests`: carry, abort,
  migrate-run, validation, queued-value, func-stability) and
  observed-output e2e (`preserve_proofs`: rows 1, 2, 3±, 4, 5,
  shared arm, abandonment, stateless fallback). Incompatible
  changes fail loudly with the reason recorded
  (`carry_abort` contains "no migrate" in the e2e) and fall back to
  drain-and-restart safely; never silent reinterpretation, never a
  silent skip or silent drop (removal with a non-identity migrate
  cannot name the dropped field — `E0310` — and restarts loudly).
- Soak (`soak_many_preserving_reloads_bounded_heap`): ten
  consecutive preserving reloads over string state plus a flush
  generation — twelve generations, every streamed output asserted
  exact (`a` … `a`×11), `carry_abort` none throughout. Heap
  accounting per generation: `objects_live` asserted bounded
  (≤ 4, every generation) while `allocated_total` grows (churn
  happened); the final carried tag verified intact by heap read.
  No leaks (live flat over 12 reloads), no corruption (exact bytes
  + intact tag). Broken-edit survival is covered by the 100-edit
  `scripts/devloop_soak.sh` harness (80 good + 20 broken, manual
  per release) and the missing-migrate e2e.
- Drain-and-restart fallback intact: `stateless_body_change_...`
  proves a program with NO declared state behaves exactly as in
  Phase 4 (new output, nothing carried, no abort); the Phase 4
  `dev` integration tests (classifier, settle, still-broken,
  no-change, ordering, parked-cancel) stay green in the full gate;
  a program with no declared state never enters the carry arm
  (`plan_carry` is skipped when both sides are stateless).
- Gates: `./scripts/check.sh` EXIT=0 on two consecutive full runs
  (fmt `--check`, `clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, stdlib 8/8, wordfreq project tests +
  interpreter-vs-built byte equivalence); `cargo fmt --all` applied
  to touched files (no other files changed by fmt).

## Corrections (authorization §2.13–§2.15, dated 2026-11-17)

- §2.13 reconciliation, re-derived from a fresh `cargo test
  --workspace` (58 `test result` lines, every line accounted for
  exactly once, all `ok`, 0 failed). The old 154/155/166 figures
  are superseded — do not copy them. Non-zero suites:
  - Spikes 39: ffi int 7 + spike-1 lib 1 + spike-1 int 7 +
    stack-1b int 6 + reload int 10 + jit-3 int 8.
    (Zero-count spike bins — demo/overflow/trapprobe/jit_probe/
    panic_probe/spike1_demo — are build-only harnesses, plus 5
    zero-count spike lib targets; the 8 `Doc-tests` lines are 0.)
  - Compiler 57: cli lib 3 + cli int 15 + corpus runner 1 + lsp 3 +
    differential 1 + fmt 2 + interp 8 + lexer 7 + mir 1 +
    mir_conform 1 + parser 5 + project 10.
  - Runtime 26: blocking 4 + concurrent 3 + fmt_equiv 1 + gc 11
    (incl. the 5 `is_live` proofs) + mn 5 + sequential 1 +
    stack 1 (lib target 0).
  - JIT 66: lib 28 (4 retain_proofs + 10 preserve_proofs + 14
    preserve_tests) + dev int 11 + differential 1 + concurrent 1 +
    task 1 + gate 8 + mn_jit 6 + opt 3 + panics 1 + race 2 +
    stack_jit 2 + timings 1 + yield_jit 1 (probe bins 0).
  - Total: 39 + 57 + 26 + 66 = **188 passed, 0 failed**.
    Gate extras outside `cargo test`: stdlib 8/8 via `toto test`,
    wordfreq project tests + interpreter-vs-built `cmp` clean.
  - The old breakdown's errors (subtotals summing to 166; toto-jit
    components to 39 not 53; dev tests missing from the toto-jit
    list) came from mixing per-file counts with per-suite lines
    and omitting the integration suites. The table above counts
    result lines only — one line, one suite, no overlap by
    construction (the dev-11 ARE the toto-jit integration tests;
    the spike-bins 0-lines are separate build-only targets).
- §2.14 build-sample reproducibility: `scripts/buildsample.sh`
  (committed this stage; was `/tmp` scratch) re-runs the 7-program
  sample with repo-relative paths:
  `tests/corpus/interp/int01_arith.gzv` (arith),
  `interp/clo04_higher_order.gzv` (closure),
  `interp/struct01_basic.gzv` (structs),
  `interp/loop02_for_range.gzv` (loops),
  `interp/int03_divzero.gzv` (divzero),
  `tests/corpus/concurrent/conc01_pingpong.gzv` (pingpong),
  `tests/corpus/project/proj09_stdlib` (stdlibproj).
  Command: `./scripts/buildsample.sh` (optional toto-binary arg;
  ~10 min cold — one full AOT build per program — so manual per
  release, like `devloop_soak.sh`, not in `check.sh`). Observed
  2026-11-17: 7/7 — arith/closure/structs/loops exit 0 with golden
  stdout; divzero exit 1 with `panic[E0202]: division by zero`;
  pingpong exit 0 `42`; stdlibproj exit 0
  `Ok(00720|1d 2h|9|6)`. All match the interpreter goldens.
- §2.1–§2.7/2.15 spec pins: verified present in `docs/spec.md`
  §14–§14.4 + the 2026-11-17 amendment record (one `state {}`
  block, `E0311` collisions, contextual keywords with the 2026-11-17
  corpus check, mutable declaration-order bindings with `E0318`/
  `E0319`, one-parameter struct-in=struct-out migrate with `E0325`
  determinism + `E0326` totality, channel revalidation incl. the
  undeclared-only drop rule, no-implicit-defaults, signature-row
  correction, whitelist channels+plain-data, Q4
  reachable/defense-only split, call directionality, helper
  boundary). No new builtins (27 frozen); production builds do not
  hot-reload.

## Exact counts (end of Phase 6)

- Corpus: unchanged since slice 2b — 157 differential-census entry
  cases; 177 `.gzv` files (9 support). No program added, removed,
  or re-goldened in Stages 3–4 (the e2e matrix is Rust tests, and
  the migrate-fixture fix touched no corpus file).
- Cargo: 188 passed / 0 failed over 58 result lines (table above);
  fmt pins 140/17/45 hold (no formatter-relevant corpus change);
  stdlib 8/8 + wordfreq green via `check.sh`.
- New tests this phase: 14 `preserve_tests` + 10 `preserve_proofs`
  (7 matrix + abandonment + stateless fallback + soak) +
  `RecordRestart` stub; 4 `retain_proofs` unchanged and green.
