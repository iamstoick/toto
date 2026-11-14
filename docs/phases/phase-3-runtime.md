# Phase 3 — Runtime + Concurrency: Stage 1 (3a) report

Status: **Stage 1 COMPLETE and green. Stage 2 (M:N) and Stage 3 (ABI
gates) NOT started — carried forward with decisions recorded below.**

## Verdict

The single-threaded production runtime meets the Stage 1 end-to-end
milestone: green threads + channels + GC-heap allocation + forced
collection, deterministic, in CI. Six executor/corpus bugs were found
and fixed during hardening (none in the spec or frozen D8). M:N,
the blocking pool, the Cranelift Tier-2 gate, and JIT-on-coroutine-
stacks remain Phase 3 Stage 2/3 work.

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
pool isolates the damage with a separately tunable size. NOT yet
implemented — Stage 2 implements it with the M:N scheduler and its
test (blocking call must not stall other green threads).

## Stage 2/3 carry-forwards (not started)

- M:N work-stealing scheduler across OS threads; STW GC coordinated
  at safepoints (no concurrent collection, D1); concurrent corpus
  green under M:N; stress tests with invariant assertions.
- Blocking pool implementation + test (decision above).
- Tier-2 ABI conformance executed via real Cranelift-compiled MIR
  (value layout, root publication, entry points, switch contract).
- JIT-on-coroutine-stacks proof (Spike 3 toy + Spike 1b stacks
  acceptable).
- Fiber-aware race detector stays Phase 4 TOOLING. No `toto dev`,
  LLVM, FFI execution, parked-recv cancellation, modules, REPL,
  formatter, package manager, or stdlib growth (all out of scope).

## Counts

- `scripts/check.sh`: **green** — fmt clean, clippy 0 warnings,
  **76 passed, 0 failed** (incl. all spike suites).
- Corpus: **121** `.gzv` files (concurrent 17 = 11 pos + 6 neg;
  diagnostics 11; interp 24; lexing 5; parsing 7; results 13;
  typing 44). Statuses: 47×`0`, 10×`1`, 64×`2`.
- Runtime differential: 39 runnable sequential programs identical
  through the executor (36 status-0 + 3 status-1); 17 concurrent
  exact-golden under forced GC; 64 static negatives asserted.
- Typing negatives: 8 new D3/concurrency goldens; MIR dumps:
  `dump_chan`, `dump_select`.
- Panic contract: Toto panics surface with spans/traces/exit 1
  through the runtime; no Rust panic escapes.

## Deviations and open questions

- Deviations: none from the spec or D1–D8. The six fixes above are
  implementation corrections toward the spec, not reinterpretations.
- `cli01_tmp.txt` side effect also exists in the CLI corpus runner
  (pre-existing); only the runtime test cleans up after itself.
- Open: debug-mode stack margin (~2.4 KiB) — comfortable in
  release; re-measure under JIT in Phase 4. Stage 2/3 designs are
  recorded above but unimplemented.
