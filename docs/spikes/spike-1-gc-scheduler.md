# Spike 1 report: single-threaded scheduler + tracing GC

- Prototype: `spikes/spike-1-gc-scheduler/` (isolated, throwaway).
- Date: 2026-11-14. Toolchain: rustc 1.100.0, edition 2024, zero deps.
- Gate: `./scripts/check.sh` (fmt --check, clippy `-D warnings`, workspace tests).

## Falsification verdict: D1 (tracing GC + shadow-stack roots) SURVIVES

The spike was designed to falsify the D1 default. It did not. Forced
collections reclaimed exactly the unreachable objects and nothing
else; live objects survived in nested frames, across task switches,
in flight in the channel, and in receiver inboxes; pauses are
microseconds on toy heaps. No evidence for the ARC fallback, no
reason to revisit D1/D2. Recommendation: **proceed to Spikes 2-3**;
re-validate at scale in the Phase 3 ABI gate and soak tests.

## Pass/fail per acceptance criterion (Section 5)

1. **Green tasks yield/resume/join predictably — PASS.**
   `green_tasks_yield_resume_and_join_predictably`
   (`spikes/spike-1-gc-scheduler/tests/spike1.rs`): three tasks,
   round-robin, exact expected log (ticks interleaved, dones in spawn
   order, each task ran exactly twice — none lost, none run twice).
   `join_drives_other_tasks_and_is_idempotent`: join drives other
   ready tasks; re-join is a no-op. Scheduler:
   `src/runtime.rs` (`Scheduler::spawn/join/run_all`, `Action`).
2. **Unreachable reclaimed; live survive; nested/switch roots found;
   no stale refs — PASS.**
   `live_objects_survive_nested_calls_switches_and_forced_gc`: list
   built through nested `cons` frames (one frame per call) survives
   two forced GCs across task switches; exactly 120 garbage objects
   reclaimed, exactly 7 live (3 heads + nil + 3 pairs).
   `unrooted_value_is_reclaimed_rooted_value_survives`: the control
   that would fail if a root were missed — the unrooted handle
   resolves to `None` after collection while the rooted one reads
   `Some(99)`. Stale handles resolve to `None` by (index, generation)
   check (`src/heap.rs`, `Heap::get`), never to a stranger's object.
3. **Root-stack state correct across switches — PASS.**
   `task_root_stack_stays_correct_across_context_switches`: keeper
   task roots one int, yields through 4 churn rounds (160 forced
   garbage objects); after every resume its shadow length is
   unchanged, the root is still published, and the value reads 4242.
   Roots unit test (`src/roots.rs`): frames nest and pop on drop.
4. **Unbuffered channel moves heap values — PASS.**
   `unbuffered_channel_moves_heap_values_through_forced_gc`: sender
   parks with a pair in flight → forced GC sees exactly 3 live (pair
   + 2 ints, rooted via the channel slot), 0 reclaimed; rendezvous
   moves the root to the receiver inbox → forced GC again 3 live;
   receiver reads back `got 7 8`. Move-on-send: the sender's root is
   removed at send (`Scheduler::handle`, `ShadowStack::remove_one`).
5. **Allocation behavior and pauses MEASURED — PASS (numbers below).**
   Rough spike numbers with method noted; no timing assertions in
   tests (only reported observations).
6. **`cargo test` deterministic; fmt + clippy clean — PASS.**
   8 tests (7 integration + 1 roots unit), no randomness, no
   timing-dependent assertions. `cargo fmt --all -- --check` clean;
   `cargo clippy --workspace --all-targets -- -D warnings` clean.
7. **This report — done.**

## Measurements (criterion 5; rough spike numbers, not benchmarks)

Method: `Instant`-based wall time around mark+sweep in
`Heap::gc_collect_for_test`; object/byte counters in `HeapStats`;
shadow depth via `ShadowStack::max_depth_observed`. Debug build on
the dev machine; counts are exact, times are observations only.

Demo (`cargo run -p spike-1-gc-scheduler --bin spike1-demo`,
producer→channel→consumer, 20 pairs + 20 deliberate garbage ints):

- objects allocated: 80; bytes allocated (approx): 3360
- objects reclaimed: 20; live at end: 60; max live: 60
- GC runs: 1 (explicit mid-run forced collection)
- forced-collection pause: 6-8 us across 3 runs (60 live objects)
- max shadow depth: 20 slots (160 bytes of handles)

Threshold test (`Hog`, 51 rooted ints, 256-byte threshold): 7
automatic collections, 25 us total, 4 us last; 0 reclaimed
(everything rooted — the point). Keeper test: 160 garbage objects
reclaimed across 4 forced GCs, 1 live. Stack usage: prototype tasks
have no native stacks (step functions — see deviations), so the only
meaningful "stack" number is shadow-stack usage: tens of slots
(hundreds of bytes). The provisional 64 KiB default green-thread
stack is therefore **unvalidated for native stacks** and must be
measured in Phase 3a with real stacks (corosensei); recorded here as
an open measurement, not a ratification.

## Context-switching crate comparison (why zero deps)

| Candidate | Verdict | Reason |
| --- | --- | --- |
| `corosensei` | Phase 3a first choice | Stackful, stable Rust, maintained; overkill for a root-protocol spike |
| `may` | Rejected | Full M:N runtime; brief forbids M:N here |
| `async-task`/futures | Rejected | Stackless; violates the D2 stackful default |
| `generator` crate | Rejected | Unmaintained |
| `libc` ucontext | Rejected | Deprecated/removed; unsafe surface |
| Hand-rolled assembly | Rejected | Unjustified for a protocol spike; brief requires documenting this |

Spike 1 uses std only. The falsification target is the GC-root
contract (what is published, where collection may run), not switch
mechanics, and native stacks would only add noise to that question.
No assembly was written.

## Safepoint protocol as implemented (`src/runtime.rs`, `Cx`)

1. **Allocation safepoint.** Every `Cx::alloc_*` checks the byte
   threshold *before* inserting, so the fresh (still unrooted) object
   is never at risk. Rule for task code: root each live handle
   (`root` / `alloc_rooted_*`) before the next allocation. A handle
   held only in a Rust local across an allocation may be reclaimed —
   proven by `unrooted_value_is_reclaimed_rooted_value_survives`.
2. **Yield/step-boundary safepoint.** A task suspends only by
   returning an `Action`; before returning it must publish every live
   handle (own shadow stack, inbox, or the `Send` value the scheduler
   moves into channel/receiver roots). The scheduler also checks the
   threshold at every step boundary, so threshold collections see all
   tasks parked at safepoints: true stop-the-world on one OS thread.
3. **No heap reference hides in a local across a safepoint.**
   Handles are plain integers, so the danger is an *unpublished*
   handle, closed by rules 1-2 by construction.
4. **Switches preserve root state.** Each task's shadow stack, inbox,
   and blocked status live in its own scheduler slot; a switch only
   changes which slot the running task is lent. Nothing is copied or
   merged. Nested calls open `with_frame` scopes (closure form: a
   guard object would borrow `Cx` for the frame's lifetime and block
   all other `Cx` use inside the frame).
5. **Reference identification: type descriptors in headers.**
   Every header carries `&'static TypeDesc` with a `RefMap`
   (`NoRefs` for ints/strings, bit `Mask` for pairs, `AllRefs` for
   arrays); the marker follows a slot only if the descriptor allows
   it. Chosen over a pointer-map side table because D8 already puts a
   type tag in the header — the map rides along for free.

## Files added

- `Cargo.toml` (workspace), `Cargo.lock` (committed per brief),
  `.gitignore`, `README.md` (toolchain, layout, license TODO),
  `scripts/check.sh` (gate: fmt --check + clippy + test).
- `crates/toto-runtime|compiler|cli/` — empty scaffolds + READMEs
  (production code starts Phase 1/3; not the spike).
- `spikes/spike-1-gc-scheduler/`: `Cargo.toml`, `README.md`,
  `src/lib.rs`, `src/heap.rs` (mark-sweep + descriptors),
  `src/roots.rs` (shadow stack + frames), `src/runtime.rs`
  (scheduler + unbuffered channel + safepoints), `src/main.rs`
  (`spike1-demo` measurements binary), `tests/spike1.rs` (7 tests),
  plus a shadow-stack unit test in `src/roots.rs` (8 tests total).
- `docs/decisions/adr-001-tracing-gc.md` (D1),
  `docs/decisions/adr-002-stackful-go.md` (D2).
- This report.

## Deviations, assumptions, open questions

Deviations from a production runtime (all documented, none precedent):

- Tasks are explicit step functions, not native stacks. Acceptable
  because the falsification target is the root/safepoint contract;
  real stackful switching is Phase 3a work (evaluate `corosensei`
  first). The scheduling/root-publication contract is what was tested.
- Stale handles resolve to `None` via (index, generation) — a
  spike-only debuggability aid, not a D8 decision. (Named
  `generation`, not `epoch`: D7 "epoch tags" are code versions, a
  different concept.)
- Finished tasks keep their slots (shadow + inbox still scanned) —
  conservative and simple; production drops them at join.
- A channel holds at most one parked sender and one parked receiver
  and panics otherwise; deadlock panics instead of reporting. Both
  are test-aid strictness, not language semantics.
- Collection threshold defaults to "never" (`u64::MAX`) so tests are
  deterministic unless they opt into auto-collection.

Assumptions: repo host unknown → no CI config, `scripts/check.sh`
is the gate (recorded in README). D3 (move-on-send, no shared
`&mut`) followed by construction; the checker comes in Phase 2.
D4/D5/D6/D7/D8 need no spike code beyond what's noted.

Open questions for later phases: native green-thread stack size
(measure in Phase 3a); real header/descriptor encoding (D8 freeze,
Phase 2); whether a timer trigger is ever needed (allocation
threshold + forced collection sufficed here); fiber-aware race
detection (Phase 4 tooling, not a gate).

## Provisional defaults used (Section 4 — awaiting ratification)

- Integer overflow (checked/panic): not exercised by this spike;
  recorded only.
- Default green-thread stack 64 KiB: shadow-stack usage measured
  (tens of slots); native-stack validation deferred to Phase 3a.
- `toto dev` drain timeout 5 s: Phase 4 concern; untouched.
- Keep typed `Result<T,E>` (no simplification): not exercised.

None of these is treated as final; all are flagged here for
ratification.

## Recommendation

D1 SURVIVES; D2 needs no change (scheduler semantics proven,
mechanics deferred as planned). Proceed to Spike 2 (hot reload)
and Spike 3 (Cranelift) — no D1/D2 revision, no re-spike. Caveat:
toy heaps (tens of objects, microsecond pauses); the Phase 3 ABI
gate and soak tests must re-validate root scanning and pauses at
real scale before the default is load-bearing.
