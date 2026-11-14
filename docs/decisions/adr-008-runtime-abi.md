# ADR-008 (DRAFT): Toto internal ABI (D8) — NOT FROZEN

- Status: **DRAFT.** Spike 3 findings appended. Freeze is a Phase 2
  exit criterion — do NOT treat anything here as frozen.
- Date: 2026-11-14 (Phase 0, Spike 3).

## Decision direction (from D1-D8)

The platform ABI (calling convention, registers, stack) is the
backend's job (Cranelift/LLVM) — Toto MUST NOT invent a custom
platform calling convention. Toto owns only the internal ABI:
untagged native value representation with shadow-stack roots,
runtime entry points, GC-root publication, the green-thread
context-switch contract, and language-level calling rules.

## Context from the PDF

- Value representation: untagged native values; the ABI tracks which
  words are GC references (shadow stacks / pointer maps).
- Stack maps: GC must identify roots on stacks and in registers;
  precise stack maps OR shadow stacks required from the start.
- Epoch tags in object headers: type information and code version.
- Runtime entry points: allocation, yield/resume, channel ops,
  I/O/defer completion — used by generated code, hot reload, and FFI.
- Internal-ABI topics to be decided: green-thread context-switch
  contract, shadow-stack discipline, value representation details,
  calling conventions, stack-trace support.
- Freeze gate (Phase 2 exit): value representation, calling
  convention, stack layout, runtime entry points, unsafe/FFI rules —
  implemented by BOTH backends (interpreter + Cranelift) with
  differential tests. D8 draft now, freeze in Phase 2.

## Spike 3 findings (appended; decision direction unchanged)

Spike 3 (`docs/spikes/spike-3-cranelift.md`, Cranelift 0.137.1,
Linux x86_64) exercised the internal ABI at toy scale:

- `ctx: i64` first parameter on every JIT function and runtime
  helper; untagged i64 with handles as u64 bit patterns; object
  bytes touched only through nine runtime entry points.
- Allocation safepoint (check-before-insert) + explicit
  `toto_gc_collect`; roots published via `toto_shadow_push`;
  native stack never scanned (missed-root control dies as specified).
- Default calling convention throughout; recursion and recompile
  (new symbol + registry dispatch; no per-function unload in
  `JITModule`) work; integer-divide-by-zero dies with SIGFPE.
- Awkward: manual shadow-stack discipline, verbose CLIF control
  flow, 0.137 API churn surface.
- Must pin down before freeze: context-passing convention,
  generated-code root protocol, safepoint placement, handle
  encoding, trap contract, coroutine-stack execution (Phase 3),
  helper-vs-inline field access after layout freeze.

## Revisit trigger

If generated code cannot satisfy the shadow-stack root protocol, or
Cranelift cannot support a required mechanism (runtime calls,
recompilation, traps as specified), stop and re-examine D8 before
Phase 2. Do not freeze an ABI the backends cannot both implement.
