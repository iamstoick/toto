# Spike 2 report: drain-and-restart hot reload

- Prototype: `spikes/spike-2-hot-reload/` (isolated, throwaway).
- Base: branch `spike-1b/stack-switching`; all Spike 1 (8) and
  Spike 1b (6) tests still pass unmodified.
- Date: 2026-11-14. Toolchain: rustc 1.100.0, edition 2024.
- Runtime reused unchanged except one 3-line additive method
  (`TaskRt::remove_one_root` in Spike 1b; see reuse section).
- Gate: `./scripts/check.sh` (fmt --check, clippy `-D warnings`,
  workspace tests). Timeout tests use a SHORT 100 ms test timeout;
  the provisional 5 s `toto dev` default is never waited out.

## Falsification verdict: D7 SURVIVES as a workable mechanism

Drain-and-restart with the v2.2 lifetime rule (retire only when no
frame executes the old version AND no reachable callable can invoke
it) was demonstrated end to end with simulated versions: old frames
finish on old code while new scopes use new code; quiescence is
observed, not timed; the timeout cancels precisely the stuck scopes
at boundaries and respawns them clean; layout-typed changes fall
back to full restart; retired versions become GC-reclaimable with
exact counts. No evidence that D7 is unworkable, and no GC/scheduler
change was needed.
Recommendation: **proceed to Spike 3 / Phase 1** — no D7 revision,
no re-spike. Standing caveats: versions are simulated (no compiler),
callable reachability is tracked explicitly rather than read from
the GC, and full lifecycle proof under real codegen remains a Phase
6 requirement (stated as such in the lifetime section).

## Spike 1 test-count reconciliation (Section 2a)

Spike 1 reported 8 tests; Spike 1b reported "7 Spike 1 tests still
green". Both are correct: Spike 1 has **7 integration tests**
(`tests/spike1.rs`) **+ 1 unit test** (`src/roots.rs`), and the 1b
report counted the integration target. **No test was dropped,
renamed, or miscounted.** All 8 pass on this branch, plus 6 from
Spike 1b and 10 new here (24 total across the workspace gate).

## Pass/fail per acceptance criterion (Section 4)

1. **Versioned dispatch — PASS.**
   `body_change_drains_old_version_and_new_scopes_use_new`
   (`spikes/spike-2-hot-reload/tests/reload.rs`): after a
   body-change reload, the slow old frame's four calls dispatch v1
   (`slow used 0,2,4,6`) while the new scope dispatches v2
   (`quick used 30`); order asserted on the log, not eyeballed.
   Scope pins its version at entry (`Hctx::in_scope`).
2. **Drain quiescence — PASS.** Same test: new scope completes while
   the old frame still drains, and `is_retired(v1)` is asserted
   false mid-drain and true only after the last old frame exits.
   Quiescence is a query (`quiescent(v)` = zero frames, suspended
   tasks included since their guards live on native stacks), and
   retirement is an event in the reload log — never inferred from
   timing.
3. **Drain timeout + cancellation — PASS.**
   `drain_timeout_cancels_at_boundary_and_respawns_clean`: a
   never-draining looper is cancelled after the 100 ms test timeout
   (`Outcome::CancelledAndRespawned(1)` asserted, so the timeout
   path genuinely fired); it exits at its next scope boundary
   (`gen0 cancelled at boundary`, never mid-frame); the scope
   respawns on the new version with clean state (fresh iteration
   `gen1 used 21`, exactly one `gen1 done`, no half-updated state).
4. **Fallback restart — PASS.**
   `layout_typed_changes_restart_fresh` (StructAdd/Remove,
   ChannelType) and `global_initializer_change_restarts_with_new_
   initializers`: full restart asserted — `task_count() == 1`
   (holder only), zero heap allocations (fresh heap), new
   initializers applied (`mode == 2`), new-version behavior holds.
   `restart_for_test` tears down the scheduler and rebuilds on the
   new version; documented as the fallback.
5. **Reloadability matrix — PASS.**
   `reloadability_matrix_classification` pins all six rows
   (body→drain, signature→scope-restart, struct±/channel/global→full
   restart). Each row is exercised behaviorally:
   `signature_change_restarts_scope` (old scope cancelled before it
   could misuse the new arity; respawn serves it),
   `struct_layout_mismatch_never_reinterprets` (tag-1 data under a
   tag-2 reader returns `Err` before any field is read; log shows
   the mismatch routing and no field values),
   `layout_typed_changes_restart_fresh` + global-init test.
6. **Code-version lifetime — PASS.**
   `version_retained_while_callable_held_then_reclaimable`: with
   zero frames on v1, retirement is refused while the callable is
   held (`is_retired` false; forced GC keeps all 4 objects: both
   tokens, callable pair, id cell); the held callable still
   dispatches old behavior post-reload (`callable gave 42` twice);
   after release + quiescence, retirement succeeds and forced GC
   reclaims exactly 3, leaving 1 (the current token). Exact counts
   asserted at every step.
7. **Reload during suspension — PASS.**
   `suspended_task_resumes_on_old_version_and_drains`: task parked
   mid-scope on old code; reload; forced GC reclaims nothing and
   keeps the token (live 1); resume runs old code (`resumed 12`,
   no v2 mixing); drain completes; old version retires.
   `suspended_task_timeout_cancels_at_boundary_no_mixing`: timeout
   variant — cancelled at the boundary, respawned generation runs
   v2 only (`finite used 21`), the stuck scope never saw v2.
8. **Spike 1/1b green; gate clean — PASS.** 7 + 6 + 1 + 10 = 24
   tests pass; `cargo fmt --check` and `cargo clippy --all-targets
   -D warnings` pass via `scripts/check.sh`.
9. **This report — done.**

## Drain/quiescence mechanism as implemented

- **Version identity:** `VersionId` (u64) over hand-built function
  tables (`registry.rs`). `Hctx::in_scope` binds the current version
  at entry; `call` dispatches through the pinning scope's table.
- **Frame accounting:** scope entry roots the version's heap token
  in the task shadow and increments `frames`; exit releases both.
  Guards live on native stacks, so suspended tasks keep counting —
  quiescence (`frames == 0`) covers them with no timing involved.
- **Drain loop** (`pump_until_quiescent`): drive tasks, retire-check
  each iteration, wall-clock deadline (100 ms in tests). On
  `TimedOut`: `cancel_version` flags exactly the specs still pinning
  the old version (tracked per restartable entry), grace-pumps until
  they exit at boundaries, respawns them on the new version.
- **Fallback** (`restart_for_test`): drop the scheduler, fresh heap,
  reset globals, apply the new version's initializers, respawn the
  version-token holder. Old handles die with the old heap — no
  dangling references by construction.

## Code-version lifetime mechanism and its limits

- **Retention:** a version's heap token is rooted (a) once per
  active frame (scope entry/exit) and (b) once by the holder task
  while the version is live in the registry. Callable references are
  heap pairs `(token, func)`; the marker follows the edge, so a held
  callable pins its version exactly like any heap reference.
- **Retirement:** `retire_if_quiescent` requires zero frames AND
  zero outstanding callables, and never retires the current version.
  Retirement drops the registry entry; the holder unroots the token
  on its next sync; the next forced GC reclaims token + callables
  (asserted: 3 reclaimed, 1 live).
- **Limits (stated, not hidden):** callable reachability is tracked
  by explicit acquire/release in task code — a simulation stand-in
  for GC reachability, which the real system must read from the
  collector. Full lifecycle proof under real codegen (unloading,
  epochs) remains a Phase 6 requirement; this spike demonstrates
  the retention mechanism only. A retired version's token object is
  reclaimed by the normal collector; no code unloading exists yet
  (nothing to unload — versions are data).

## Reloadability-matrix classification design

Pure function `classify(ChangeKind) -> Disposition`, tested
directly. The change input is an explicit test-only descriptor, not
an inferred diff. Struct layouts are version-tagged pair chains
`(tag, (f1, (f2, ...)))`; the reader checks the tag before walking
any field, so old-layout data can never be read under a new layout
— mismatch routes to restart. Channel-type tags and globals live in
the registry; layout/global changes restart rather than migrate
(v1 rule: migrate-or-restart resolves to restart, no preservation).

## Measurements (rough, debug build, method noted)

- Timeout path: 100 ms drain deadline observed (~0.10 s per timeout
  test), grace drain after cancel completes in ~1 ms (cooperative
  exit at the next boundary — one scheduler round).
- Clean drain: no waiting; completes as fast as tasks yield.
- Forced-GC pauses during reload sequences: 0–1 µs on toy heaps
  (single-digit live objects); exact live/reclaimed counts asserted
  per test (e.g. 4 live/0 reclaimed with a held callable; 1 live/3
  reclaimed after release).
- No timing assertions in tests except the mechanism itself (the
  timeout must fire — asserted via `Outcome`, not durations).
- The 5 s `toto dev` default is NOT validated here; 100 ms test
  timeouts prove the mechanism, not the UX value.

## Files changed/added and reuse choice

- **Reuse (no fork):** `spike-2-hot-reload` depends by path on
  `spike-1b-stack-switching` (scheduler, `TaskRt`, channels) and
  `spike-1-gc-scheduler` (heap types). One additive change was
  needed: `TaskRt::remove_one_root` (+8 lines in Spike 1b) so scope
  exit and callable release can unroot from task code — same
  additive precedent as Spike 1b. All 14 Spike 1/1b tests pass
  unmodified. A shared extracted crate was rejected (overkill for
  throwaways); copying runtime code was rejected (divergence risk).
- **New:** `src/registry.rs` (versions, classification, lifetime
  data), `src/hctx.rs` (pinned scopes, cancellation, callables,
  layout-checked structs), `src/runtime.rs` (reload protocol,
  timeout/cancel/respawn/restart), `src/lib.rs`, `README.md`,
  `tests/reload.rs` (10 tests), `Cargo.toml`. No new external
  dependencies; `Cargo.lock` unchanged in content (no new crates).
- Root `README.md` layout notes the Spike 2 prototype.

## Deviations, assumptions, open questions

- Simulated versions (hand-built tables, explicit IDs) — the
  mechanism under test is drain/lifetime/classification, not
  compilation, watching, or unloading. No file watcher, no JIT.
- Cancellation is cooperative at scope boundaries; a task that
  never suspends cannot be cancelled on one thread (true of any
  cooperative scheme; preemption is out of scope). Channel-parked
  cancellation is unimplemented — stuck tasks in tests are
  yield-loopers; parked-recv cancellation is Phase 4 work.
- `RefCell` discipline: no registry borrow is held across a task
  suspend (one double-borrow found and fixed during development).
- Dead-task shadows keep their roots (Spike 1 simplification,
  kept); restart drops the whole heap instead.
- Tests use 100 ms timeouts and run in parallel threads (each with
  its own runtime — no shared state, deterministic outcomes).

## Provisional defaults (unchanged, awaiting ratification)

- Drain timeout 5 s: provisional Phase 4 UX value. This spike
  proves the timeout MECHANISM with 100 ms test timeouts; it does
  not validate 5 s. Saying so explicitly per the brief.
- 64 KiB stacks (measured in 1b, inconclusive for codegen),
  checked-overflow panics, typed `Result<T,E>`: unchanged, not
  exercised, not implemented, not ratified.

## Recommendation

D7 SURVIVES with the v2.2 lifetime rule as implemented. Proceed to
Spike 3 (Cranelift from toy IR) and Phase 1 (vertical slice) — no
D7 revision, no re-spike. The mechanisms to carry forward: pin
versions at scope entry, count frames including suspended tasks,
retire on frames-zero-plus-no-callables, cancel precisely at
boundaries, restart fresh on layout changes. Re-validate under real
codegen in Phase 6 (code-version lifecycle proof) before hot reload
is load-bearing.
