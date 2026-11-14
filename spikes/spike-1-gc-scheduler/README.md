# Spike 1 prototype: cooperative scheduler + tracing GC (THROWAWAY)

**This is isolated prototype code, not the production runtime.**
It may be deleted once Phase 3 begins. Do not import it from
`crates/*` and do not treat its shortcuts as precedents.

What it proves (see `docs/spikes/spike-1-gc-scheduler.md`):

- Several green-thread tasks spawn, yield, resume, and join on ONE OS
  thread (cooperative, round-robin; no M:N, no work-stealing).
- A non-moving mark-sweep collector with per-task shadow-stack roots.
- Root-stack state preserved across context switches, including nested
  calls (scoped root frames via `Cx::with_frame`).
- One minimal unbuffered channel carrying heap values between tasks.
- A test-only forced-collection hook named `gc_collect_for_test()`.

Deliberate shortcuts (documented, not precedent):

- Tasks are explicit step functions driven by the scheduler, not native
  stacks. The scheduling/root-publication contract is what is under
  test; real stackful switching is Phase 3a work (see the crate
  comparison in the spike report).
- No buffered channels, `select`, cancellation, scopes, timers, or IO.
- `toto dev` drain/reload, FFI, and the checker are all out of scope.
