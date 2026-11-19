# Toto GC: collector, targets, and tuning (Enterprise Wave 0, Stage 2)

## What the collector is (D1, unchanged)

Stop-the-world non-moving mark-sweep over a single heap
(`crates/toto-runtime/src/heap.rs`): type descriptors in object
headers, shadow-stack roots published at safepoints, collection
only at safepoints (allocation points, yield/switch boundaries,
loop back edges — pinned by the Tier-1 `mir_conform` suite).
Single-threaded STW first; M:N coordinates the same stop at
safepoints (Phase 3). No concurrent collection, no
pause-minimization redesign in this wave. A pause IS the
`Heap::collect` call: mark all reachable from the published
roots, sweep the rest; exact reclaimed/live counts are asserted
by the `gc.rs` suite (spike practice).

## The one tuning knob

`RuntimeConfig.gc_threshold_bytes` (default **1 MiB**):
collections trigger when `bytes_since_gc` (approximate payload
bytes, header plus payload) reaches the threshold, checked at
every safepoint. Effects of changing it:

- Lower threshold → more frequent, shorter pauses; more total
  GC CPU (less amortization per stop).
- Higher threshold → rarer, longer pauses (live set grows
  between stops); less total GC CPU until memory pressure
  dominates (cache misses, RSS growth).
- Safe range: anything positive works; below ~64 KiB the gate
  spends more time collecting than running (the `gc.rs` tests
  use tiny thresholds to force frequent collections and stay
  green — correctness never depends on the value). There is
  no other GC tunable; allocation is always on the GC heap,
  there is no nursery/generational mode, and the threshold is
  per-scheduler (M:N workers rendezvous at the same safepoint
  protocol). `toto run`/`dev`/`build` do not expose a CLI flag
  for it — changing the default is a code change with a
  report entry, not a user setting (a flag would be a new
  interface; STOP-and-ask applies).

## Representative service workloads (committed, in `bench/`)

- `gc_alloc.gzv` — sustained allocation pressure: 4 workers ×
  300 iterations of build/sum/drop over 200-int arrays
  (~240k ints ≈ 2 MB churn, ~2 collections at the default
  threshold). Models request handlers that allocate temp
  buffers and drop them.
- `gc_chan.gzv` — channel-heavy stateful service: a long-lived
  registry task owns 64 counters (live state across
  collections) while 3 clients exchange 1500 heap-allocated
  request arrays plus per-request 200-int temp buffers
  (~2.5 MB churn, ~2–3 collections). Models a stateful
  service with cross-task messages and persistent state.
  Addition commutes, so the checksum is identical under every
  interleaving; FIFO request/reply pairing (one outstanding
  request per client) keeps client sums deterministic —
  verified bit-identical across 3 runs.
- `wordfreq` (existing reference): concurrent file scan with
  per-line FFI calls; exercises GC under mixed heap/FFI load.

What makes them representative: allocation pressure covers
churn-then-drop; the service shape covers long-lived state
plus cross-task heap traffic plus temp buffers per request
(the combination that stresses both mark (live state) and
sweep (garbage)); wordfreq covers FFI-adjacent heap use.
Checksums are asserted by `scripts/bench.sh` before timing
(`EXPECT_GC_ALLOC`, `EXPECT_GC_CHAN`); both are executor-only
(the tree-walker cannot run concurrent programs — the
executor is the reference, as recorded since Phase 4).

## Published targets (measured, not asserted)

Machine: Ryzen 5 5600G (12 threads), 27 GB RAM, Linux,
rustc 1.100.0. Method: synthetic heaps with controlled live
sets + garbage, 21 timed `collect()` calls each, median/p99
(`crates/toto-runtime/tests/gc_perf.rs`, `--nocapture`
prints the table; debug gate uses a 30 ms guard bound, the
10 ms budget binds release — see below).

| live set | release med | release p99 | debug med | debug p99 |
|---|---|---|---|---|
| ~1k objects | 25 µs | 32 µs | 99 µs | 129 µs |
| ~10k objects | 289 µs | 313 µs | 1044 µs | 1688 µs |
| ~100k objects | 5.6 ms | 6.8 ms | 14.3 ms | 15.2 ms |

Pause scales ~linearly with live-set size (~0.6 µs/object
release, ~1.4 µs debug): mark-sweep cost is proportional to
live data, as expected — there is no cliff, and garbage
volume does not matter (sweep is free-list work).

- **p99 pause budget: 10 ms at ≤100k live objects
  (production/release).** Measured 6.8 ms (1.5x headroom on
  the reference machine; CI enforces it). Rationale for 10 ms:
  sub-frame for interactive services; the collector meets it
  with margin to spare, so no redesign is needed.
- **Throughput targets: the `exec-gc-*` bench rows.**
  `exec-gc-alloc` 2682 ms, `exec-gc-chan` 3553 ms
  (debug-executor medians, n=3, same machine). These rows run
  in CI with the 5x flagging rule (same rule as Phase 7):
  any median above 5x baseline FAILS the bench job. The rule
  absorbs machine variance while catching order-of-magnitude
  regressions the way the wordfreq 75 s anomaly was caught.

Variance observed: workload medians tight (±1% across 3
runs); pause p99/max ratio ~1.2x at 100k. No single-run
conclusions: budgets bind distributions (21 samples), not
individual stops.

## What is NOT claimed

Cold-cache offline builds are unchanged (Phase 7 story
stands). The 10 ms budget covers STW stop duration only —
M:N rendezvous latency around the stop is not measured here
(it is scheduler coordination, reported with the M:N work,
not collector time). Workloads above ~100k live objects are
outside the published budget: the threshold knob is the
lever (raise it to amortize, at the cost of longer rarer
pauses), and larger live sets need re-measurement on the
target hardware — not extrapolation.
