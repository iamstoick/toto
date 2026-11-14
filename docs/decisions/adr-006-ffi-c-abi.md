# ADR-006: C-ABI-first FFI (D6)

- Status: recommended default from Phase 0 Decision Doc (D1-D8).
- Date: 2026-11-14 (Phase 0, FFI smoke test).

## Decision

FFI is C ABI first, declared in `unsafe extern` blocks (name TBD).
No IDL, no stub generator in v1; that comes only if FFI proves
painful enough. Production builds may disable FFI entirely
(`--no-ffi`); `toto run` and the interpreter are unaffected.

## Context from the PDF

FFI constrains D3 (crossing the aliasing boundary), D7 (no
unloading while foreign code can reach old code), and D8 (calling
convention belongs to Cranelift; Toto owns only its internal ABI).

## Rules

- **Rooting covers only the duration of a call.** C calls into Toto
  callbacks are not supported in v1. Anything retained after return
  needs an explicit ownership/handle rule; finalizers belong to the
  host.
- **Recursive re-entry** (foreign code calling back into the
  runtime to allocate or trigger GC) is unsupported in v1; crossed
  values must be deeply immutable, moved, or Mutex/Atomic-wrapped.
- **Callbacks** are restricted to static functions marked FFI-safe,
  with no scheduler/GC re-entry except through the runtime queue
  and no thread hops except through the blocking pool.
- **Panic boundaries:** Toto panics must never cross into foreign
  code; convert to error codes/sentinels. Panics inside a foreign
  callback via `catch_unwind` are last-resort only. Test with
  forced panics in both directions.
- **Strings:** Toto strings are UTF-8, length-prefixed, not
  NUL-terminated. FFI conversion copies + NUL-terminates for C and
  validates on input with explicit errors, never silent corruption.
- **Error conversion:** Toto errors at the C boundary become error
  codes or a last-error slot; `Result`-to-error conversion preserves
  the original error as the cause.
- **Testing:** every FFI boundary gets a misuse test (wrong type,
  bad encoding, retained-after-free); pre-commit review for every
  `unsafe extern` block.

## Spike findings (smoke test appended; decision unchanged)

The smoke test (`docs/spikes/ffi-smoke-test.md`, gcc 15.2.0, one
82-line C file, `cc` 1.11.2 build) satisfied every rule at toy
scale: correct marshalling; length-prefixed → NUL-copy strings
with invalid UTF-8 rejected as `Err`; rooting for exactly the call
duration (missed-root control dies under forced GC); integer-handle
retention with clean post-release failure and no pointer-retaining
API; a real Rust panic caught at the boundary into an error code
(default `panic = "unwind"`; `panic = "abort"` caveat recorded);
a static callback that only enqueues (drained after return);
error-code + last-error slot conversion. No GC change needed. No
amendment to the decision; the revisit trigger stands.

## Revisit trigger

If the C-ABI-first approach cannot express a required boundary
(rooting/handle rules unenforceable, panics crossing, silent
reinterpretation), stop and re-examine D6 before Phase 5. The full
end-to-end proof (real library, callbacks with data, blocking
calls, foreign threads) is a Phase 5 exit criterion — the smoke
test does not replace it.
