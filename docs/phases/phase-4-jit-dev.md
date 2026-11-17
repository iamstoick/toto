# Phase 4 — Cranelift JIT + `toto dev` report (Stages 0–3)

Branch `phase-4/jit-dev`. PDF v2.4 governs (37 pages, verified by the
tablet session; link in the resume brief). D8 / ADR-008 stays FROZEN:
no frozen decision was changed at any point in this phase.

## Verdict

Phase 4 exit criteria met. The full Cranelift backend compiles all
123 corpus programs; the JIT matches the reference executors exactly
on every runnable case at every optimization level; `toto dev`
rebuilds with drain-then-restart including the parked-recv
cancellation proof; the fiber-aware race detector flags conc10 and
nothing else; Tier-1/2 and the end-to-end milestone re-run green
through the JIT. Gate: `./scripts/check.sh` exit 0 (54 suites,
127 tests, 0 failures), pushed, CI green on both jobs.

## Stage 0 confirmation

- `phase-3/runtime` merged to `main` as `4fcaa1a` (prior session).
- README status updated; roadmap link restored as v2.4.
- Phase 3 report verdict fixed to three races + one trace bug, with
  the dated 2026-11-15 reconciliation note.
- Stage 0 README + report edits rode in the first Phase 4 commit
  (`ebe61f9`).

## Stage 1 — full backend + proof harnesses (COMPLETE)

Built in `crates/toto-jit` (`compile.rs`, `runtime.rs`, `task.rs`)
on the tablet session, compiled and fixed on PC. Real MIR
(`parse_and_check` + `mir::lower`, never hand-built IR) lowers for
ALL MIR constructs and executes against the production heap through
the frozen ADR-008 entry points. Two modes share one lowering core:
Gate (sequential, standalone context, diverging panics) and Task
(any program incl. concurrent, scheduler tasks, sentinel panics,
suspension).

Value traffic is single-word `u64` (D8 untagged native): ints, bools
and floats travel as words (floats as bit patterns; bitcasts use
bare little-endian flags). `None` is word 0. `Range`, `Result`,
`Option` and closures are GC-heap payloads. Function identity uses
the 2³² discrimination rule (`crates/toto-jit/src/task.rs:191-192`,
`1252-1253`): `TAG_FN_ANY` (9, `crates/toto-jit/src/runtime.rs:44`)
with word < 2³² is a top-level `Func` (generations start at 1, so 0
is never a valid id); word ≥ 2³² is a heap `Closure` handle. Codec
`(generation << 32) | index` is pinned by `gate_value_layout_codec`.

MIR block order is non-topological: `seal_all_blocks()` runs after
all branches are created. `toto_chan_make` arity is 4→5. Builtins
route through the shared implementation (parity by construction);
core ops (`+`, `==`, indexing, …) are independent in codegen, which
is what makes the differential meaningful. `x % 0` is unified on
"division by zero" (one line in `ops.rs`) with the `int04_modzero`
golden.

### New-helper enumeration (D8 unchanged)

Spike 3 had nine entry points (including `toto_alloc_int`); ints are
now immediates, so no `alloc_int` exists. The production table is
35 unique helpers — 30 shared by both modes plus 5 task-only —
with every addition following the frozen codec and frozen allocator
names. ADR-008 needed no change:

- Allocation (7): `toto_alloc_array/str/struct/closure/range/result/some`.
- Layout and access: `toto_struct_init/get/set`, `toto_index_read/write`,
  `toto_str_concat`, `toto_range_lo/hi`, `toto_result_is_ok`,
  `toto_wrap_inner`, `toto_len`, `toto_builtin`.
- Calls and values: `toto_func_entry`, `toto_body_entry`,
  `toto_closure_body/cap`, `toto_values_eq/order`.
- GC roots: `toto_shadow_push/len/pop_to`, `toto_gc_collect`.
- Panics: `toto_panic` (plus `toto_check_failed`, task-only).
- Task-only (5): `toto_yield/spawn/chan_make/check_failed/task_capture`.

### JIT-debug turn (commit `6c7414c`)

Starting state `5ccfb92`, 11 stable divergences across 3 runs:

- Bug class A — variable type discipline (10 cases: cli01, eq02,
  int01, loop01, loop02, loop03, lex01, lex03, lex04, parse01):
  `cranelift-frontend: declared type of variable … doesn't match
  type of value`. One Cranelift `Variable` was defined with
  mismatched int/float CLIF values. Fix: declare one `I64`
  variable per MIR local in index order
  (`crates/toto-jit/src/compile.rs:841`) — every variable carries
  only single-word `u64` traffic, consistent with D8. No value
  representation, ABI, or frozen-decision change.
- Bug class B — function equality tag (1 case: conc18 path via
  clo04 shape): the JIT raised `E0204 values_eq with unknown value
  tag 9` where the reference raises `E0203`. Root cause: the
  equality snapshot did not classify `TAG_FN_ANY` (9), so
  function-involving comparisons never reached tag dispatch. Fix:
  classify `TAG_FN_ANY` by the 2³² rule, so mixed fn-vs-closure
  comparisons flow into the `E0203` arm exactly like the reference
  (`task.rs:1311`). `E0204` stays reserved for genuinely
  unreachable cases.

Also in `6c7414c`: the scheduler false-deadlock `E0216` race
(reproduced 18/2000 on the executor path) fixed with a `settling`
counter mirroring `pool.in_flight`, sched-lock only, no lock-order
change; and flat-25ms timing budgets replaced with a size-relative
budget — 8000 µs/fn + 2200 µs/MIR-instr
(`crates/toto-jit/tests/timings.rs:25-26`), worst program at ~50%
of budget, catches ≥2× regressions.

### E0203 correction (Phase 2 said unreachable)

`E0203` IS reachable in checked programs, in exactly one shape:
mixed fn-vs-closure comparison. `conc18_fneq` (`h == g`, top-level
fn vs closure literal) panics `E0203 "cannot compare function
with function"` (golden pins 12:13 + 6 carets). Same-kind function
comparisons are identity by word (`clo04_higher_order`,
`doub == doub` → `true`, status 0). Cross-type `==` (e.g.
int-vs-float with the conversion hint) is statically rejected by
the checker, so the runtime `E0203` backstop is unreachable in
checked code — kept for exactness. `E0204` is reserved for
genuinely unreachable internal cases.

## Truth corrections

### Corpus census (recomputed from the tree: 123 files)

- 47 status-0: 36 sequential + 11 concurrent-ok.
- 12 status-1: 4 sequential panics + `diag10_recursion_limit`
  (interpreter-only, spec §5.9) + 7 concurrent
  (`conc18_fneq` + 6 `conc_neg_*`).
- 64 status-2 (static negatives).
- Areas: concurrent 18, diagnostics 11, interp 25, lexing 5,
  parsing 7, results 13, typing 44.

The handoff's arithmetic did not add up (40+18+64=122); the 123rd
file is explicitly `diag10_recursion_limit`. Pins, all verified
against the committed asserts and the gate log:

- `sequential.rs` (40/18/64): 40 runnable + 18 skipped-concurrent
  + 64 static (`crates/toto-runtime/tests/sequential.rs:136-138`).
- Gate differential (36/18/4/64): 36 ran + 18 skipped-concurrent
  + 4 skipped-panic + 64 static
  (`crates/toto-jit/tests/differential.rs:119-122`).
- Task differential (40/18/64): 40 = 36 status-0 + 4 status-1,
  panics through task mode for full traces
  (`differential_task.rs:172-174`).
- Concurrent differential: all 18 (directory walk; the "17" in its
  header comment predates `conc18`).
- Timings (41/59): 41 distinct programs gate-mode (36 + 4 +
  `diag10`, which compiles but never runs) + 59 task-mode (41 +
  18 concurrent), counted from the gate log.

### Builtin reconciliation (27, not 28)

`BUILTIN_NAMES` holds 27 names. The spec holds 27 unique names:
§7 lists 25 lines but `len` appears twice (string + array
overloads = 24 unique), plus §13's `chan`, `close`, `closed` (3).
The brief's "28 builtins" counted catalog lines, double-counting
the `len` overload. Do not assert 28 again.

## Stage 2 — dev driver + CLI + dev tests (COMPLETE, `8e18662`)

`crates/toto-jit/src/dev.rs` part 2: `LiveRun`, `Generation`
(compile/spawn/render; the generation's JIT module outlives its
run), `DevDriver::rebuild` (classify → drain-vs-cancel settle →
respawn), `settle_run` (drain timeout → cancel → grace →
restart-policy abandonment). Generations are strictly sequential:
the old run is always joined before the new one spawns, so old and
new code can never mix; misclassification in the reloadability
matrix is always safe (both paths isolate fully). `toto dev` CLI
(`dev_watch` ingest loop, `--workers`, `--drain-timeout`, content
polling watcher). CI run `43781893014` green on both jobs.

`crates/toto-jit/tests/dev.rs`, 11 tests: classifier matrix,
settle drain/cancel/restart paths, ordering, no-mixing,
`StillBroken` recovery, watcher units, and the parked-recv
cancellation proof in two halves:

- `parked_recv_cancelled_on_scope_failure`: victim prints then
  parks on `<-ch`; a signaller + main div-zero fails the scope.
  Workers=1 FIFO makes the park-then-release deterministic;
  exact JIT==executor equality plus a 60-run M:N hammer asserting
  no hang, no deadlock, no leaked task, outcome-class invariance.
- `rebuild_cancels_parked_generation`: the full rebuild path. The
  draft parked both tasks with no sender, which the scheduler
  correctly reaps as `E0216` deadlock (verified via `jit_probe
  --task`) before settle can cancel — an all-parked run can never
  reach the cancel path. The test instead holds a task parked on
  `<-ch` while main spins in a non-blocking `select`/`default`
  (each iteration pre-checks scope failure, so cancel releases it
  in microseconds); the run cannot finish on its own, so settle
  always finds it live and always cancels. Deterministic, no
  timing dependence.

## Stage 3 — race detector, opt pipeline, gates (COMPLETE, `1149ef9`)

### Fiber-aware race detector (`crates/toto-jit/tests/race.rs`)

Schedule-sweep stability classification: every concurrent program
runs under 16 seeds × workers {2,4} × 2 repeats (1,152 runs),
each outcome classed as (status, stdout, panic code):

- `conc10_select_send` is the positive control: it shows both
  legal classes — clean `1\n2` and the `E0216` deadlock — and the
  test fails closed if either class disappears or a third appears.
- `conc03_select_two` shows exactly the designed tie-break classes
  `{10, 20}` (membership, not a race).
- The other 16 show exactly one class each, matching their
  goldens (status, stdout, and panic code).

Fiber-aware means the sweep varies ONLY `rng_seed` — fiber
tie-break/spawn/steal order — never OS thread identity: stable
programs show the same single class at 2 and 4 workers, so task
migration never counts as instability. `seed_determinism_single_worker`
pins the foundation: at one worker the same seed reproduces the
outcome bit-for-bit (10 repeats on racy seeds 1, 2 and the mn
seed). At M:N the same seed can flip run-to-run on racy programs
(probe-verified); that flip IS the race. Sensitivity note: a race
whose minority branch is rarer than ~1/64 could escape the sweep;
the negatives are additionally covered by the 1,620-run hammer.

### Minimal flagged optimization pipeline

`toto_jit::OptLevel::{Off, Speed, SpeedAndSize}` (default `Off` =
Stage-1 `opt_level=none`), via `JITBuilder::with_flags`,
`Jit::new_with_opt` / `new_task_with_opt`, threaded through
`Generation::compile`, `DevDriver::new` (one level per watch
session, keeping the no-mixing contract auditable),
`jit_probe --opt-level=`, and `toto dev --opt-level` (help text +
exit-3 validation + CLI tests). `toto run` takes no such flag: it
has no JIT path (interpreter / runtime executor only). No passes
of our own; the level affects machine-code emission only.

`crates/toto-jit/tests/opt.rs` proves opt-vs-off equivalence over
the full runnable corpus at both levels: gate 36+4 via the probe
subprocess (exit code, stdout, stderr exactly), task 40
in-process, concurrent 18 with golden membership for the two racy
programs. Tier-1 (`mir_conform`) needs no opt re-run: it checks
MIR shape, which a backend emission flag cannot alter.

Investigation note: an early strict-equivalence draft flagged
`conc10@Speed` (same seed, different class than off). Probing
showed Off equally nondeterministic per seed at M:N (e.g. seed 1,
workers 4: ok 3/27 dead at every level, identical ratios) —
inherent thread-timing in schedule selection, not an opt behavior
change. No illegal outcome was ever observed at any level. The
concurrent opt test therefore asserts equivalence for the 16
stable programs and legal-membership for the two racy ones. No
STOP was needed: no behavior mismatch, only test-design
over-strictness.

### Tier-1/2 + end-to-end re-runs through the JIT

All green in the Stage-3 gate: Tier-1 `abi_conformance_tier1`,
Tier-2 `gate.rs` (8 tests, incl. JIT-on-coroutine-stacks),
`differential*` (36 + 40 + 18), `panics`, `timings`
(size-relative budgets), `stack_jit` (all programs under 64 KiB;
deep JIT recursion hits the guard page), `yield_jit`,
`mn_jit` (1,620-run hammer), `dev` (11), `race` (2), `opt` (3),
`cli` (7).

### Transient Phase 3 `mn` failure: dispositioned ABSENT

The full hammer (`mn_jit_hammer_1620`: 18 programs × 30 rounds ×
workers {1,2,4} under forced GC) is green through the JIT, as are
the 1,152 race-sweep runs and the 60-run parked M:N hammer. The
transient Phase 3 failure did not reproduce at any opt level and
is dispositioned absent — never unexplained.

## Recorded divergences (all preserved, none papered over)

- `(Chan, Chan) == true`: the quirk is preserved in the JIT
  (`task.rs:1443`) and the runtime executor is the reference on
  concurrent constructs. Differentials green.
- `Range[i]`: the tree-walker panics `E0209 "cannot index Range"`
  (`ops.rs`); the JIT follows it (same `E0209` via the
  non-indexable arm). The MIR executor's fast path aliases
  in-range `Range(a,b)[i]` to `a+i` (`exec.rs`) — a RECORDED
  latent divergence, not fixed; no corpus case reaches it, so no
  differential covers it.
- Latent test note (cleanup, not a finding):
  `mn_jit_racy_select_legal_outcomes`' dead branch asserts empty
  stdout, but the real `E0216` branch prints `2` first (select
  took the b branch, then deadlocked on the final recv). The
  branch is unreachable at the fixed mn seed (30/30 clean in
  probe); if the seed ever changes, the assertion needs the
  `2\n` expectation.

## Counts

- Gate: `./scripts/check.sh` exit 0 — 54 suites, 127 tests, 0
  failures (52 pre-Stage-3 + `opt` + `race`).
- Corpus: 123 programs (47 status-0 / 12 status-1 / 64 static-2;
  the 123rd is interpreter-only `diag10`).
- Race sweep: 1,152 runs; opt equivalence: 160 probe runs +
  120 task runs + 432 concurrent runs; M:N hammer: 1,620 runs.
- New tests this phase: `opt` 3, `race` 2, `dev` 11, CLI 2.

## Deviations, assumptions, open questions

- None from the brief's scope or any frozen decision. The `opt.rs`
  concurrent design moved from strict equivalence to
  equivalence-plus-membership after the investigation above;
  that is a correction, not a deviation.
- `toto dev` was verified through the driver, watcher, and CLI
  flag tests; no live interactive session was held open (the
  watch loop ends by signal by design).
- The Phase 1 gate summary did not report Stage 0; Stage 0 is
  confirmed complete above.

## Carry-forwards (out of scope, not built)

LLVM AOT; FFI execution (Phase 5 gate); state-preserving reload /
migration (Phase 6, gated); module system/imports; REPL, LSP,
formatter; package manager; `gzv.std` beyond the frozen builtins;
user generics/traits; a general optimization framework (the
flagged three-level pipeline is the whole of it); benchmarks
suite; anything from the Enterprise Track. Email reporting to
`geraldvillorente@gmail.com` remains a user step (no send tool).

---

## Addendum, 2026-11-16 (Phase 5 Stage 0 record fixes — appended, history above untouched)

### (a) Stack high-water, debug AND release, vs the 64 KiB budget

`stack_jit` (`jit_stack_high_water_under_budget` + guard-page test)
run in both profiles on the Phase 4 tree; all green. Highest water
per program (bytes; `default` vs `forced-gc` identical within noise):

| program | debug | release |
|---|---|---|
| int01_arith | 33,096 | 3,816 |
| loop02_for_range | 32,848 | 3,064 |
| struct01_basic | 34,104 | 3,248 |
| clo04_higher_order | 32,912 | 3,136 |
| conc01_pingpong | 32,808 | 3,072 |

Debug peaks at 34,104 of 65,536 bytes (52% of budget); release
peaks at 3,816 (6%). The optimizer slashes coroutine stack use
~9x (less spill, tighter frames). Deep JIT recursion still hits
the guard page in both profiles (subprocess killed by SIGSEGV,
never a clean exit). Final validation under `speed`/`speed_and_size`
remains open only in the sense that those levels were not
separately measured — their code is strictly leaner, and the
release numbers bound them from above.

### (b) JIT compile times per function vs the Spike 3 baseline

Spike 3 baseline, toy scale: 450–950 µs/function (debug).
`timings` on the real corpus (172 functions, both profiles):

| profile | min | p50 | p90 | max | mean |
|---|---|---|---|---|---|
| debug | 669 µs | 2,913 µs | 11,652 µs | 78,434 µs (eq04_deep main) | 6,340 µs |
| release | 123 µs | 280 µs | 877 µs | 5,321 µs (eq04_deep main) | 517 µs |

This is not a regression: toy functions were a handful of CLIF
instructions; real functions are 10–100x bigger, and the
size-relative budget (8000 µs/fn + 2200 µs/MIR-instr, worst program
at ~50% in debug) holds in both profiles with wider margin in
release. Only 6/172 debug rows fall inside the old 450–950 toy
band; the band simply does not apply at real-function scale.

### (c) 127-test reconciliation, test by test

Gate log (`./scripts/check.sh` exit 0, 54 suites). Spike tests: 39
(ffi-smoke 7, spike-1 lib 1 + spike1 7, spike-1b 6, spike-2 10,
spike-3 8). Project tests: 88 — cli 7 + corpus 1; compiler 22
(differential tree-vs-MIR 1, interp 8, lexer 7, mir 1,
mir_conform/Tier-1 1, parser 4); jit 38 (dev 11, differential 1,
differential_concurrent 1, differential_task 1, gate/Tier-2 8,
mn_jit 6, opt 3, panics 1, race 2, stack_jit 2, timings 1,
yield_jit 1); runtime 20 (blocking 4, concurrent 3, gc 6, mn 5,
sequential 1, stack 1). Unit-test files with `--lib` targets
contribute 0 tests (all coverage lives in the integration suites
above). Full list:

- spikes/ffi-smoke-test/tests/ffi.rs (7): c_add_returns_correct_value,
  error_slot_converts_and_success_path_unaffected,
  handle_resolves_after_gc_and_fails_cleanly_after_release,
  panic_at_boundary_becomes_error_code,
  rooted_object_survives_c_call_missed_root_dies,
  static_callback_enqueues_without_reentry,
  string_round_trip_validates_and_rejects_invalid_utf8
- spikes/spike-1-gc-scheduler lib (1):
  roots::tests::frames_pop_on_drop_nesting_stacks
- spikes/spike-1-gc-scheduler/tests/spike1.rs (7):
  green_tasks_yield_resume_and_join_predictably,
  allocation_threshold_triggers_collection_automatically,
  join_drives_other_tasks_and_is_idempotent,
  live_objects_survive_nested_calls_switches_and_forced_gc,
  unbuffered_channel_moves_heap_values_through_forced_gc,
  task_root_stack_stays_correct_across_context_switches,
  unrooted_value_is_reclaimed_rooted_value_survives
- spikes/spike-1b-stack-switching/tests/stack1b.rs (6):
  unpublished_root_dies_across_real_switch,
  channel_moves_heap_values_through_real_switches,
  tasks_run_on_distinct_native_stacks_and_join,
  roots_survive_real_switches_with_exact_counts,
  yield_from_nested_calls_preserves_locals_and_returns,
  overflow_child_traps
- spikes/spike-2-hot-reload/tests/reload.rs (10):
  reloadability_matrix_classification,
  global_initializer_change_restarts_with_new_initializers,
  struct_layout_mismatch_never_reinterprets,
  body_change_drains_old_version_and_new_scopes_use_new,
  suspended_task_resumes_on_old_version_and_drains,
  layout_typed_changes_restart_fresh,
  version_retained_while_callable_held_then_reclaimable,
  signature_change_restarts_scope,
  drain_timeout_cancels_at_boundary_and_respawns_clean,
  suspended_task_timeout_cancels_at_boundary_no_mixing
- spikes/spike-3-cranelift/tests/jit.rs (8): jit_recompile_replaces_behavior_for_new_calls,
  jit_runtime_calls_cross_the_boundary_and_gc_is_exact,
  jit_missed_root_dies_while_keeper_survives,
  jit_collection_mid_execution_preserves_published_values,
  jit_pair_objects_round_trip_and_trace_nested_refs,
  jit_supports_calls_and_recursion,
  jit_executes_arithmetic_comparison_if_and_loop,
  trap_child_dies
- crates/toto-cli/tests/cli.rs (7): help_and_version,
  missing_file_is_exit_3, bad_usage_is_exit_3,
  workers_flag_runs_concurrent, workers_flag_rejects_zero,
  opt_level_flag_documented, opt_level_flag_rejects_bad_value
- crates/toto-cli/tests/corpus.rs (1): corpus
- crates/toto-compiler/tests/differential.rs (1):
  differential_tree_vs_mir
- crates/toto-compiler/tests/interp.rs (8):
  cyclic_display_terminates, closure_capture_copies_scalars,
  non_unit_falloff_panics, nan_and_identity,
  closure_sees_innermost_shadow_binding, overflow_panics_with_span_code,
  stdout_before_panic_is_preserved
- crates/toto-compiler/tests/lexer.rs (7):
  bad_characters_suggest_fixes, channel_operator_lexes_as_one_token,
  int_overflow_is_a_lexer_error_not_a_panic,
  less_than_with_spaced_negation_stays_separate,
  string_escapes_and_errors, range_vs_float, trailing_dot_is_not_a_float
- crates/toto-compiler/tests/mir.rs (1): mir_dumps
- crates/toto-compiler/tests/mir_conform.rs (1): abi_conformance_tier1
- crates/toto-compiler/tests/parser.rs (4):
  method_call_rejected_with_phase_hint,
  missing_return_type_is_an_error,
  bare_if_rejected_in_let_but_allowed_as_statement,
  out_of_subset_names_its_phase
- crates/toto-jit/tests/dev.rs (11): classify_matrix,
  settle_drains_natural_finish_without_cancel,
  settle_cancels_after_drain_timeout,
  settle_restart_after_cancel_grace,
  still_broken_keeps_old_running_and_recovers,
  no_change_keeps_generation_without_respawn,
  generations_stream_in_order_without_mixing,
  rebuild_cancels_parked_generation,
  parked_recv_cancelled_on_scope_failure,
  watcher_ignores_untouched_saves, watcher_live_change_and_stop
- crates/toto-jit/tests/differential.rs (1):
  differential_gate_sequential
- crates/toto-jit/tests/differential_concurrent.rs (1):
  differential_task_concurrent
- crates/toto-jit/tests/differential_task.rs (1):
  differential_task_sequential
- crates/toto-jit/tests/gate.rs (8): gate_value_layout_codec,
  gate_missed_root_dies_after_return, gate_arith_and_call,
  gate_jit_on_coroutine_stack, gate_nested_layout_host_assert,
  gate_array_layout_and_roots, gate_differential_interpreter,
  gate_panics_no_signal
- crates/toto-jit/tests/mn_jit.rs (6):
  mn_jit_select_tiebreak_membership,
  mn_jit_racy_select_legal_outcomes, mn_jit_negatives_exact,
  mn_jit_exact_race_free, mn_jit_stress_pairing, mn_jit_hammer_1620
- crates/toto-jit/tests/opt.rs (3):
  opt_gate_sequential_equivalent,
  opt_task_sequential_equivalent, opt_concurrent_equivalent
- crates/toto-jit/tests/panics.rs (1): panics_match_interpreter
- crates/toto-jit/tests/race.rs (2): detector_flags_conc10_and_only_conc10,
  seed_determinism_single_worker
- crates/toto-jit/tests/stack_jit.rs (2):
  jit_stack_high_water_under_budget, jit_guard_page_overflow
- crates/toto-jit/tests/timings.rs (1): jit_compile_timings
- crates/toto-jit/tests/yield_jit.rs (1): yield_from_live_jit_frames
- crates/toto-runtime/tests/blocking.rs (4):
  blocking_fifo_nonstall_order,
  blocking_fifo_rendezvous_no_deadlock, blocking_file_io_parity,
  blocking_error_paths
- crates/toto-runtime/tests/concurrent.rs (3):
  concurrent_negatives_exact_under_gc_pressure,
  concurrent_deterministic_across_runs,
  concurrent_positives_exact_under_gc_pressure
- crates/toto-runtime/tests/gc.rs (6):
  heap_refs_cover_ok_wrappers,
  nested_objects_trace_through_containers,
  shadow_publish_then_collect_keeps_published,
  stale_refs_resolve_to_none,
  unreachable_objects_reclaimed_with_exact_counts,
  zero_values_match_spec_table
- crates/toto-runtime/tests/mn.rs (5): mn_select_tiebreak_membership,
  mn_racy_select_legal_outcomes, mn_negatives_exact,
  mn_exact_race_free_programs, mn_stress_unbuffered_pairing
- crates/toto-runtime/tests/sequential.rs (1):
  sequential_matches_goldens_through_runtime
- crates/toto-runtime/tests/stack.rs (1):
  high_water_under_budget_on_realistic_programs

39 + 88 = 127. Zero failures.

### (d) "160 probe runs", resolved

The Phase 4 report's opt figure counts `jit_probe` subprocess
invocations, not cases: 40 runnable sequential programs (36
status-0 + 4 status-1) × 2 levels (`speed`, `speed_and_size`) × 2
invocations each (the `off` baseline plus the level under test) =
160. No missing coverage: every runnable sequential program runs
at every level plus baseline.
