# ADR-008 (FROZEN): Toto internal ABI (D8)

- Status: **FROZEN** (Phase 2 exit, 2026-11-14). After this freeze, any
  change requires stop-and-ask.
- Supersedes: the DRAFT of 2026-11-14 (Phase 0, Spike 3).

## Decision direction (from D1-D8, unchanged)

The platform ABI (calling convention, registers, stack) is the backend's
job (Cranelift/LLVM) — Toto MUST NOT invent a custom platform calling
convention. Toto owns only the internal ABI: untagged native value
representation with shadow-stack roots, runtime entry points, GC-root
publication, the green-thread context-switch contract, and
language-level calling rules.

## Layers

- **Platform ABI** (Cranelift/LLVM owns): machine calling convention,
  register allocation, stack layout, frame records, unwind tables.
  Generated code uses the backend's default convention everywhere.
- **Toto internal ABI** (this ADR freezes): how values are represented,
  which words are GC references, the runtime entry-point signatures,
  when roots must be published, how green threads switch, and how
  language-level calls pass the runtime context.

## Value representation (frozen)

Untagged native values (D8): `int` is a 64-bit word, `float` IEEE-754
double, `bool` a byte/word, `string`/`Array`/struct/`Range`/
`Result`/`Option`/closure are heap-object references (handles as `u64`
bit patterns, Spike 3 codec: `(generation << 32) | index`; stale handles
resolve to `None` at use sites, never to strangers' objects). Heap
objects carry type-descriptor headers identifying reference fields
(Spike 1 rule, unchanged). Object bytes are touched only through
runtime entry points, never directly by generated code (Spike 3 rule).

## Runtime entry points (frozen initial list)

Starting from Spike 3's nine (`toto_alloc_int`, `toto_alloc_pair`,
`toto_gc_collect`, `toto_get_int_or`, `toto_read_field`,
`toto_shadow_len`, `toto_shadow_pop_to`, `toto_shadow_push`,
`toto_write_field`), the frozen list generalizes the toy allocators and
adds the panic path the explicit-check policy requires:

- Allocation: `toto_alloc_int/float/bool/str/array/struct/range/result/
  option/closure` (one per value family; field/element initialization
  through entry points, not direct writes). Each checks the allocation
  threshold BEFORE inserting (Spike 1 rule).
- Roots: `toto_shadow_push`, `toto_shadow_pop_to`, `toto_shadow_len`.
- Collection: `toto_gc_collect` (explicit safepoint, same publish-before
  precondition as allocation).
- Panic: `toto_panic(code, span, message)` — explicit checks lower to a
  call here, which raises a Toto panic with span and trace per the
  spec §6 contract. **Justification:** Spike 3's SIGFPE path produced no
  span, no trace, and no recovery; a panic entry point is what makes the
  explicit-check policy implementable.
- Reserved for Phase 3 (names frozen, signatures not): `toto_yield`,
  `toto_resume`, `toto_spawn`, `toto_chan_make/send/recv` (green-thread
  and channel contracts from Spikes 1b/2; signatures are Phase 3
  design). No other entry points may be added without stop-and-ask.

## GC-root publication (frozen)

- Shadow-stack discipline (Spikes 1/1b/3): every heap reference live
  across a safepoint is published to the shadow stack BEFORE the
  safepoint. The native stack is never scanned — a value held only in a
  register or native-stack slot across a safepoint is collectable (the
  missed-root controls in Spikes 1/1b/3 prove this).
- At a context switch the task's shadow-stack pointer is saved/restored
  as part of task state (Spike 1b rule).
- Codegen MUST auto-generate root publication (Spike 3 finding): the
  backend, not the language user, emits push/pop around safepoints.
  MIR marks every safepoint explicitly (`safepoint entry/loop/call/
  alloc`) so this lowering is mechanical; Tier-1 conformance pins the
  markers structurally.

## Context-switch contract (frozen, from Spike 1b)

- One OS thread, cooperative switching; each task runs on its own
  64 KiB fixed stack with guard pages (ratified).
- A switch saves/restores the stack pointer and callee-saved state;
  yielding is legal from nested calls; the publish-before-switch rule
  applies (no heap reference held only in a register across a switch).
- Coroutine-stack execution of JIT code is a Phase 3 gate item (Spike 3
  ran JIT code on the calling thread's stack only).

## Language-level calling rules (frozen)

- `ctx` first: every generated function and every runtime helper takes
  the runtime context as its first parameter (Spike 3 rule, kept —
  threading context explicitly survived at toy scale with no awkwardness
  beyond verbose CLIF, which is a tooling property, not an ABI defect).
- Calls evaluate callee then arguments left to right (spec §5.1); arity
  is checked statically, so generated call sequences need no
  runtime arity check on the fast path.

## Safepoint rule (frozen)

Collection happens ONLY at safepoints: function entry, loop headers,
calls, and allocations. No heap reference may be held only in a register
(or native-stack slot) across a safepoint. MIR makes this checkable:
`safepoint` markers plus `divcheck` before every integer division.

## Trap policy decision (REQUIRED, decided here)

**Explicit checks in codegen. Signal-based trap handling is rejected
for v1.**

Rationale:

1. Spike 3 evidence: integer division by zero in JIT code kills the
   process with SIGFPE (exit 136) — no span, no trace, no recovery.
   Recovering the spec §6 panic contract (span + trace + exit 1) from a
   signal would require platform-specific unwinding that Cranelift does
   not provide.
2. Portability: signal semantics differ per OS/arch (Windows SEH vs
   POSIX signals, WASM traps); explicit branches behave identically
   everywhere, which differential testing (Phase 4) requires.
3. Cost: checks are predictable branches on cold paths; the v1 backends
   do not need trap-table or deoptimization machinery.
4. MIR reflects the decision: integer `/`/`%` lower to an explicit
   `divcheck` op (E0202) followed by a total `div`/`mod`; Tier-1
   conformance requires the guard structurally. Overflow keeps the
   checked ops (E0201); out-of-bounds keeps explicit checks (E0209).

Reopen only if: measured check overhead on real programs justifies
re-examination AND a portable recovery scheme exists. That is a
stop-and-ask change.

## ABI conformance test list

**Tier 1 — structural, no Cranelift (implemented, Phase 2):**
`crates/toto-compiler/tests/mir_conform.rs` over the MIR dump cases
plus every positive corpus program: divcheck-before-int-div/mod,
safepoint-call before calls, safepoint-loop on loop headers (via
dominator-based back-edge detection), safepoint-alloc before
allocations, safepoint-entry at function entry.

**Tier 2 — representation-level, Cranelift required (specified now,
executed in Phase 3):**

1. Value layout: every value family has the frozen untagged size/
   encoding; handles decode as generation|index; stale handles fail
   cleanly.
2. Root publication at runtime: JIT code holding live heap values
   across a forced collection preserves them; the missed-root control
   (unpublished value dies) passes in generated code; shadow depth is
   correct across calls in both directions.
3. Entry-point signatures: every call from generated code passes `ctx`
   first with the frozen signature; allocation threshold checked
   before insert; `toto_panic` produces a spec-§6 panic (span, trace,
   exit 1) for div-by-zero, overflow, and out-of-bounds.
4. Context-switch contract: task switch on coroutine stacks preserves
   the shadow-stack pointer; yield from nested JIT calls resumes with
   locals intact.
5. Trap behavior: no signal escapes generated code — every former
   SIGFPE-class event is a Toto panic; process never exits 136.

## Revisit trigger

If Cranelift cannot implement any frozen rule (entry-point signatures,
root publication, explicit checks, coroutine stacks), STOP and
re-examine D8 before Phase 3. Do not silently reinterpret this ADR.

## Phase 2 changes from the draft

- Status DRAFT → FROZEN; Spike 3 "must pin down" list resolved above.
- Entry points generalized from toy (`alloc_int/pair`) to per-family
  allocators; `toto_panic` added (required by the trap decision);
  scheduler/channel names reserved with Phase 3 signatures.
- Trap policy decided: explicit checks (this was the open decision).
- Conformance split into Tier 1 (implemented) and Tier 2 (Phase 3).
