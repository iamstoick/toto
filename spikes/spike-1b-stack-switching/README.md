# Spike 1b prototype: real stack switching (THROWAWAY)

**Isolated prototype, not the production runtime.** Closes the one gap
Spike 1 left open: tasks run on their own native stacks via
`corosensei`, switching stack pointer and callee-saved state, instead
of step functions. Everything else (GC, shadow stacks, channel
semantics) is shared with Spike 1, not forked.

- GC + roots: reused from `spike-1-gc-scheduler` via path dependency
  (`heap`, `roots`). The only change asked of Spike 1 was widening
  `ShadowStack::open/close/frames_len` to `pub`.
- Scheduler + task context + stack measurement: new here, coupled to
  `corosensei` coroutines (`src/sched.rs`, `src/task.rs`).
- Safepoint protocol v2: allocation + switch/yield boundary; see
  `src/lib.rs` and `docs/spikes/spike-1b-stack-switching.md`.

What this proves: yield from nested calls on real stacks, root
publication across real switches, in-flight channel values surviving
forced GC, per-task high-water measurement, guard-page overflow as a
defined trap, and rough switch cost.
