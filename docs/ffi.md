# FFI — C ABI surface (Phase 5, Stage 3)

D6: C ABI first, declared in `unsafe extern "C"` blocks. The checker
accepts C scalar types (`c_char`, `c_int`, `c_long`, `c_double`),
raw pointers (`*T`), callbacks (`fn(..) -> ..`), `string` as a
return type only, and `unit`. Anything else in an extern signature
is `E0302`; calling a declared-but-unlinked name is `E0301`.
`toto run` and `toto test` execute all FFI; `toto build` lowers
non-callback FFI to `toto_foreign` and links a standalone binary
whose stdout is byte-identical to the interpreter. Only a CALLED
callback site refuses at compile time (exit 3, with the run/test
workaround in the message): unused callback declarations never
poison a build, so importing `gzv.std/ffi` stays buildable.

The canonical wrappers live in `gzv.std/ffi` (`stdlib/ffi.gzv`);
foreign threads live in `gzv.std/ffi_thread`
(`stdlib/ffi_thread.gzv`), which is run/test-only because
`c_thread_spawn` hands a callback to `pthread_create`. Prefer the
wrappers over raw extern declarations. Tests:
`stdlib/tests/test_ffi.gzv`, project corpus `proj11_ffi`,
diagnostics `diag13`–`diag15`, CLI `build_runs_ffi_programs` +
`build_refuses_callback_ffi`.

## Rules (enforced by construction + checks)

- **Rooting covers the call only.** String arguments cross as fresh
  NUL-copies owned by the call frame; the C side must never retain
  them. Interior NUL bytes fail with `E0210` rather than truncating.
- **Retained pointers use no raw escape.** The boundary API offers no
  way to retain a raw pointer: the 8-slot trampoline pool hands C an
  integer-indexed static address, and `pthread`-style out-params are
  read back with `toto_rt_read_i64` before the slot is freed.
- **Callbacks enqueue, never re-enter.** A fired trampoline pushes
  `(slot, arg)` on the runtime queue and returns NULL. The Toto
  callable runs later on the worker that drains the queue — after the
  foreign call completes (or after `pthread_join`). Trampolines hold
  no interpreter, scheduler, GC, or heap handle: only the queue
  mutex. Foreign threads may call nothing else.
- **No panic crosses the boundary.** Rust-side failures convert to
  `E0308`/`E0210`/`E0217` before reaching Toto; `catch_unwind`
  stands at the hand-written boundary (see the abort note below).
- **Strings are UTF-8, length-prefixed.** `strerror`-style returns
  (`-> string`) copy the C message into a Toto string on return:
  invalid UTF-8 is `E0210`, NULL is `E0308`.
- **Errors are values.** `open` reports failure in-band as `-errno`
  (never bare `-1`): `errno` is thread-local while FFI runs on a
  thread pool, so reading `errno` later could see another thread's
  value. Negate back and feed the number to `strerror`, which needs
  no `errno` state. `read` and `toto_rt_close` follow the same
  in-band discipline; `c_read_file` funnels every failure (open,
  mid-read, close) into `Err` with the `errno` text.
- **Whole-file reads compose.** `c_read_file` opens, loops 4 KiB
  `read`s, decodes each chunk with `toto_rt_read_str`, and closes;
  the fd and buffer release on every path, including mid-read I/O
  errors.
- **Blocking calls use the dedicated pool.** Foreign calls suspend
  the task on the blocking pool exactly like file builtins, so a
  `sleep` (or `join`) never stalls other green threads.

## Per-binding contracts

Against real system libraries (libc, libm, libpthread) plus three
runtime helpers. Ownership, encoding, and thread-safety per binding:

| Foreign symbol | Wrapper | Ownership / encoding / threads |
|---|---|---|
| `strlen` | `c_strlen` | arg is a NUL-copy, dies on return; interior NUL is `E0210` |
| `strcmp` | `c_strcmp` | same string rule; sign only, not distance |
| `strtod` | `c_strtod` | end pointer dropped (`None`): trailing garbage ignored; `c_double` bit-exact both directions |
| `pow` (libm) | `c_pow` | pure float computation; overflow follows C (INF), not Toto checked arithmetic |
| `open` | `c_open_read` | `O_RDONLY` only; failure is `-errno` in-band (see above); fd is yours |
| `strerror` | `errno_message` | `-> string` return: NUL-terminated UTF-8 copied on return (`E0210`/`E0308` otherwise); nothing retained |
| `__errno_location` | `errno_value` | reads calling-thread `errno`; meaningful only on the thread that made the call (single-threaded interpreter path) |
| `malloc` | `c_malloc` | uninitialized; every success needs its `c_free`, including error paths |
| `free` | `c_free` | releases `c_malloc` memory; double-free is C UB, as in C |
| `memset` | `c_memzero` | fills `n` bytes with zero; returns the pointer for chaining |
| `memcmp` | `c_memcmp` | 0 when equal; sign only otherwise |
| `sleep` | `c_sleep` | runs on the blocking pool; returns unslept seconds |
| `pthread_create` | `c_thread_spawn` | slot/`pthread_t` detail hidden; `Ok(id)` or `Err` with the error number's text; at most 8 live callbacks (`E0217` when exhausted); callable runs on the joining worker's drain, seeing only plain memory through its arg |
| `pthread_join` | `c_thread_join` | blocks on the pool, then drains the callback: effects visible after return |
| `toto_rt_read_i32` | `errno_value` | aligned `i32` load; the pointer must be valid (runtime-owned) |
| `toto_rt_read_i64` | `c_read_i64` | aligned `i64` load; the pointer must be valid |
| `toto_rt_write_i64` | `c_write_i64` | aligned `i64` store; the pointer must be valid |
| `read` | `c_read` | byte count (0 is EOF) or `-errno` in-band, like `open`; the buffer must hold `n` bytes |
| `toto_rt_close` | `c_close` | `close(2)` alias (`close` itself is unnameable — reserved channel token); `Ok(0)` or `Err` with the `errno` text |
| `toto_rt_read_str` | `c_read_str` | counted bytes to a Toto string, UTF-8 validated (`E0210` on bad bytes, `E0308` on NULL); backs `c_read_file` chunk decoding |
| `toto_rt_net_*` (13 names) | `gzv.std/net` wrappers | Wave 1 sockets (unix-only): fds are opaque `int` handles; bytes cross as `Array<int>` (0–255, `E0308` otherwise); every failure is a `Result` `Err` with the `strerror` text via the per-task call-duration slot; park on the M:N poller (DNS on the blocking pool), never on a worker |
| `toto_rt_crypto_*` (13 names) | `gzv.std/crypto` wrappers | Wave 1 crypto (unix-only): one-shot OpenSSL/LibreSSL calls through `cryptolib` — no retained contexts or key handles (rooting covers the call); bytes cross as `Array<int>`; `verify`-style ops return 1/0 with a clean slot or -1 plus the slot on malformed input; fixed strings (never library queue wording) wherever goldens pin messages, so error text holds on OpenSSL and LibreSSL alike |
| `toto_rt_gzip_*` (3 names) | `gzv.std/compress` wrappers | Wave 1 compression (unix-only gate; backend is portable pure Rust): one-shot gzip over `flate2`/`miniz_oxide`; same byte/error-slot discipline; failures are one fixed string |

Two linked names have no wrapper: `close` cannot be named in an
`unsafe extern` block (`close` is the channel builtin — reserved
token), so file-descriptor lifecycle ends at process exit in
examples until the naming restriction is lifted; `free`'s slot and
`toto_rt_*` helpers are covered above. (`memset` with nonzero fill
and `memcmp` over overlapping regions follow C semantics.)

## Wave 1 allowlist extension (rationale)

The 18-name Phase 5 allowlist grows by 29 names within this
wave's FFI rules (13 net + 13 crypto + 3 gzip), each recorded
above with its per-binding contract. No deferred-FFI work is
smuggled in: no structs-by-value cross (opaque OpenSSL pointers
never leave Rust — Toto sees `int` handles and `Array<int>`
bytes; the `Array<int>` extern shape was authorized 2026-11-19),
no `dlopen` (every symbol is linked at binary build time), no
binding generation (all `unsafe extern` blocks are hand-written).

Link model (unchanged from Phase 5): "static linking" means
statically-declared linkage — every foreign symbol known at
build time, no runtime loading. The system libraries
(libc/libm/libpthread, plus libcrypto here) link the same way:
load-time `DT_NEEDED` entries, never `dlopen`. The
`toto-compiler` build script symlinks the versioned system
libcrypto it finds (`OUT_DIR/libcrypto.so`) so `-lcrypto`
resolves without a dev symlink; the recorded `DT_NEEDED` is
still the SONAME (`libcrypto.so.3`). `TOTO_CRYPTO_LIB_DIR`
overrides the search path. The pure-Rust backends (gzip via
`flate2`) need no native library at all.

## End-to-end proof dispositions (smoke-test unproven list)

1. **Real library — proven.** libc (`strlen`, `strcmp`, `strtod`,
   `open`, `strerror`, `errno`), libm (`pow`), libpthread
   (`create`/`join`), exercised by `test_ffi.gzv` and `proj11_ffi`
   in CI on Linux x86_64.
2. **Data-carrying callbacks — proven.** The pthread entry carries a
   `*mut c_void` through the queue to the drained callable
   (`c_write_i64(arg, 42)` observed after join); trampoline identity
   equals registration identity, so concurrent callbacks cannot
   misroute.
3. **Blocking calls — proven.** `exec_foreign` suspends on the
   dedicated blocking pool; `proj11_ffi` runs `c_sleep` beside a
   parked channel sender/receiver pair with no stall.
4. **Foreign threads — proven by construction.** Trampolines touch
   only the queue mutex. The drained callable runs on a worker, and
   must touch only plain memory through its pointer arg — never
   scheduler/GC state.
5. **Structs-by-value — deferred.** No struct marshalling exists; the
   checker rejects non-C types in extern signatures (`E0302`).
   Consequence: any C API needing struct values is inexpressible in
   v1. Rationale: nothing on the proven surface needs it.
6. **Floats — proven.** `c_double` crosses bit-exact in both
   directions (`pow(2,10) == 1024.0`, `strtod("2.5") == 2.5`).
7. **`dlopen` — deferred.** Static linking against the 18-name
   allowlist only; the checker enforces it (`E0301`). Consequence:
   the runtime plugin ABI stays gated; every foreign symbol must be
   known at binary build time.
8. **Binding generation — deferred.** Hand-written `unsafe extern`
   declarations; no IDL or stub generator in v1 (per ADR-006).
   Revisit if the surface outgrows hand maintenance.
9. **Abort policy — documented limitation.** The boundary converts
   failures under `panic = "unwind"` (the tested configuration). A
   `panic = "abort"` build is untested: any unwind — including the
   `expect` on a poisoned callback-pool mutex — would terminate the
   process instead of converting. No graceful conversion is claimed
   under abort.
10. **Foreign-stack scanning — explicitly unscanned.** Pool and
    pthread stacks are outside shadow-stack discipline by design.
    Consequence: no heap reference may live on a foreign stack
    across a safepoint; trampolines carry only `(slot, address)`
    integers, and string NUL-copies are owned by worker frames, so
    the rule holds by construction.
