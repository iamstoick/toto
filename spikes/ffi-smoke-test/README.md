# FFI smoke test (THROWAWAY)

**Isolated prototype, not the production runtime.** One real C file
through hand-written `extern "C"` declarations, proving the D6
rules at toy scale: calls, strings, rooting, handles, panic
boundary, static callback, error slot.

- GC heap + roots: reused from `spike-1-gc-scheduler` via path
  dependency. Zero changes to Spike 1 needed.
- New here: `build.rs` (`cc` build), `csrc/toto_ffi_test.c`,
  `src/boundary.rs` (safe wrappers, handle table, panic/callback
  guards), `tests/ffi.rs`.
- Panic setting: default `panic = "unwind"` (required for
  `catch_unwind`; see the report for the `panic = "abort"` caveat).

See `docs/spikes/ffi-smoke-test.md` for the verdict.
