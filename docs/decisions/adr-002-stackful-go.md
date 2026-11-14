# ADR-002: Stackful `go` blocks, no async/await coloring (D2)

- Status: recommended default from Phase 0 Decision Doc (D1-D8).
- Date: 2026-11-14 (Phase 0, Spike 1).

## Decision

Toto concurrency is stackful `go` blocks: each task owns a real call
stack, blocking calls block only the task, the scheduler switches
contexts. No `async`/`await`, no function coloring. Single-threaded
cooperative scheduler first (Phase 3a), work-stealing M:N second
(Phase 3b). Growable stacks with guard pages; default size set after
measurement, not guessed. Evaluate existing Rust context-switching
crates in the spike before writing assembly.

## Context

D2 constrains D3 (capture/sharing rules) and D4 (panic behavior inside
scopes). A stackful task owns a real stack; a stackless task is a
state machine, which would split the stdlib into two colors.

## Options considered

- **A. Stackful `go` blocks.** No coloring, ordinary blocking code
  works, channels/`select`/scopes read naturally. Costs: context
  switching, stack growth policy, harder debugging.
- **B. Stackless async/await.** Cheaper tasks, easy Rust-futures
  reuse. Costs: coloring infects the stdlib; suspending mid-computation
  for hot reload is much harder. Rejected.

## Consequences

- Phase 3a needs a context-switch implementation, a stack growth
  policy, and a blocking-syscall policy (lean: dedicated blocking
  pool; decided before M:N).
- The Toto internal ABI (D8) must specify the green-thread
  context-switch contract; platform register/stack conventions stay
  Cranelift/LLVM's.
- Spike 1 models tasks as explicit step functions (see the spike
  report: honest shortcut, not precedent). Real stackful switching is
  Phase 3a work; the spike proves the scheduling/root-publication
  contract, not the switch mechanics.

## Revisit trigger

If no suitable context-switching approach works with
Cranelift-generated code and FFI, revisit. Do not mix models in v1:
no `async fn` alongside `go`.

## Spike 1b findings (appended; decision unchanged)

Spike 1b (`docs/spikes/spike-1b-stack-switching.md`) validated the
mechanism this ADR defers: real stackful switching on one OS thread
via **corosensei 0.3.4** (fixed 64 KiB mmap stacks, PROT_NONE guard
page, ~115–145 ns/resume debug). Tasks yield from nested calls with
locals intact; shadow-stack roots survive real switches exactly;
guard-page overflow is a defined SIGSEGV trap (child-process test).
Measured high-water on spike workloads is 2–3 KiB — the 64 KiB
default is sufficient for spikes but stays provisional until
generated code re-measures it. No amendment to the decision; the
revisit trigger stands.
