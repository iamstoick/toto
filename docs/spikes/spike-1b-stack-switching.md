# Spike 1b report: real stackful switching with shadow-stack GC

- Prototype: `spikes/spike-1b-stack-switching/` (isolated, throwaway).
- Base: branch `spike-1/gc-scheduler`; all Spike 1 tests still pass.
- Date: 2026-11-14. Toolchain: rustc 1.100.0, edition 2024.
- Switching: **corosensei 0.3.4** (latest overall line per cargo
  resolution; 0.2.2 is a stale line — supersession check done, see
  below). Tested on Linux x86_64 only.
- Gate: `./scripts/check.sh` (fmt --check, clippy `-D warnings`,
  workspace tests).

## Falsification verdict: D2 (and the D1+D2 combination) SURVIVE

Real stackful switching works cooperatively on one OS thread with
exact GC: tasks run on separate native stacks, yield from nested
calls with locals intact, publish roots before switches, and forced
collections while tasks are suspended mid-computation reclaim
exactly the unreachable objects. The missed-root control proves the
native stack is never scanned and the publish-before-switch rule is
load-bearing. Guard pages turn runaway recursion into a defined
trap. No evidence against D1 or D2.
Recommendation: **proceed to Phase 3a** (single-threaded runtime on
this mechanism) **and Spikes 2-3** — no D1/D2 revision, no re-spike.
Caveat: toy heaps and hand-written task bodies; generated-code frame
depths must re-validate the 64 KiB default (see below).

## Pass/fail per acceptance criterion (Section 5)

1. **Real switching on separate native stacks — PASS.**
   `tasks_run_on_distinct_native_stacks_and_join`
   (`spikes/spike-1b-stack-switching/tests/stack1b.rs`): two tasks
   record disjoint stack ranges (asserted non-overlapping), each
   task's stack probe lies inside its own range (execution really
   happened there), round-robin log order, idempotent join. No task
   lost or double-run (each logs ticks + done exactly once).
   Scheduler: `src/sched.rs` (`Scheduler::spawn/join/run_all`,
   `drive` resumes a `corosensei` coroutine per poll).
2. **Nested yield — PASS.**
   `yield_from_nested_calls_preserves_locals_and_returns`: task
   suspends 4 frames deep (`nest_task → l2 → l3 → l4 → yield_now`),
   3 rounds with distinct sentinels; after every resume, locals at
   all four levels and the full return-value chain are asserted
   intact (`r == 6000 + 4*round`).
3. **Roots across real switches — PASS.**
   `roots_survive_real_switches_with_exact_counts`: list built
   through nested frames survives two forced GCs while the builder
   is suspended mid-computation (handle in a native local + shadow
   root); exactly 120 garbage objects reclaimed, exactly 7 live,
   shadow depth stable (`depth=1`) across switches in both
   directions. `unpublished_root_dies_across_real_switch` is the
   mirror control: an unrooted handle kept only in a native-stack
   local is reclaimed by a GC forced mid-suspension
   (`live_after == 1`), while the published keeper reads `Some(99)`.
4. **Channel through real switches — PASS.**
   `channel_moves_heap_values_through_real_switches`: sender builds
   pairs in nested frames and sends from 2 deep; receiver recvs from
   2 deep. Forced GC while the sender is parked: 3 live / 0
   reclaimed (channel-slot root); after rendezvous: 3 live / 0
   reclaimed (inbox root). Three values arrive in order with intact
   contents (`got 7 700`, `got 8 800`, `got 9 900`).
5. **Stack evidence MEASURED — PASS (numbers below).** High-water
   per task, guard-page mechanism + trap demonstrated, fixed-size
   finding, 64 KiB verdict below.
6. **Switch cost reported, rough only — PASS (numbers below).** No
   timing assertions in tests.
7. **Spike 1 tests still pass; gate clean — PASS.** 7/7 Spike 1
   tests unchanged and green; 6 new 1b tests green; `cargo fmt
   --check` and `cargo clippy --all-targets -D warnings` pass via
   `scripts/check.sh`.
8. **This report — done.**

## Measurements (rough spike numbers, debug build, method noted)

Method: pattern-fill the writable stack at spawn (`0xAA` words via
`measure::fill_pattern`, guard page untouched), scan upward after
task completion (`scan_high_water`); heap/GC numbers from
`HeapStats`/`Instant` as in Spike 1. Counts exact; times observed.

Demo (`demo1b`: producer→channel→consumer, 10 pairs + 10 garbage
ints, 64 KiB stacks, 3 runs — high-water exact every run):

- heap: 40 allocated / 10 reclaimed / 30 live; mid forced GC 4–5 us
- producer high-water: **2848 B** of 65536 usable (4.3%)
- consumer high-water: **2056 B** (3.1%); includes `corosensei`
  bootstrap frames, `RefCell` machinery, nested `with_frame` calls
- switch cost: **114–145 ns/resume** (10002 resumes, ~1.1–1.5 ms
  total, debug build)

**64 KiB verdict: sufficient for spike workloads, inconclusive for
real codegen.** Spike task bodies (nested calls, closures, channel
rendezvous, forced GCs) use under 5% of 64 KiB — ~23× headroom. But
these are hand-written shallow bodies, not generated code with
unknown frame sizes and recursion. Do NOT ratify 64 KiB from this:
recommend 64 KiB initial + guard pages for Phase 3a, re-measure with
real generated code, grow only on evidence.

## Corosensei verdict (version, platform, limitations)

- **Version: 0.3.4**, the latest overall line (a `"*"` requirement
  resolves to 0.3.4; `"0.2"` pins the stale 0.2.2 line — same API,
  older release). No maintained equivalent supersedes it; used
  directly per the brief's first choice. No assembly written.
- **Tested: Linux x86_64 only** (dev machine). Not tested: every
  other OS/arch in corosensei's support matrix (other Unixes,
  macOS, Windows, AArch64, …). No portability claimed.
- **Dependencies added: `corosensei 0.3`, `libc 0.2`.**
  `libc` is measurement-only (`sysconf(_SC_PAGESIZE)` for the guard
  skip); switching itself needs only corosensei. `Cargo.lock`
  updated and committed.
- Limitations found: (a) stacks are fixed-size — no growable option
  in the used API; (b) coroutine bodies must be `'static`, which
  forced the `Rc<RefCell<World>>` sharing shape (safe here:
  single-threaded, no borrow held across a suspend); (c) no
  built-in high-water reporting — measured by us; (d) `into_stack`
  consumes the coroutine, so high-water is available only after a
  task finishes (fine for join-then-measure; a live-task watermark
  would need more machinery).

## Safepoint protocol v2 as implemented (`src/lib.rs`, task/sched)

1. **Allocation safepoint** (unchanged from Spike 1): every
   allocator checks the byte threshold before inserting; root each
   live handle before the next allocation.
2. **Switch/yield boundary** (new): a task suspends only through
   `TaskRt::{yield_now, send, recv}`. Before suspending, every live
   handle must be published (own shadow stack, inbox, or the `send`
   payload the scheduler moves into channel/receiver roots). The
   native stack is never scanned — proven by the missed-root
   control, which dies exactly because its handle sat only in a
   native local.
3. **Enforcement:** `send` asserts at the suspend site
   (`publish-before-switch: send of unpublished root` panics
   otherwise). `yield_now` carries no payload so nothing checkable
   crosses it: held by task-code construction plus the control test.
4. **Shadow-stack pointer at switch time:** nothing is saved or
   restored — each task's `ShadowStack` lives in scheduler-owned
   `World` memory, never on a native stack. A switch swaps the
   native stack (corosensei); the running task's `TaskRt` selects
   which shadow is lent. Depth/contents asserted equal across
   switches in both directions by the tests.

## Files changed/added and reuse choice

- **Reuse (no fork):** `spike-1b` depends on `spike-1-gc-scheduler`
  by path and reuses `heap` (Heap, GcRef, descriptors, forced
  collection) and `roots` (ShadowStack) unchanged in behavior. One
  additive refactor on the Spike 1 branch state:
  `ShadowStack::open/close/frames_len` widened `pub(crate)`→`pub`
  (committed on this branch before the 1b code; all 7 Spike 1 tests
  still pass unmodified). A full copy was rejected (divergence
  risk); a shared extracted crate was rejected (overkill for two
  throwaway spikes).
- **Reimplemented, not reused:** the scheduler and channel
  rendezvous (`src/sched.rs`, `src/task.rs`, `src/world.rs`,
  ~600 lines). The rendezvous semantics mirror Spike 1, but the
  parking mechanics differ fundamentally (coroutine suspend/resume
  vs step-function return), so sharing the code would have meant
  parameterizing over the suspension mechanism — more complexity
  than the ~60 lines of rendezvous logic justify.
- **New:** `src/measure.rs` (pattern fill/scan, `unsafe` confined
  here with SAFETY notes), `src/bin/demo1b.rs` (measurements +
  switch-cost micro-scenario), `src/bin/overflow.rs` (guard-page
  probe child), `tests/stack1b.rs` (6 tests), `README.md`,
  `Cargo.toml`. `Cargo.lock` updated (corosensei 0.3.4, libc).
- Root `README.md` layout section notes the 1b prototype.

## Deviations, assumptions, open questions

Deviations (documented, none precedent-setting):

- `Rc<RefCell<World>>` sharing: forced by corosensei's `'static`
  body bound; discipline is single-threaded alternation with no
  borrow held across a suspend (a violation panics loudly via
  `RefCell` instead of corrupting).
- `unsafe` confined to `measure.rs` (raw fill/scan of owned stack
  memory) — measurement only, never switching.
- Threshold default "never" (`u64::MAX`); deadlock panics; finished
  tasks' shadows still scanned (Spike 1 simplification, kept).
- The `send`-site root assert is runtime enforcement; the
  yield-side rule rests on construction + control test (stated
  honestly, not oversold).
- Overflow test is unix-only (`SIGSEGV` assertion); no trap
  handler, no recovery (per brief).

Assumptions: Linux x86_64 dev machine (page size read at runtime,
not assumed); repo host still unknown → still no CI config.

Open questions for Phase 3a: real stack size under generated code
(re-measure); growable stacks only on evidence; `trap_handler` for
the failure policy (deadlock reporting, panic-in-task rules);
whether `send`-site asserts stay on in production builds; M:N
coordination of safepoints stays a Phase 3b problem.

## Provisional defaults (Section 7 — unchanged, awaiting ratification)

- Stack size (64 KiB initial, growable, guard pages): UNDER TEST
  here — measured 2–3 KiB high-water on spike workloads; verdict
  "sufficient for spikes, inconclusive for codegen" above.
- Checked-overflow panics, 5 s `toto dev` drain timeout, typed
  `Result<T,E>`: unchanged, not exercised, not implemented, not
  ratified.

## Recommendation

D2 SURVIVES on real stacks; the D1+D2 combination SURVIVES with the
v2 protocol. Proceed to Phase 3a (single-threaded runtime on
corosensei at 64 KiB + guards, re-measure under generated code) and
Spikes 2-3. No D1/D2 revision, no re-spike — with the standing
caveat that generated-code frame depths and real heap sizes must
re-validate both the stack default and pause behavior before either
is load-bearing.

Guard pages: `DefaultStack` mmaps the stack with a `PROT_NONE` guard
page at the bottom (`src/stack/unix.rs` in corosensei 0.3.4; the
`Stack` trait safety contract *requires* a guard). Demonstrated:
`overflow1b` recurses with ~4 KiB frames on a 64 KiB stack; the
parent test (`overflow_child_traps`, unix-only) asserts the child
dies by `SIGSEGV` — not a clean exit — after printing recursion
depths to stderr. Defined trap, no silent corruption, no recovery
attempted (per brief).

Fixed vs growable: `DefaultStack` is fixed-size (one mmap per task,
no segmented/growable option in the used API). Finding: **fixed**.
For Phase 3a, stay fixed at 64 KiB with guard pages; consider
growable stacks only if real workloads overflow, with measurement.
`corosensei::Coroutine::trap_handler` exists for graceful overflow
handling but was deliberately not used (production hardening is out
of scope); flag it as a Phase 3a failure-policy option.
