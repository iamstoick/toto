# ADR-001: Tracing GC, non-moving mark-sweep, shadow-stack roots (D1)

- Status: recommended default from Phase 0 Decision Doc (D1-D8); this
  spike is the falsification test. Survives unless Spike 1 says otherwise.
- Date: 2026-11-14 (Phase 0, Spike 1).

## Decision

Toto v1 uses a tracing garbage collector: non-moving mark-sweep with
segregated free lists, shadow-stack roots, stop-the-world collection,
single-threaded first. No finalizers; resources via `defer`/`close`.
No concurrent collection, no pause minimization, no moving GC, no
hybrid memory model in v1.

Terminology (v2.2): say **rooting** (keeping an object discoverable as
live), not "pinning". Pinning is about preventing movement and does
not apply to the non-moving v1 collector.

## Context

D1 constrains D3 (aliasing), D6 (FFI rooting), D7 (heap traversal,
old-code retention), and D8 (object layout, root representation).
Who frees memory, and when, is the constraint everything else builds on.

## Options considered

- **A. Tracing GC.** Simplest user model, handles cycles, fits green
  threads/channels, heap traversable for hot reload. Costs: pauses,
  root scanning in generated code, runtime integration, FFI rooting.
- **B. ARC with weak references.** Predictable reclamation, simpler
  codegen. Costs: cycles leak without a cycle collector, atomic
  refcount cost, still needs aliasing rules. Documented fallback.
- **C. Rust-style ownership/borrowing.** No pauses, no collector.
  Costs: by far the hardest checker to build; fights channels and hot
  reload; would dominate v1. Eliminated on paper, not prototyped.
- **D. Hybrid.** Rejected for v1: implementation and maintenance cost.

## Consequences

- Compiler (Phase 2/3) must maintain an explicit per-task root stack,
  publish all live heap references at safepoints, and describe
  reference fields via object headers (or pointer maps).
- FFI (D6) must root values for call duration plus a retained-pointer
  rule; blocking calls go to a pool.
- Hot reload (D7) can traverse the heap but must handle code versions
  separately (GC reachability alone never unloads code).

## Spike 1 findings (appended after the spike)

- The prototype collected exactly: forced collections reclaimed only
  unreachable objects across 7 deterministic tests; live objects in
  nested frames, across task switches, in flight in the channel, and
  in receiver inboxes all survived.
- Safepoint protocol held: allocations and step boundaries only, with
  the "publish before safepoint" rule enforced by construction and a
  dedicated test proving an unpublished handle is reclaimed.
- Observed pauses are microseconds on toy heaps (6-8 us for 60 live
  objects, debug build); not benchmark-grade, but nothing suggests
  the default is unworkable at this scale.
- Verdict: **D1 SURVIVES this spike.** See
  `docs/spikes/spike-1-gc-scheduler.md`.

## Revisit trigger

If generated-code root scanning or pause times prove unworkable in
Phase 3 (real codegen, real heaps), fall back to ARC with weak
references and documented cycle leaks. Do not drift into
ownership/borrowing without a separate RFC.

## Safepoint-protocol note from Spike 1b (appended; decision unchanged)

Spike 1b extended the protocol to allocation + switch/yield
boundary (`docs/spikes/spike-1b-stack-switching.md`). The native
stack is never scanned; each task's shadow stack lives in
scheduler-owned memory so no shadow-pointer save/restore crosses a
switch. `send` enforces publish-before-switch with a runtime assert
at the suspend site; the yield side rests on construction plus a
missed-root control test. Forced GCs while tasks are suspended
mid-computation stay exact. No change to the collector design.
