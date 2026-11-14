# Phase 3 — Runtime + Concurrency report (Stages 1–3)

Status: **Stages 1, 2, and 3 COMPLETE and green.**

## Verdict

Phase 3 exit criteria are met: the single-threaded milestone (Stage 1)
still passes unchanged; the M:N work-stealing scheduler runs the full
concurrent corpus green with a decided/tested blocking-I/O policy
(Stage 2); the Tier-2 ABI gate compiles real MIR with Cranelift and
executes it against the production heap through the frozen ADR-008
entry points, including a JIT-on-coroutine-stacks proof (Stage 3).
Two M:N lost-wakeup races and one error-trace fidelity bug were found
by stress testing and fixed (no spec or frozen-D8 change). Carry-
forwards to Phase 4 are listed at the end.

## Stage 0 confirmation

- `phase-2/checker-ir` merged to `main` with a merge commit, pushed,
  CI green (prior session; branch work continues on
  `phase-3/runtime` off `main`).
- README status line updated to Phase 2 complete / Phase 3 in
  progress (commit `e090249`).
- Default branch is `main`.

## Reconciliations

1. **The 2 non-surviving programs** are the parser negatives
   `tests/corpus/parsing/parse03_reject_go_select` and
   `parse04_reject_channel` (4 files each: `.gzv`/`.status`/`.stderr`/
   `.stdout`, deleted). They asserted the parser *rejects*
   `go`/channels; Phase 3 enables `chan<T>`, `go`, `<-`, `select`,
   `close`, so the rejections are obsolete. Replaced by
   `parse05_bare_select` (bare `select` is still rejected), the 17
   concurrent corpus programs, and 8 typing negatives covering the D3
   rules. Nothing was dropped in error.
2. **"Map values"** meant the Phase 2 note that `Map<K,V>` checks as a
   type but has no values or constructors. Per `docs/spec.md`
   (§12: "`Map<K,V>` checks as a type but no values"; §13.1: "`Map`
   has no values, so a `Map` zero never arises") this is intentional:
   Maps are type-only in v1 (D5, frozen builtins contain no Map
   constructors). **Nothing outstanding; no owner phase needed.**
   (If a future stdlib phase wants runtime Maps, that phase owns it.)
3. **The three §0 decisions are in `docs/spec.md`**: function-type
   syntax `fn(T1, T2) -> R` (§12), `?` identity-only with `E0308`
   (§12, error table), recursive types rejected with `E0302` (§12).

## What was built (Stage 1)

Production runtime in `crates/toto-runtime/` (spikes untouched):

- `heap.rs` — stop-the-world non-moving mark-sweep, descriptors in
  headers, generational indices, allocation-threshold trigger plus
  the test-only forced-collection hook
  (`RuntimeConfig::collect_at_every_safepoint_for_test`, the
  `gc_collect_for_test` equivalent).
- `sched.rs` — corosensei stackful switching, 64 KiB FIXED stacks
  with `PROT_NONE` guard pages, FIFO ready queue, SplitMix64-seeded
  RNG for `select` tie-breaks (deterministic mode = fixed seed).
- `channel.rs` — bounded/unbounded `chan<T>`, send/recv/close/
  `closed`, `select` with default, structured scopes (scope waits for
  children; first-panic-wins; cancellation at suspension
  boundaries). Parked-recv cancellation is NOT built (Phase 4 item).
- `exec.rs` — ONE MIR executor on coroutine stacks AND the host
  thread (differential mode). Extends the Phase 2 MIR interpreter
  logic, borrowing `ops`/`builtins` via memoized sharing-preserving
  conversion (`conv.rs`), with native paths for aliasing-sensitive
  operations (field/index reads alias like the interpreter;
  `push`/`pop`/`len` mutate in place). Tree-walker stays the
  semantics reference.
- Safepoint rule (frozen ADR-008): publish-before-switch; collection
  at allocation AND switch/suspension safepoints on threshold.
- `measure.rs` — pattern-fill high-water tracking;
  `examples/overflow.rs` demonstrates the guard abort (SIGSEGV, exit
  139 — defined trap, never silent corruption).

Checker/parser: `chan<T>`, `go`, `<-`, `select`, `close` enabled;
FFI blocks and user generics stay rejected. D3 enforced with
diagnostics naming the rule: `E0314` (`go` `mut` capture,
send-moved-use, field/index send unless deeply immutable),
`E0315` (close of `chan<chan<_>>`/`chan<fn>` — no zero value).
Frozen builtins stay 25 plus `chan`/`close`/`closed` = 28.

## Bugs found and fixed during hardening

All were implementation bugs; no spec or D8 change:

1. **Suspension safepoints never collected.** The code documented
   "allocation + switch safepoints" but only allocations and explicit
   `Safepoint` instructions ran the threshold check. `yield_now` /
   `park` published roots but never collected. Fixed: both run
   `maybe_collect` (sound — channel/select waiters are gathered
   roots).
2. **No test-only forced-collection hook** although the brief
   requires one. Added
   `RuntimeConfig::collect_at_every_safepoint_for_test` (default
   `false`; CLI never sets it), threaded through `Shared`.
3. **Executor `len` rejected strings** with the wrong code/message
   (`E0209` "needs an array"). Now mirrors the interpreter exactly:
   chars for strings, `E0210` with the shared message. Caught by
   `lex02_strings`.
4. **Closure `==` always false.** Conversion minted a fresh `Rc`
   per operand, defeating the interpreter's `Rc::ptr_eq` identity.
   The executor short-circuits `Arc::ptr_eq` natively. Caught by
   `clo04_higher_order`.
5. **Sequential test ran file IO under the wrong cwd** (restored
   before executing), breaking `cli01_fileprog`; and asserted
   `ran > 100` against a 39-program runnable set. Fixed: hold the
   case dir through execution (as the CLI runner does), pin the
   exact census (39 run / 17 concurrent-skipped / 64 static /
   1 interpreter-only), and remove the `cli01_tmp.txt` artifact.
6. **`let mut z` missing in `gc.rs`** (would not compile).

## Determinism

All 17 concurrent programs are **exact-golden** under the fixed
seed, run twice (functional + determinism re-run). No
invariant-checked tests — nothing in the corpus is nondeterministic
by design; stress tests for genuine nondeterminism belong to
Stage 2 (M:N).

## Stack measurements (64 KiB budget)

Pattern-fill high-water per task, five representative programs:

| program      | debug hw  | release hw |
|--------------|-----------|------------|
| pingpong     | 63,128    | 3,584      |
| fanout       | 63,128    | 3,584      |
| select       | 63,160    | 3,624      |
| deep-loop    | 63,160    | 3,624      |
| struct churn | 63,128    | 3,584      |

Release builds use ~3.5 KiB (5% of budget, all in the top page).
Debug builds use up to ~63.2 KiB (margin ~2.4 KiB): unoptimized
giant frames (`exec_instr`, conversions). Verified by fault
isolation (write-protected bottom page → fault inside legitimate
`print`/`format_value` frames) and a debug-vs-release comparison —
not a stray writer. Guard overflow demonstrated by
`examples/overflow.rs` (SIGSEGV, exit 139). Budget verdict:
**comfortable for optimized builds; thin in debug builds.**
Final validation under JIT-generated code stays a Phase 4 gate
item. `stack.rs` pins debug high-water under budget.

## Ownership redesign (M:N-ready)

No `Rc`/`RefCell` in `crates/toto-runtime/src`. All cross-task
state lives in one `Arc<Shared>` (`Mutex<State>` = heap + channels
+ shadows + select waiters, `Mutex<ScopeTable>`,
`Mutex<output>`); coroutine bodies are `'static` (only `Arc`s and
plain data cross). Executor state is task-owned. Stage 2 may share
workers without redesigning ownership.

## Blocking I/O policy (decided, recorded for Stage 2)

**Dedicated blocking pool** (the PDF lean), recorded before M:N
lands as required: blocking builtins (`read_file`, `write_file`,
`file_exists`) execute on a separate pool so worker threads never
stall on unpredictable file-IO latency. Green threads awaiting IO
park normally; scope cancellation still applies at suspension
boundaries only (no parked-recv cancellation in v1). Rationale:
file IO has unbounded latency (NFS, pipes, FUSE); stealing a
worker per blocked call breaks scheduling guarantees, while a
pool isolates the damage with a separately tunable size. Implemented
in Stage 2 (see above); `blocking_pool_threads` defaults to 2.

## Stage 2 — M:N scheduler + blocking pool (COMPLETE)

Production M:N in `crates/toto-runtime/src/sched.rs` (rewritten;
single-threaded paths and all Stage 1 tests unchanged in behavior):

- **Scheduler**: per-worker run queues + work-stealing (a steal from
  another worker's queue counts in `RunStats.steals`), 1 ms
  `sched_cv` sleep when idle (timeout doubles as the GC-rendezvous
  progress guarantee), `TaskState::Running` checkout markers so a
  worker handling a task is never misdiagnosed as idle.
- **STW GC rendezvous**: stop-the-world barrier coordinated across
  workers at safepoints (no concurrent collection, D1). Publication
  covers every non-`Done` slot's pending response (verified when the
  completion-drain moved inside the decide section).
- **Blocking I/O policy** (decided Stage 1, implemented now):
  dedicated blocking pool (`read_file`/`write_file`/`file_exists`
  execute on pool threads; workers never stall on file syscalls).
  `blocking_pool_threads` defaults to 2, tunable via
  `RuntimeConfig`. Test `blocking.rs` proves a blocking call does not
  stall other green threads and that file builtins keep exact
  interpreter parity (stdout, exit code, and — after the trace fix
  below — panic traces).
- **Determinism story**: `concurrent.rs` runs the 17-program corpus
  at `workers: 1` (exact goldens); `mn.rs` runs the same corpus at
  `workers: 4` with forced collection at every safepoint — race-free
  programs assert exact goldens (schedule-independent single-printer
  output), `conc03` asserts membership in the two seeded tie-break
  outcomes, `conc10` asserts the two legal outcomes of its genuine
  program race, negatives assert exact goldens. `mn_stress_unbuffered_pairing`
  (8 senders × 40 rounds over one unbuffered channel, sum invariant)
  hammers the pairing paths at 1/2/4/8 workers plus a forced-GC pass.
- **CLI**: `toto run [--workers N] <file.gzv>` (default 1,
  deterministic); `--workers 0` and unknown args are exit-3 usage
  errors. Covered by two new CLI tests (execution at `--workers 4`,
  rejection of `--workers 0`).

### Races found by stress testing and fixed

Both are implementation bugs (no spec/D8 change); both were
intermittent under M:N only and invisible single-threaded:

1. **Check-then-register split (both directions).** The pairing
   decision (scan for a partner) ran in one lock hold and
   registration + parking in later holds. Two complementary tasks
   could both observe an empty list and both park — a lost wakeup and
   a bogus `E0216` (~1/9 runs on `conc01` at 4 workers). Fix: the
   task registers in the waiter list *inside* the decision hold
   (register-first; the scan skips the running task's own entry via
   `keep`), so holders serialize and whoever arrives second always
   observes the first.
2. **Register-then-park split (mirror race).** After fix 1, a pairing
   landing between registration and parking had its wake dropped
   (`enqueue_if_blocked` only wakes `Blocked` tasks) and the later
   park stranded the task (~1/300 runs). Fix: the park moved *into*
   the decision hold — a visible waiter entry now always belongs to
   a `Blocked` task, so a popped peer is always woken. The same
   single-hold fusion was applied to all five park paths (`op_send`,
   `op_recv`, `op_select` Block, both select re-parks); the pump
   needs no change (it re-scans after every mutation).
3. **Deadlock-claim vs completion-delivery race.** Pool completions
   were drained outside the decide section: a worker could observe an
   idle-looking pool while another worker held a drained-but-not-yet-
   enqueued completion, claim a bogus `E0216`, and the late enqueue
   landed after `finished`. Fix: `op_blocking` parks + submits the
   pool job atomically, and the completion drain runs *inside* the
   decide critical section (new `drain_completions_locked`, with a
   stale-`seq` guard so released/re-parked tasks drop stale
   completions). The `in_flight` pool counter (jobs popped but not yet
   completed) closes the analogous pre-claim gap.
4. **Blocking-error trace fidelity.** A pool-reported failure
   delivered `OpPanic` without the interpreter's
   `{name} (builtin)` trace frame. `builtin_blocking` now pushes the
   frame before parking (popped on success/cancel, kept on panic).

A concurrent-scope refinement made in passing: every park path
fail-fasts a scope that died between the executor's pre-yield check
and the park hold (`Cancelled` to self instead of stranding), and
direct re-parks park unconditionally (the running task cannot have
been released — the early return would strand it with no queue
entry). The old unconditional `park()` helper is gone; `enqueue_if_blocked`
remains the conditional wake (load-bearing for release-vs-pair).

Lock discipline (documented in `sched.rs`): `scopes → sched → state`,
plus `sched → done`, `sched → pool`, `sched → gc` for the nested
sections. No cycles; verified by the full suite plus a 1,530-run
M:N stress (17 programs × 30 rounds × workers {1,2,4} under forced
GC) with zero mismatches.

## Stage 3 — Tier-2 Cranelift gate + JIT-on-coroutine-stacks (COMPLETE)

New crate `crates/toto-jit` (8 gate tests; explicitly the
conformance gate, NOT the Phase 4 backend). It compiles **real MIR**
(`parse_and_check` + `mir::lower` output — never hand-built IR) for
a documented subset and executes it with Cranelift 0.137 against the
**production** heap through entry points with the frozen ADR-008
names and semantics:

- **Subset** (everything else is `SubsetError::Unsupported`): single-
  block bodies ending in `Return`; int arithmetic with explicit
  checks, int/float comparisons, `Div`/`Mod` structurally paired
  with their `DivCheck` (verified, span-faithful); static
  `FuncRef`-resolved calls (ctx-first, like all generated calls);
  `ArrayLit` (via `toto_alloc_array` + per-element `toto_write_field`),
  `Index` (via `toto_read_field`, exact interpreter message/span),
  `Len`/`len()` (via `toto_array_len`); `Const` int/bool/float/unit/
  string; any `Safepoint` publishes all defined handle locals
  (conservative auto-generation rule). Epilogues pop to the
  prologue-recorded base, so a returned handle is unrooted by
  construction (documented; callers resolve before collecting).
- **Tier-2 item 1 (value layout)**: codec tests
  (`(generation << 32) | index`, round-trip, stale → `None`), plus
  host-side layout assertions on JIT-built nested arrays (descriptor-
  traced references, lengths, contents).
- **Tier-2 item 2 (root publication)**: threshold-1 collections with
  `reclaimed_total == 0` (every object survives under published roots
  only); the missed-root control runs at the epilogue boundary (an
  unrooted return value dies under the next collection and resolves
  to `None` — auto-publication makes in-frame unrooted values
  impossible by construction, documented); shadow depth balanced
  across calls in both directions.
- **Tier-2 item 3 (entry points)**: ctx-first everywhere (including
  JIT-to-JIT calls, tested); threshold checked before inserting
  (Spike-1 rule); `toto_panic` for div-zero / overflow / `MIN/-1` /
  negative-`Neg` / out-of-bounds / stale-handle — exact interpreter
  codes, messages, and span bytes (verified case-by-case against
  `run_source`, including `index 5 out of bounds for array of length
  2`). C-type choices (trailing span words, `u64` value traffic with
  kind tags, static message bytes) are recorded in `runtime.rs`, not
  the ADR — the ADR freezes names and semantics, not C types.
- **Tier-2 item 4 (coroutine stacks)**: JIT functions execute on a
  64 KiB corosensei coroutine stack; the harness body acts as the
  runtime (publish, suspend via the reserved name `toto_yield` with a
  gate-only signature, resume with a token that crosses back into JIT
  code); a forced collection while suspended keeps exactly the one
  published object alive; shadow depth is 1 across the switch and 0
  after return. Yield-from-inside-live-JIT-frames is Phase 4 (it falls
  out of park-safepoint lowering); stack switching itself is
  corosensei's proven domain (Spike 1b).
- **Tier-2 item 5 (no signals)**: all seven formerly-trapping paths
  exit 1 with spec-§6 output — never a signal. No `trap` instruction
  is emitted anywhere by construction (unreachable post-panic paths
  return a dummy).
- **Mini-differential**: five programs (arith+call, div/mod, nested
  arrays, negative operands, `MIN % -1`) agree exactly between the
  tree-walking interpreter and the JIT; panic parity is covered by the
  subprocess goldens above.

Two additive production APIs enable the gate (no behavior change):
`GcRef::index/generation` (the codec needs them) and
`ShadowStack::push/pop_to` (codegen-emitted scoped publication; the
MIR executor keeps its rebuild-scan discipline). The reserved name
`toto_yield` is used with a gate-only signature (reserved signatures
are explicitly unfrozen).

### Finding: cosmetic `mod-zero` message divergence (spec-compliant)

The gate surfaced a latent tree-walker-vs-MIR divergence: `x % 0`
reports "modulo by zero" in the tree-walker but "division by zero"
in every MIR executor (DivCheck fires first — same `E0202`, same
span). No corpus program covers `% 0`, so no golden pins either
wording, and the spec (§5.2) pins codes, not messages. The JIT
matches the MIR interpreters exactly. Carry-forward: unify the
wording or bless the difference in the Phase 4 differential (codes
already agree).

## Phase 4 carry-forwards

- Full Cranelift JIT backend extending the gate subset (control flow,
  remaining MIR ops and value families, real liveness, rodata,
  trace-depth in panics, returned-handle rooting convention).
- Yield lowering inside generated code (park safepoints → switch
  points) on top of the proven stack/root mechanics.
- `mod-zero` message wording (unify or bless; see above).
- Fiber-aware race detector (Phase 4 tooling, explicitly not built).
- Debug-mode stack margin stays thin (~2.4 KiB); final validation
  under JIT-generated code is a Phase 4 gate item.
- Out of scope throughout (unchanged): `toto dev`, hot reload, LLVM
  AOT, FFI execution, parked-recv cancellation, module system,
  REPL/LSP/formatter, package manager, `gzv.std` beyond the frozen
  28, user generics/traits, optimization passes, benchmarks.

## Counts

- `scripts/check.sh`: **green** — fmt clean, clippy 0 warnings,
  **95 passed, 0 failed** (76 at Stage 1 + 19: `blocking` 4, `mn`
  whole-file-new 5, `toto-jit` gate 8, CLI `--workers` 2).
  Breakdown: spikes 39 (ffi 7, spike-1 1+7, spike-1b 6, spike-2 10,
  spike-3 8); compiler 22 (differential 1, interp 8, lexer 7, mir 1,
  conform 1, parser 4); CLI 6 (cli 5, corpus 1); runtime 20
  (blocking 4, concurrent 3, gc 6, mn 5, sequential 1, stack 1);
  jit gate 8.
- Corpus: **121** `.gzv` files, unchanged (concurrent 17 = 11 pos +
  6 neg; diagnostics 11; interp 24; lexing 5; parsing 7; results 13;
  typing 44). Statuses: 47×`0`, 10×`1`, 64×`2`.
- Runtime differential: 39 runnable sequential programs identical
  through the executor; 17 concurrent exact-golden at workers 1 and
  (race-free/membership/legal-outcome) at workers 4 under forced GC;
  64 static negatives asserted. New: 1,530-run M:N stress, zero
  mismatches; committed pair-hammer stress at 1/2/4/8 workers.
- Panic contract: Toto panics surface with spans/traces/exit 1
  through the runtime AND through generated code (7 subprocess
  goldens); no Rust panic escapes; no signal escapes generated code.

## Deviations and open questions

- Deviations: none from the spec or D1–D8. The fixes above (six in
  Stage 1, four race/trace items in Stage 2) are implementation
  corrections toward the spec, not reinterpretations.
- `cli01_tmp.txt` side effect also exists in the CLI corpus runner
  (pre-existing); only the runtime test cleans up after itself.
- Open: debug-mode stack margin (~2.4 KiB) — comfortable in
  release; re-measure under JIT in Phase 4. The `mod-zero` message
  wording is the only known interpreter divergence (cosmetic).
- One transient `mn` failure occurred once during development
  (single test, unidentified, 45+ green runs since across looped
  suite/stress/racy-select hammers); kept on the record, not
  reproduced.
