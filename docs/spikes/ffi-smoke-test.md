# FFI smoke test report (D6, C ABI)

- Prototype: `spikes/ffi-smoke-test/` (isolated, throwaway).
- Base: branch `spike-3/cranelift-toy-ir`; all Spike 1 (8), 1b
  (6), 2 (10), and 3 (8) tests still pass unmodified.
- Date: 2026-11-14. Toolchain: rustc 1.100.0, edition 2024.
- C toolchain: **gcc 15.2.0 (Ubuntu)**, driven by the `cc` crate
  1.11.2 from `build.rs` (`-Wall -Wextra -std=c11`; the C file
  compiles warning-clean, verified directly and in the build).
- Gate: `./scripts/check.sh` (fmt --check, clippy `-D warnings`,
  workspace tests). Panic setting: default `panic = "unwind"`
  (no profile override anywhere in the workspace).

## Falsification verdict: D6 C-ABI-first is VIABLE at smoke scale

One real C file through hand-written `extern "C"` declarations
satisfied every D6 rule at toy scale: correct marshalling,
length-prefixed strings with NUL-copy and UTF-8 validation (invalid
bytes rejected as `Err`, never panicking), rooting for exactly the
call duration with a dying missed-root control, integer-handle
retention with clean post-release failure, a caught panic converted
to an error code, a static callback that only enqueues, and an
error-code + last-error slot. No GC change was needed.
Recommendation: **close Phase 0 and proceed to Phase 1** — no D6
revision, no re-spike. What remains unproven is listed below; the
full end-to-end FFI proof (real library, callbacks with data,
blocking calls, foreign threads) is a Phase 5 exit criterion.

## Pass/fail per acceptance criterion (Section 4)

1. **C call — PASS.**
   `c_add_returns_correct_value` (`spikes/ffi-smoke-test/tests/
   ffi.rs`): `ffi_add` from the real C file (`csrc/
   toto_ffi_test.c`, compiled by `cc` in `build.rs`) returns 42,
   0, and `i32::MAX` for the asserted inputs.
2. **String boundary — PASS.**
   `string_round_trip_validates_and_rejects_invalid_utf8`:
   ASCII and multi-byte strings round-trip byte-exact
   (`greet("héllo🍲 wörld")`); the C file's `{0xFF,0xFE,'x'}`
   comes back as `Err(InvalidUtf8)` — asserted, process alive.
3. **Rooting — PASS.**
   `rooted_object_survives_c_call_missed_root_dies`: rooted `Str`
   read through its pointer by C (`b'h'`), survives forced GC
   (live 1); the same call shape unrooted still reads fine while
   the object is alive (`b'w'`), then dies under forced GC (live
   1 / reclaimed 1, stale handle resolves to `None`). Exact counts.
4. **Handle rule — PASS.**
   `handle_resolves_after_gc_and_fails_cleanly_after_release`:
   retained handle resolves after forced GC (live 1, value 42);
   after release + forced GC the heap object is reclaimed (live 0
   / reclaimed 1) and resolution fails as
   `Err(DeadHandle(h))` — including for a never-issued handle.
   No API path retains a raw pointer: the boundary offers only
   `u32` handles and call-scoped borrows (by construction; see
   boundary shape below).
5. **Panic boundary — PASS.**
   `panic_at_boundary_becomes_error_code`: a REAL Rust panic
   inside `extern "C" fn rust_div` is caught by `catch_unwind`
   and returned as `-100`; the suite keeps running (a panic
   escaping through C would abort the harness); the success path
   still returns 5 afterwards. Crate uses default `panic =
   "unwind"` — recorded; under `panic = "abort"` the catch
   cannot work (documented caveat; production policy is Phase 5).
6. **Callback shape — PASS.**
   `static_callback_enqueues_without_reentry`: C invokes the
   static `extern "C"` callback twice; the drained queue is
   exactly `[(1, "start"), (2, "stop")]`. The callback body
   performs only infallible queue ops and touches no
   scheduler/GC state (enforced by inspection — `on_event`
   references nothing else; the no-re-entry rule is stated in
   code and here).
7. **Error slot — PASS.**
   `error_slot_converts_and_success_path_unaffected`:
   `ffi_fallible(-1)` → `Err(CError { code: -1, message:
   "negative input" })`; success returns doubled values and a
   later success shows no stale error.
8. **Prior spikes green; gate clean — PASS.** 8 + 6 + 10 + 8 + 7
   = 39 tests pass; `cargo fmt --check` and `cargo clippy
   --all-targets -D warnings` pass; the C file builds
   warning-clean under `-Wall -Wextra`.
9. **This report — done.**

## C toolchain and build setup

- Compiler: gcc 15.2.0 on x86_64 Ubuntu (also the `cc` binary);
  selected automatically by the `cc` crate (no `CC` override set).
- `build.rs` compiles `csrc/toto_ffi_test.c` (82 lines, C11) with
  `-Wall -Wextra -std=c11` into static lib `toto_ffi_test`,
  linked via `#[link(name)]`; `rerun-if-changed` on the C file.
  `-Werror` deliberately NOT set: a new compiler's new warnings
  stay visible (and must be justified) instead of failing
  opaquely. Direct `gcc` recompile is warning-clean (recorded).
- Declarations are hand-written `unsafe extern "C"` blocks
  (edition 2024 style) standing in for future Toto
  `unsafe extern` blocks. No binding generator, no `dlopen`.

## Boundary API shape (as implemented, `src/boundary.rs`)

- **Calls:** `add`, `read_first_byte(&[u8]) -> u8`,
  `greet(&str)`, `bad_bytes()`, `fallible_double`,
  `emit_all`/`drain_queue` — all safe wrappers; raw pointers
  exist only as call-scoped borrows inside them.
- **Rooting:** explicit `ShadowStack` push before deriving a
  pointer; borrow (NLL) ends at the call; GC allowed only after.
  No implicit rooting anywhere.
- **Handles:** `HandleTable { table: HashMap<u32, GcRef>,
  roots: ShadowStack }` — `retain` roots, `resolve` re-checks
  heap liveness, `release` unroots. `u32` only; no pointer
  retention possible through this API.
- **Panic boundary:** `catch_unwind` inside `rust_div` and inside
  the static callback; error codes out, never a panic across C.
- **Error slot:** `Result<i32, FfiError::CError{code,message}>`;
  success path never touches the slot.
- **GC helper:** `gc_collect(heap, &[&ShadowStack])` — forced
  collection over exactly the given root sets (test-only).

## Files changed/added and reuse choice

- **Reuse (no fork):** path dependency on `spike-1-gc-scheduler`
  (`Heap`, `GcRef`, `ShadowStack`, `gc_collect_for_test`) — zero
  changes to Spike 1 needed this time. No scheduler/GC/coroutine
  code involved at all.
- **New:** `Cargo.toml` (+ build-dep `cc 1.11.2`), `build.rs`,
  `csrc/toto_ffi_test.c` (82 lines), `src/lib.rs`,
  `src/boundary.rs`, `tests/ffi.rs` (7 tests), `README.md`.
  `Cargo.lock` updated (cc + find-msvc-tools) and committed.
- Root `README.md` layout notes the prototype.

## Deviations, assumptions, open questions

- No deviations from the brief's scope; no new dependencies beyond
  the `cc` build driver.
- The missed-root control calls C while the object is alive, then
  proves death after forced GC — the only sound shape (calling
  with a dead object would be UB, not a test).
- `Mutex` poisoning in the callback queue is dismissed by
  construction (only infallible ops under the lock), documented
  in code.
- Assumes single-threaded callers (all calls are call-scoped;
  foreign threads and blocking calls are Phase 3b/5 work).

## What remains unproven for the Phase 5 FFI proof

Real C library integration (headers, versioning); callbacks
carrying data and lifetimes beyond static fns; strings with
interior NULs and large payloads; error mapping for a full error
taxonomy; blocking C calls and the blocking-pool policy;
foreign-thread entry; structs by value; float marshalling;
`dlopen`; binding generation from `unsafe extern` declarations;
panic policy under `panic = "abort"`; GC root scanning of foreign
stacks (foreign threads). None is needed before Phase 5.

## Provisional defaults (unchanged, awaiting ratification)

Integer-overflow panics, 64 KiB green-thread stacks, 5 s `toto
dev` drain timeout, typed `Result<T,E>` — none exercised by this
smoke test; listed here as provisional, not ratified.

## Recommendation

D6 stands as specified. With Spikes 1–3 and this smoke test all
green on their branches, Phase 0 has answered its questions:
tracing GC with shadow-stack roots (1), real stackful switching
(1b), drain-and-restart mechanics (2), Cranelift codegen against
the GC (3), and C-ABI FFI with rooting/handles/panics (smoke).
Close Phase 0, proceed to Phase 1 (vertical slice:
lexer/parser/interpreter on the proven runtime).
