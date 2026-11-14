# Spike 3 prototype: Cranelift JIT from toy IR (THROWAWAY)

**Isolated prototype, not the production runtime.** Answers one
question at toy scale: can GENERATED code call runtime functions,
allocate on the GC heap, publish roots, and survive collections at
safepoints? Hand-built Rust IR structures (no parser), lowered to
CLIF with `cranelift-frontend`, compiled with `cranelift-jit`
0.137.1, executed against the Spike 1 GC heap on the calling Rust
thread's stack.

- GC heap + shadow-stack roots: reused from
  `spike-1-gc-scheduler` via path dependency. Three additive
  methods were needed (`GcRef` field accessors,
  `ShadowStack::truncate`, `Heap::set_pair_field`); nothing forked.
- New here: toy IR (`src/toy.rs`), CLIF lowering + JIT session
  (`src/lower.rs`), runtime entry points for generated code
  (`src/runtime.rs`).
- JIT code runs on the calling thread's stack. Running JIT code
  inside corosensei tasks is a Phase 3 gate item — NOT built here.

See `docs/spikes/spike-3-cranelift.md` for the verdict.
