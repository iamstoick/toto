# Spike 3 report: Cranelift JIT from a toy IR

- Prototype: `spikes/spike-3-cranelift/` (isolated, throwaway).
- Base: branch `spike-2/hot-reload`; all Spike 1 (8), 1b (6), and
  Spike 2 (10) tests still pass unmodified.
- Date: 2026-11-14. Toolchain: rustc 1.100.0, edition 2024.
- Cranelift: **0.137.1** (`jit`, `module`, `frontend`, `codegen`,
  `native` — single version for all five). Tested on Linux x86_64
  only; no other target tested.
- Gate: `./scripts/check.sh` (fmt --check, clippy `-D warnings`,
  workspace tests).

## Falsification verdict: D8 internal ABI SURVIVES at toy scale

Generated code called runtime functions with correct values in both
directions, allocated on the shared GC heap, published roots to the
shadow stack, and survived collections at allocation and explicit
safepoints — including a missed-root control that died exactly as
specified and nested pairs traced through descriptors. The D1+D8
combination (tracing GC + shadow-stack roots + untagged values under
generated code) is NOT falsified.
Recommendation: **proceed to Phase 1** (vertical slice) — no D8
revision, no re-spike. D8 stays a DRAFT: nothing here freezes it
(Phase 2 exit criterion). Caveats: toy heaps, hand-built IR, calls
run on the Rust thread stack (coroutine integration is an explicit
Phase 3 gate item, not built here).

## Pass/fail per acceptance criterion (Section 4)

1. **JIT execution — PASS.**
   `jit_executes_arithmetic_comparison_if_and_loop`
   (`spikes/spike-3-cranelift/tests/jit.rs`): add/sub/mul, `eq`/`lt`
   (1/0), if/else max, counted loop (104 at 5 iters, 0 at 0 iters) —
   all asserted values. Wrapping asserted too (`MAX + 1 == MIN`).
2. **Calls + recursion — PASS.** `jit_supports_calls_and_recursion`:
   `quad(5) == 20` through a nested JIT-to-JIT call;
   `fact(0/5/10) == 1/120/3628800` recursive.
3. **Runtime calls — PASS.**
   `jit_runtime_calls_cross_the_boundary_and_gc_is_exact`: argument
   crosses in (41), handle crosses out through `toto_alloc_int`,
   keeper value crosses back — plus `toto_shadow_push` and
   `toto_gc_collect` exercised in the same test (nine runtime
   entry points total, all declared as imports).
4. **GC integration — PASS.** Same test: 2 objects allocated; the
   in-JIT collection reclaims exactly the garbage (proven: a later
   manual GC finds live 1 / reclaimed 0); dropping the root and
   collecting reclaims the keeper (live 0 / reclaimed 1).
   `jit_missed_root_dies_while_keeper_survives` is the generated-code
   mirror of the Spikes 1/1b control: the unpublished temp reads as
   the default (-1000) while the published keeper reads 42
   (result -958 asserted).
5. **Safepoint in generated code — PASS.**
   `jit_collection_mid_execution_preserves_published_values`: 256-byte
   threshold fires collections from inside JIT execution (20-iteration
   allocation loop); the keeper published before the loop reads back
   intact (7); `gc_runs >= 1`, `reclaimed >= 1`, 21 objects allocated —
   all asserted. Rule as implemented: see below.
6. **Value representation — PASS.**
   `jit_pair_objects_round_trip_and_trace_nested_refs`: JIT builds
   `(inner=(1,2), 3)`, overwrites field 1 with a 99-cell, reads both
   fields back (1 + 99 asserted). Inner pair is reachable ONLY via
   the outer pair (never directly rooted) — the GC traces it through
   the header descriptor (`Mask(0b11)`). Reachable: 5 live / 0
   reclaimed (the overwritten int-3 cell correctly died in-JIT).
   Unreachable (roots dropped): 0 live / 5 reclaimed. Exact counts.
7. **Recompile/replace — PASS.**
   `jit_recompile_replaces_behavior_for_new_calls`: v1 (`×2`)
   finalized and called (42); v2 compiled as a NEW symbol, second
   `finalize_definitions`, registry updated — new calls give 63,
   the old pointer still gives 42. Limits documented honestly:
   `JITModule` has no per-function replace or unload API (only
   whole-module `free_memory`); old machine code stays mapped for
   the module's lifetime; version switching happens in the
   dispatch registry (Spike 2's rule at toy scale). No incremental
   compilation system built.
8. **Trap behavior — PASS (recorded, no recovery).**
   `trap_child_dies` runs `trapprobe3` (JIT `udiv(10, 0)`) in a
   child process: the process dies with **SIGFPE (signal 8, exit
   code 136)** — Cranelift lowers integer division-by-zero to a
   hardware fault. `stderr` shows the pre-call line and no
   `SURVIVED` line: the trap never returns. No handler, no
   recovery attempted (per brief).
9. **Spike 1/1b/2 green; gate clean — PASS.** 7 + 1 + 6 + 10 + 8
   = 32 tests pass; `cargo fmt --check` and `cargo clippy
   --all-targets -D warnings` pass via `scripts/check.sh`.
10. **This report — done.**

## Cranelift versions, platform, compile times

- `cranelift-jit / -module / -frontend / -codegen / -native`:
  **0.137.1**, resolved from `"0.137"` requirements (latest line at
  the time; recorded in `Cargo.lock`). OS/arch tested: **Linux
  x86_64 only**.
- Rough JIT compile times at toy scale (debug build, per function,
  CLIF build + codegen + `finalize_definitions`, printed — never
  asserted): add 947 µs, sub 668 µs, mul 597 µs, eq 564 µs,
  lt 473 µs, max 723 µs, loop 863 µs, wrap 454 µs. Sub-millisecond
  per toy function; NOT benchmark-grade, no conclusions drawn.
- GC pauses during JIT sequences: 0–2 µs on toy heaps (reported in
  `GcReport`s, not asserted).

## Safepoint / root-publication rule for generated code (as implemented)

- Every allocation helper checks the byte threshold BEFORE
  inserting (same rule as Spike 1): live values must already be
  published. `toto_gc_collect` is an explicit safepoint with the
  same precondition.
- Publication = `toto_shadow_push(handle)`; handles live only in
  JIT SSA values across a safepoint are collectable (the native
  stack is never scanned — proven by the missed-root control).
- Root discipline: callees push; callers and the top-level test
  harness reset the shadow afterwards (`truncate(base)`); generated
  functions in this spike emit no prologue/epilogue of their own.
  A production backend will need frame maps or equivalent (Phase 3
  ABI gate).
- Handles cross as `u64` (`(generation << 32) | index`); stale
  handles resolve to `None` at use sites, never to strangers'
  objects. Object bytes are never touched by generated code — all
  heap access goes through the nine runtime entry points.

## Internal-ABI findings (D8 input)

- **Worked:** `ctx: i64` as the first parameter of every JIT
  function and every runtime helper (threading the runtime context
  explicitly through calls); untagged i64 values with handles-as-u64;
  imports via `Linkage::Import` resolved from host function pointers
  (`JITBuilder::symbol`); default calling convention (platform ABI
  is Cranelift's job — nothing custom invented); declare-all-first
  for recursion; repeated `finalize_definitions` for recompile-by-new-symbol.
- **Awkward:** the 0.137 API surface moved since older examples
  (`BlockArg` wrapping for branch args, `finalize` requiring the
  target `TargetFrontendConfig` from the ISA); no built-in
  shadow-stack or safepoint support — all manual via helpers;
  nested CLIF control flow needs hand-built blocks/params/sealing
  (the frontend helps but stays verbose); no per-function unload.
- **D8 must pin down before the Phase 2 freeze:** the exact
  context-passing convention (first-arg like here, or dedicated
  register); the generated-code root-publication protocol (helper
  names/signatures, who resets the shadow); safepoint placement
  rules for generated code; the handle encoding; the trap behavior
  contract (hardware signal vs catchable error); coroutine-stack
  execution (Phase 3 gate); whether field access stays behind
  helpers or becomes inline base+offset (needs the frozen object
  layout first).

## Recompile/unload findings

`JITModule` (0.137.1) offers `declare/define/finalize_definitions/
get_finalized_function` and whole-module `free_memory` — and
nothing between: no redefine, no undefine, no per-function free.
The spike therefore replays Spike 2's rule one level down: the new
version is a new symbol, the dispatch registry switches pointers,
the old pointer stays valid while referenced. Old machine code is
retained for the module's lifetime (documented, not worked around).

## Trap behavior observed

`trapprobe3` JIT-compiles `udiv(arg0, arg1)`, calls it with
`(10, 0)`, and dies: `Floating point exception (core dumped)`,
exit code 136 = SIGFPE. The trap never returns to JIT or host code;
no `SURVIVED` line prints. Recovery is out of scope; the behavior is
recorded here and pinned by `trap_child_dies` (unix-only).

## Files changed/added and reuse choice

- **Reuse (no fork):** `spike-3-cranelift` depends by path on
  `spike-1-gc-scheduler` (`Heap`, `GcRef`, `ShadowStack`,
  descriptors, marker) and on nothing else at runtime. Three
  additive methods were needed on Spike 1 (`GcRef::index/
  generation/from_parts`, `ShadowStack::truncate`,
  `Heap::set_pair_field`) — all behavior-preserving additions for
  generated-code interop; all Spike 1/1b/2 tests still pass
  unmodified. Copying the heap was rejected (divergence risk);
  a shared extracted crate was rejected (overkill for throwaways).
- **New:** `src/toy.rs` (hand-built IR: arith, cmp, if/else, loop,
  calls, recursion, alloc/read/write/GetIntOr/Keep/GcCollect, Let/Var),
  `src/lower.rs` (CLIF lowering, JIT session, `call_ptr`),
  `src/runtime.rs` (nine `extern "C"` entry points + handle codec),
  `src/bin/trapprobe.rs` (trap child), `tests/jit.rs` (8 tests),
  `README.md`, `Cargo.toml`. External deps added:
  `cranelift-{codegen,jit,module,frontend,native} 0.137`
  (+ dev-only `libc` for the signal assertion). `Cargo.lock`
  updated and committed.
- Root `README.md` layout notes the Spike 3 prototype.

## Deviations, assumptions, open questions

- Toy IR is built in Rust test code (no lexer/parser — explicitly
  out of scope); functions take `(ctx, args...)`, fixed arities,
  single i64 return; `Loop` exposes the index but no accumulator
  (sums go through recursion — adequate for the criteria).
- `Call` results are raw values; cross-function rooting is the
  caller's job (reset at the top level). Production codegen needs
  the real protocol (Phase 3 gate).
- Wrapping arithmetic is Cranelift's default (`iadd/isub/imul`);
  checked overflow NOT implemented (provisional default untouched).
- All JIT execution happens on the calling Rust thread's stack.
  Coroutine-stack execution, channel ops from JIT, and debug info
  were not built (explicit Phase 3 / out-of-scope items).
- Division exists ONLY for the trap probe (`Div` = unsigned).
- `unsafe` is confined to FFI boundaries: host function pointers
  registered by address, raw `ctx` round-trip (single-threaded,
  documented), and `call_ptr` transmutation (arity-checked by the
  caller, documented `# Safety`).
- Open for Phase 2/3: freezing the context convention, helper ABI,
  and safepoint rules; inline field access after layout freeze;
  trap-as-error vs trap-as-crash policy; coroutine integration;
  differential testing (Phase 4).

## Provisional defaults (unchanged, awaiting ratification)

- Integer overflow panics on checked arithmetic: NOT exercised —
  this spike uses wrapping toy arithmetic (asserted `MAX+1 == MIN`).
- Stack size 64 KiB: JIT runs on the Rust thread stack here;
  coroutine-stack measurement stays a Phase 3 item.
- `toto dev` drain timeout 5 s: Phase 4, untouched.
- Typed `Result<T,E>`: not exercised, not implemented.

## Recommendation

D8 SURVIVES at toy scale; the D1+D8 combination holds under
generated code. Proceed to Phase 1 (vertical slice: lexer/parser/
interpreter on the proven runtime) — no D8 revision, no re-spike.
D8 remains a DRAFT (freeze is a Phase 2 exit criterion); carry the
"must pin down" list above into the Phase 2 ABI work, and the
coroutine + differential-testing gates into Phases 3–4.
