# Enterprise Wave 2 report — services, HTTP, logging, metrics

Fix-forward correction (2026-11-22): wave verdict is NOT YET
ACCEPTABLE — STOP. CI run 44886709131 (head `dac9c6a`)
failed on two non-Windows jobs (`check`, macOS probe);
Windows is NOT the only failure, so the green gate is
violated. No merge. Details in the dated addendum at the
end; earlier section text is preserved verbatim.

Status: STAGES 1–5 COMPLETE (final-verifier `check.sh` scope
run in Stage 5, §5.4). Earlier section text is preserved
verbatim.

Branch: `enterprise/wave-2-services-http-logging-metrics`, from
`main` at `bcc0824`. This report is appended stage by stage (§6
in the Wave 1 style); the wave exit criteria are answered at the
end.

Hard constraints carried (violation = STOP, report blocked,
commit nothing noncompliant): D1–D8 FROZEN; 27 builtins frozen
(TLS adds zero builtins — new `gzv.std/tls` module only);
Phase 5 module model frozen (path imports, flat namespace, no
mod/use, `pub` core unenforced, `gzv.std/` prefix); ADR-009 +
spec §17 APPROVED as written 2026-11-21 (implemented exactly —
§17/§18 untouched, no timeout parameters, no new socket
semantics); static linking only (statically-declared linkage,
load-time `DT_NEEDED`, never `dlopen`); structs-by-value,
`dlopen`, and binding generation stay deferred; no TLS/crypto
implementation of Toto's own, no home-grown primitives, modes,
or cert logic; HTTP/1.1 only (HTTP/2, WebSockets, raw sockets,
and the full bytes story are separate gated proposals, NOT this
wave); no Waves 3–5 work (DB, OAuth/OIDC/JWT, config/secrets,
tracing, profiling, debugger, graceful shutdown, static
binaries/containers, package hardening, embedding, cloud SDKs,
plugin ABI); Windows stays DEFERRED (supported: Linux x86_64,
macOS ARM64, Linux ARM64); no real release/tag/artifact; never
force-push; stage named files only (`git add`, never `-A`);
never merge to `main`; spec silent on semantics => STOP.

## Stage 1 — TLS via the system libssl (client + server)

### Library choice + rationale (name/version/license/D6-fit)

The system libssl — the TLS face of the same mature library
Wave 1 chose for crypto: OpenSSL 3 on Linux (`libssl.so.3`;
observed OpenSSL 3.5.5 on the implementing machine, link-time
`DT_NEEDED libssl.so.3`, version runtime-detected via
`crypto_openssl_version()` and never pinned in goldens),
LibreSSL on macOS. Licenses: Apache License 2.0 (OpenSSL 3.x);
LibreSSL retains the original OpenSSL/SSLeay dual-license
terms (plus ISC for newer parts) — recorded in `docs/ffi.md`.

Rationale: libssl is the only candidate that covers both
directions (client + server) with an opaque-pointer-only API —
no caller-allocated structs, so the no-structs-by-value rule
holds by construction — under the same load-time link model as
libc/libm/libcrypto. Only long-stable APIs are used
(`TLS_client_method`/`TLS_server_method`, `SSL_CTX_new/free`,
`SSL_new/free`, `SSL_set_fd`, `SSL_connect/accept/read/write/`
`shutdown/get_error/get_verify_result`,
`SSL_CTX_use_certificate`/`use_PrivateKey`/`check_private_key`/
`get_cert_store` — pointers, ints, and one long, in every
OpenSSL 1.1.1+/3.x and LibreSSL release).
`SSL_set_tlsext_host_name` is deliberately NOT used: upstream
it is a C macro, not an exported symbol, so it cannot be
linked — Stage 1 sends no SNI (loopback only).

D6/FFI fit (see `docs/ffi.md`, "Wave 2 Stage 1 allowlist
extension"): Toto calls 6 hand-written `toto_rt_tls_*` foreign
functions (allowlist +6 with per-binding contracts; the
`Array<int>` extern shape is the one authorized 2026-11-19).
The Rust bridge (`tlslib`) holds all opaque pointers itself —
Toto sees only `int` handles and `Array<int>` bytes. Retained
sessions ride the explicit handle rule: the `SSL*` is keyed by
its fd in a process-global table, Toto names only the fd,
`tls_close` (or any `net_close` of the same fd) frees the
session, and a second close names the missing session loudly
instead of touching a recycled fd number. No `dlopen` (every
symbol linked at binary build time), no binding generation, no
callbacks (the verify callback slot is NULL — the library
enforces with its default behavior, no re-entry), no retained
Toto state. One declaration site per C symbol: libcrypto
declarations live in `cryptolib` (`pub(crate)` sharing —
redeclaring them tripped `clashing_extern_declarations`, fixed
by sharing, not by allowing).

STOP flag (d) discharge: this machine carries no libssl
headers (`libssl-dev` absent), so every planned symbol was
confirmed in the installed dynamic symbol table instead (`nm
-D libssl.so.3`: `T` for all 17 libssl names used;
`PEM_read_bio_PrivateKey`/`EVP_PKEY_free` are `T` in
libcrypto.so.3, already linked). The opaque-pointer-only claim
rests on that table plus OpenSSL API knowledge and the Wave 1
precedent.

### Design (no spec change — §17/§18 implemented exactly)

- Upgrade model, not a new socket layer: TLS upgrades an
  already-connected TCP fd in place (client: `net_tcp_connect`
  then `tls_client_connect(conn, ca_pem)`; server:
  `net_tcp_listen`/`net_accept` then
  `tls_server_accept(conn, cert_pem, key_pem)`). The Wave 1
  loopback rule carries over unchanged (no address parameter
  anywhere); no new builtins, no module-model change.
- Trust model (Wave 1 reuse): the client verifies the server
  chain against the given CA PEM with the same `X509_STORE`
  semantics as `crypto_x509_verify`, enforced by the library at
  handshake time. No hostname check (as in Wave 1), no client
  certificates, no session resumption, no ALPN — one-way
  loopback TLS only. Recorded follow-ups, not silent additions.
- Blocking semantics (§17.5 rule — "a wait is poller-eligible
  iff its fd can be nonblocking and its readiness is
  `poll()`able; everything else goes to the pool"): an SSL step
  is not a readiness wait, so handshakes and record I/O run as
  blocking calls on the interpreter and on the dedicated
  blocking pool on the executor (new `BlockingOp::Tls` +
  `TlsOp`, `TlsInt`/`TlsBytes`/`TlsFail` completions, separate
  per-task `tls_error` slot so interleaved net/TLS calls never
  clobber each other) — never inline on a worker, never on the
  poller. TLS fds are forced blocking for the session lifetime
  (`netsys::set_blocking`, the mirror of `set_nonblocking`).
  `WANT_READ`/`WANT_WRITE` cannot fire on a blocking fd; the
  retry loop through `poll_wait` is defense only.
- Teardown sends `close_notify` (best-effort loop) so the peer
  sees orderly shutdown (its `recv` returns empty, not an
  error); `net_close` of a TLS fd frees the session first (no
  leaks, no stale entries for recycled fd numbers).
- JIT/AOT refuse the surface with the workaround (run with
  `toto run`/`toto test`); Windows gets check-time `E0301` like
  the net surface.
- Error discipline (mirrors §17.3/§18.2): `Result<_, string>`
  everywhere; fixed strings wherever goldens pin messages (never
  library queue wording, so text holds on OpenSSL and LibreSSL
  alike).

### Tests + evidence (all passing at Stage 1 tip)

- `tests/corpus/project/proj15_tls_echo/` (new): Toto server +
  Toto client loopback echo through the real `gzv.std/tls` +
  `gzv.std/net` wrappers, plus three deterministic error paths
  (close/send on a non-TLS fd, `max < 1`) — all with exact
  goldens. Runs in the CLI corpus gate; also green 3/3 under
  `toto run --workers 4`.
- Fixtures (`proj15_tls_echo/fixtures/`, pinned, committed):
  `ca.pem` (test CA, `CN=toto-test-ca`), `server-cert.pem`
  (`CN=localhost`, SAN `DNS:localhost,IP:127.0.0.1`, signed by
  the CA, 2026–2056), `server-key.pem`, `other-ca.pem`
  (untrusted root for the negative). Verified with
  `openssl verify` at generation (OK against `ca.pem`,
  fails against `other-ca.pem`).
- Interop BOTH directions against the reference
  (`openssl s_server`/`s_client`), no library wording pinned:
  - `crates/toto-compiler/tests/tls_interop.rs` (3 tests,
    interpreter path): client↔`s_server` (client reads the
    server's bytes AND the server capture holds the client's),
    server↔`s_client` (chain verified with
    `-verify_return_error`, echo both ways), wrong-CA client
    fails the handshake (program marker, exit 0).
  - `crates/toto-runtime/tests/tls_openssl.rs` (3 tests,
    executor pool path): same three scenarios through
    `run_checked`.
  - Mechanics recorded in the test headers: peers' stdin is a
    held-open pipe (both openssl ends close on stdin EOF, which
    wins the relay race); captures are pipes drained by reader
    threads (a bare-TCP readiness probe consumes
    `s_server -naccept 1`'s single slot — verified empirically —
    so readiness is client-retry on refused / a ready file from
    the server, never a probe); watchdogs turn stalls into loud
    failures.
- Package gates at the Stage 1 tip: `cargo fmt --check` clean;
  `cargo clippy --workspace --all-targets -- -D warnings`
  clean; `cargo test -p toto-compiler` green (incl. updated
  `stdlib_table_is_complete` 15→16 and the `diag13` golden with
  the 6 new linked names); `cargo test -p toto-runtime` green
  (incl. the 3 new interop tests); `cargo test -p toto-cli`
  green (corpus gate covers `proj15` + `diag13`);
  `cargo test -p toto-jit` green (refuse arm verified by hand:
  `jit_probe` on a TLS extern refuses with the pool message);
  stdlib `toto test` 13/13. Full `check.sh` left to the final
  verifier per the brief.

### Follow-ups (recorded, not done)

Hostname verification, SNI/ALPN, session resumption, client
certificates, non-loopback bind (service waves), TLS rows in
the bench suite (one-shots and handshakes do not fit the
calibrated-workload methodology), macOS ARM64 leg of the new
tests (same stable symbols, but unrun here).

---

## Stage 2 — HTTP/1.1 framework (routing, middleware, handlers, client)

Status: STAGE 2 COMPLETE (pending final-verifier `check.sh`).
Earlier section text is preserved verbatim.

### API design + rationale (flagged for user review)

The framework surface is `gzv.std/http` (one module; the
`http_server`/`http_client` split was considered and rejected —
one import covers both sides, and the namespace is flat anyway),
implemented EXACTLY per the accepted HTTP design with two
documented v1 deviations below. New `Http*` structs over existing
types only (`int`, `string`, `Array<int>`, `Result<_, string>`,
user structs, channels, `go`): `HttpRequest` (method, target,
version, headers, body, peer, params), `HttpResponse`, `HttpHeader`,
`HttpParam`, `HttpRoute`, `HttpRouter` (routes + middlewares),
`HttpServeOpts` (`expose_errors`, `max_body`, `keepalive_ms`,
`server_name`), `HttpClientOpts` (`max_body`, `redirects`,
`host_allow`), `HttpClientReq` (method, url, headers, body,
`ca_pem`), `HttpClientResp`. Bodies stay `Array<int>` end to end
(binary-safe; the §17.1 carve-out); framing text is ASCII-only by
construction. URL/query parsing is pure-Toto (`parse_query`,
`decode_pct`); NO new builtin.

Routing: `route_add` validates (empty method, non-`/` pattern,
malformed `{capture}` rejected loudly); match order is
exact > template > wildcard with first-registration tiebreak
(deterministic, golden-tested); `"*"` matches all methods.
Middleware composes to an onion at dispatch (closures capture
`(mw, next)` by value — first-registered is outermost; proven by
the order vector). Path params arrive RAW (still-encoded) in
`req.params` with `http_param`/`http_decode_pct` for use-site
decoding — lazy validation at use is the honest rule given the
`Err`-as-values wall (see deviations).

Server: `http_serve` binds loopback + spawns the accept loop on
its own green thread, returning the listener fd
(`net_close(listener)` stops the loop — a parked accept wakes
with `Err`, proven); each connection gets one task running the
keep-alive loop (`Connection: close`/parse-failure/EOF closes;
one exchange never starves the next — pipelining is golden).
`http_serve_tls` is the TLS twin (per-connection server upgrade
with the pinned fixtures). Client: one-shot exchanges over the
same substrate (Host always set with explicit port;
`Connection: close`), upgrading via `tls_client_connect` for
`https://` (empty `ca_pem` is a loud pre-connect error).
Redirects: `0` never follows; otherwise relative or absolute
same-scheme/host/port only, fixed 301/302/303/307/308 method
rules, budget overrun is `Err("http: too many redirects")`.
`host_allow` narrows the loopback set before connecting.

GREEN-THREAD BOUNDARY: every network wait (accept/recv/send/
connect-handshake) parks on `ToSched::Net` with the fd
nonblocking; TLS handshakes/record I/O run on the dedicated
blocking pool (interpreter: blocking calls); DNS via the pool
inside `net_tcp_connect`. One connection = one green thread; N
concurrent clients never hold N workers (4×10 hammer green at 1
and 4 workers). No timeout params anywhere (§17.5 stands);
`keepalive_ms` is recorded but advisory (no clock in v1).

### Two v1 deviations (both flagged, neither silent)

1. Handler/middleware `Err` → connection abort (EOF), NOT the
designed 500 mapping; `expose_errors` is accepted but RESERVED
(no effect — the secure default holds either way). REASON: v1
Toto has no `Err` destructuring (`match` is Phase 2; `?` only
propagates), so a framework can never turn an unknown `Err`
payload into a response body or a log line. Handlers that must
answer 500 with detail return `Ok(http_text(500, ...))`
explicitly (proven in corpus: `500:explicit-500`). Full design
behavior needs Phase-2 destructuring or a gated new builtin —
recorded as a follow-up, NOT smuggled in.
2. Returning a `Result` whose payload transitively holds function
values, then unwrapping with `?` in the caller, panics E0204
("function from builtin") at runtime — found while structuring
the corpus driver (minimal repro in the work notes). Bare-struct
returns, param-direction passing, `use_mw` registration, and
comparing wrapped Results WITHOUT unwrapping are all fine; the
framework itself never unwraps a fn-carrying `Result` (all its
`Result` payloads are `int`/`string`/plain-data structs), and
the corpus builder takes the router as a parameter. Report filed
as a runtime edge for the owning wave; the framework and its
tests are structured around it with comments at each site.

### Implementation (pure Toto, no new FFI, zero new builtins)

- `stdlib/http.gzv` (~2,100 lines): ASCII codec (allocation-free
comparison chains — the first table-scan revision stalled past
~3 KB bodies), router, onion dispatch, request/response framing
(strict CRLF, token validation, single-CL/single-chunked
accounting, 16 KiB head cap, `max_body` enforcement), server
loops (plain + TLS-injected IO triple), client (URL parse,
host policy, redirects, three body disciplines).
- `crates/toto-compiler/src/stdlib.rs`: register `http` (17
modules); `project.rs` count 16→17. No compiler/runtime
changes — the framework is a stdlib citizen.
- `docs/stdlib.md`: `## http` section (surface table, fixed
wire mapping, limits, test pointers).

### Tests + evidence (all passing at the Stage 2 tip)

- `tests/corpus/project/proj16_http/` (new, pinned fixtures
copied from `proj15`): 5 golden lines covering lifecycle,
routing precedence (template-first registration still loses to
exact, wildcard, `*` methods), middleware order vector +
short-circuit + passthrough, query/pct, binary echo, redirect
follow/no-follow/cross-host-as-is/budget, 404/405+Allow/
explicit-500/HEAD, policy errors (scheme/host-allow/CA/caps),
server-side cap 400, refused-port negative, keep-alive
pipelining, 400/505/abort-then-survival, chunked upload,
client-side truncation via raw responder, HTTPS both directions
+ wrong-CA + post-failure survival. Exact goldens; green 3/3
plus `--workers 4` (byte-identical).
- `stdlib/tests/test_http.gzv` (new): 40+ pure-unit assertions
(codec incl. non-text rejection, pct/query, headers/params,
route validation, constructors, every pre-connect client
error). stdlib `toto test` 14/14.
- `crates/toto-cli/tests/http_interop.rs` (new, 4 tests):
our server vs curl (bodies, keep-alive reuse, echo, 404),
our TLS server vs curl `--cacert` (chain + hostname verified
by curl; wrong-CA curl fails), our client vs a curl-checked
Rust reference (GET 200 + POST 501 passthrough), our HTTPS
client vs `openssl s_server` (canned response). All green.
- Package gates at the Stage 2 tip: `cargo fmt --check`
clean; `cargo clippy --workspace --all-targets -- -D warnings`
clean; `cargo test` green for toto-compiler (incl. 17-module
table), toto-runtime, toto-cli (corpus incl. `proj16` +
`http_interop`), toto-jit; stdlib `toto test` 14/14. Full
`check.sh` left to the final verifier per the brief.

### Follow-ups (recorded, not done)

Destructuring-dependent 500 mapping + `expose_errors`
semantics (needs Phase 2 or gated builtin); `?`-unwrap of
fn-carrying `Result`s (runtime edge, repro on file);
hostname verification/SNI/ALPN/resumption (TLS waves);
non-loopback bind + `Host`-based virtual hosting (service
waves); `getpeername` exposure for a real `peer` field (small
FFI proposal); response-side chunked/streaming (needs a
streaming handler shape — separate proposal); bench rows.
macOS ARM64 + Linux ARM64 legs unrun here (same stable
surface; CI tier-1 owns the verdicts).

---

## Stage 3 — context propagation, monotonic clock, structured logging

Status: STAGE 3 COMPLETE (pending final-verifier `check.sh`).
Earlier section text is preserved verbatim.

### API design (ADR-009 implemented exactly)

`gzv.std/context` (new): the `Context` immutable library value
(`pairs: Array<CtxPair>` plus `deadline: int`, `-1` = none —
exactly `-1`; negative absolute times are legitimate past
deadlines and store as immediately expired), built by
`ctx_background` / `ctx_with_value` (derive-only; inputs never
modified) and read by `ctx_get` (fixed `Err` on absence) /
`ctx_get_or` / `ctx_has`. The current task's slot is
authoritative: `ctx_set_current` installs (values + deadline),
`ctx_current` reads back, `ctx_expired()` inspects without
parking (both engines).

Spawn inheritance is the only default that cannot silently drop
traces: the scheduler copies the spawner's slot into each child
at `Spawn`/`SpawnNative` (plain-data clone — no heap reference,
so no GC rooting is needed; the ADR's rooting note is satisfied
by construction: there is nothing to root). Opt-out is explicit
and one-shot: `ctx_detach_next()` arms the next `go`
(`go_detached(f)` wraps it for closures, whose captures still
cross by value). Contexts never ride channels (receiver scope
governs); the opt-in edge helper is `log_trace_edge`, and
message-scoped transfer (`send_with`) awaits user generics, which
v1 does not have — recorded as a Wave 4 follow-up, not smuggled
in as partial per-type variants.

Cancellation = scope failure. `ctx_with_cancel()` snapshots the
caller's CURRENT scope into a `CtxCancel` handle — no scope is
created and no task moves (the snapshot reading of "fails that
scope"); `ctx_cancel(h)` raises `ToSched::CtxCancel`, served
inline by the standard first-panic-wins sweep (the ADR-009
`cancel_scope` hook — a scheduler request, not an FFI call).
Parked tasks in the subtree release with fail-fast E0214 at the
next suspension point; in-flight waits unwind exactly like a
scope panic; the failure propagates upward like any scope panic,
so cancellation is program-fatal by design (proven by the
`proj18` golden: main returns 0, the run still exits 1 with the
cancellation panic). Forged handles bounds-check loudly
(`Err`, never silent misroute).

Deadlines = carried values, enforced at suspension points.
`ctx_with_deadline(ctx, after_ms)` stores an absolute
monotonic-ms timestamp; every pre-park fail-fast site (`park`
for send/recv/select, blocking/file/foreign calls, network and
TLS waits) compares the slot deadline against the Wave 2 clock
and fails the scope with deadline-exceeded (E0214) when passed —
the same path as cancellation (the unwind fails the scope
through the normal panic path). A wait admitted before expiry
runs to its normal wakeup, then fail-fasts at the next
suspension; preemptive mid-wait wakeups are a recorded follow-up.
The sequential interpreter carries deadlines but never enforces
them (no suspension points exist there). No timeout parameters
anywhere (§17.5 stands).

`gzv.std/time` gains the Wave 2 clock (no new module — the
module header anticipated exactly this): `clock_monotonic_ms()`
(absolute ms since process start, shared with deadline checks
and log timestamps) plus `clock_elapsed_since`. Still no wall
clock in v1; values are nondeterministic by construction and
never pinned in goldens.

`gzv.std/log` (new): levels `debug/info/warn/error/off`
(runtime gate — filtered calls cost one int compare and never
format); format is one JSON object per line in exact field order
`{"ts":..,"level":"..","msg":"..","ctx":{..}}` with sorted ctx
keys, `ts` monotonic ms, `ctx` the CURRENT task's pairs (request/
trace IDs flow with no per-call-site plumbing); escaping covers
`"`/`\`, short forms, and lowercase `\u00xx`; lines never
contain raw newlines. Sinks: `stdout` (captured output, one
`println` per record — never interleaved), `stderr` (fd
directly), `buffer` (retained for `log_drain_lines` — tests and
golden pipelines). No file sink (no append builtin). Returns
`Ok(1)` for a produced stdout line, `Ok(0)` otherwise (filtered
or non-stdout sink), `Err` only on runtime misuse.

Zero new builtins (the 27-name catalog is untouched): the three
surfaces ride 17 new linked `toto_rt_*` names (1 clock +
11 context + 5 log; `docs/ffi.md` records the per-binding
contracts) plus one scheduler request. The `Context` struct
itself never crosses FFI — only C scalars and strings through
per-field getters/setters. The multi-file panic renderer gained
caller-file frame attribution: frame spans are call-site
coordinates, so attributing them to the callee's file misrendered
cross-file traces (a stdlib-coordinate span against the entry
source — found via the cancel golden). Single-file output is
byte-identical.

### Implementation (runtime + library)

- `crates/toto-compiler/src/ctxlog.rs` (new): process clock
  (`Instant`-based monotonic ms), task-slot shape (`CtxSlot`:
  ordered pairs + optional absolute deadline + one-shot detach
  flag), and the process log gate/sink/buffer plus the exact line
  formatter and escaper. Pure Rust — no new C symbols, no
  `dlopen`. Unit-tested (escapes, ordering, gate, drain).
- `crates/toto-compiler/src/ffi.rs`: +17 linked names (+17
  `POSIX_ONLY` — unix-gated like the other Wave 2 surfaces);
  interpreter arms over a thread-local slot (carried, never
  enforced — the sequential engine has no suspension points;
  `cancel` validates its handle only).
- `crates/toto-runtime/src/sched.rs`: per-task `ctx` (+
  `ctx_error`) slot fields; spawn inheritance (+ one-shot
  detach) in both `Spawn` arms; `ToSched::CtxCancel` served by
  the standard sweep with the caller's trace for attribution
  (empty traces misattribute stdlib spans to the entry file).
- `crates/toto-runtime/src/exec.rs`: `CTX/CLOCK/LOG_FNS`
  inline routing (never park, never block); slot access with the
  Wave 1 ABBA order (slot lock before heap lock, never nested);
  `check_deadline` at every pre-park fail-fast site; validation
  failures return `-1` + slot (the wrapper's `Err`), never a
  panic.
- `crates/toto-jit/src/compile.rs`: `toto_rt_ctx_/clock_/log_`
  refuse arms (executor-only surface; verified by hand via
  `jit_probe` with the workaround message).
- `stdlib/context.gzv`, `stdlib/log.gzv` (new), `time.gzv`
  clock extension; `stdlib.rs` 17→19 modules;
  `project.rs` count test 17→19; `diag13` golden +17 names
  (deliberate — the allowlist grew); `docs/ffi.md` Stage 3
  section + contracts; `docs/stdlib.md` `context`/`log` sections
  + clock paragraph (suite count 8→16, long stale).

### Tests + evidence (all passing at the Stage 3 tip)

- `stdlib/tests/test_context.gzv` (new, executor): values
  (immutability, order, fixed missing-key message), slot
  round-trip + clear, inheritance, one-shot detach (incl. the
  `go_detached` closure form), deadline values + `ctx_expired`
  inspector, forged-handle message. Fatal paths stay out (a
  valid `cancel` ends the run — see `proj18`).
- `stdlib/tests/test_log.gzv` (new, SEQUENTIAL — no `context`
  import, so it covers the interpreter arms): level gate incl.
  fixed unknown-name messages, exact wire format rebuilt around
  the observed `ts`, escaping round-trip, empty `ctx`, edge
  record, sink returns; restores level/sink globals. stdlib
  `toto test` 16/16.
- `tests/corpus/project/proj17_context_log/` (new): the full
  live pipeline through the buffer sink — values, level filter,
  format, inheritance into a spawned child (no plumbing in the
  child), detach, causality edge — with `ts` normalized to 0 at
  print time, so the golden stays exact while proving the live
  clock, live slots, and emission order. Exact goldens; green
  3/3 plus `--workers 4` (byte-identical). The golden pins the
  request-scoped trace ID (`req-7f3a`, `r-42`) on every
  inheriting record and `{}` on the detached one.
- `tests/corpus/project/proj18_ctx_cancel/` (new, exit 1):
  `with_cancel` in the child + park + main-side `cancel` →
  `panic[E0214]: context cancel: handle 1 failed scope 1 ...`
  with a coherent cross-file trace. Exact goldens; green 3/3
  plus `--workers 4` (byte-identical).
- `crates/toto-runtime/tests/ctx_stage3.rs` (new): past
  deadline fail-fasts at park entry with E0214
  deadline-exceeded (deterministic — nothing is ever sent);
  future-deadline control parks and wakes normally (the check
  admits live waits).
- `ctxlog` unit tests (3): escapes, field order + key sort,
  gate validation + filter + drain.
- Package gates at the Stage 3 tip: `cargo fmt --check` clean;
  `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test` green for toto-compiler (incl. 19-module table),
  toto-runtime (incl. the 2 new deadline tests), toto-cli
  (corpus incl. `proj17`/`proj18` + `diag13`), toto-jit; stdlib
  `toto test` 16/16. Full `check.sh` left to the final verifier
  per the brief. `/tmp` writes fail in this sandbox (EDQUOT),
  so captures use `target/` and `TMPDIR=target/sandbox-tmp`
  was set for the affected suites; the one pre-existing
  environmental failure (blocking seed file) passes with it.

### Follow-ups (recorded, not done)

Preemptive mid-wait deadline wakeups (timer integration, not
entry checks); message-scoped `send_with` transfer (needs user
generics — Wave 4); file sink (needs an append builtin);
`getpeername` for a real log `peer` field; hostname/SNI/ALPN
(TLS waves); bench rows; macOS ARM64 + Linux ARM64 legs unrun
here (same portable-Rust surface; CI tier-1 owns the verdicts).

---

## Stage 4 — Prometheus metrics (counters, gauges, histograms)

Status: STAGE 4 COMPLETE (pending final-verifier `check.sh`).
Earlier section text is preserved verbatim.

### API design (no spec change — §17/§18 implemented exactly)

`gzv.std/metrics` (new, pure Toto over `gzv.std/http` — zero new
builtins, zero new FFI names, `diag13` golden untouched): counters,
gauges, and histograms with Prometheus text-exposition 0.0.4
rendering plus a scrape responder for the framework.

State types (FIXED — one choice each, golden-pinned): a
`MetricsRegistry` holds `counters`, `gauges`, and `histograms`
(registration-ordered definition arrays). Every definition
carries `name`, `help`, and `label_names`; every observation is
a sample under one label-value set (first-observation order).
`MetricsSample` pairs label values with the current total;
`MetricsHistoSample` pairs them with cumulative per-bucket
`counts` plus `sum`/`count`. Counters hold monotonic `int`
totals (`metrics_inc` rejects negative deltas loudly); gauges
hold `int` values set or added to (`metrics_gauge_set/add` —
deltas may be negative); histograms observe `int` values into
cumulative `le` buckets. Values are ints (request counts,
millisecond latencies) — float observations are a follow-up
with the full bytes story.

Registration (FIXED): every metric registers before use; the
name must match `[a-zA-Z_:][a-zA-Z0-9_:]*` and be unique across
ALL kinds (Prometheus forbids a counter and gauge sharing a
name); label names follow the same grammar, must be unique
within the metric, must not start with `__` (reserved), and `le`
is reserved on histograms (the renderer owns it); histogram
bounds must be non-empty and strictly increasing. Unknown
metrics, wrong-arity label sets, and non-HTTP-text help/label
values are all fixed-`Err` (never silent, never echoing input).
Stored state copies caller arrays (arrays are shared by
reference — registration/observation must not alias them).

Format version (FIXED): `metrics_exposition_version()` returns
`"0.0.4"`; `metrics_content_type()` returns
`text/plain; version=0.0.4; charset=utf-8`. Render order is
counters, gauges, histograms, samples in first-observation
order, labels in declaration order (`le` first on bucket lines,
as the reference clients emit). HELP escapes `\`/LF, label
values escape `\`/`"`/LF — exactly the reference escaping; a
registered-but-unobserved metric still emits HELP/TYPE; no
timestamps, no `# EOF` (OpenMetrics, not 0.0.4). The scrape
endpoint is `route_add(router, "GET", "/metrics",
metrics_handler(reg))` + `http_serve` (loopback rule rides
along; no timeout parameters); the handler closure shares the
registry arrays (the `mwlog` pattern), so scrapes see every
observation. Returning a bare `fn` from `metrics_handler` is
sound — only `Result`-wrapped function values hit the v1 E0204
unwrap edge (Stage 2 report), and the router builder still takes
the router as a parameter for that reason.

### Implementation (library only)

- `stdlib/metrics.gzv` (new): state types, ASCII name grammar
  (comparison-based, mirroring the http codec's acceptance),
  registration/observation with fixed errors, reference-matching
  escapers, the 0.0.4 renderer, `metrics_respond` (200 +
  pinned media type via `http_response`/`http_text_to_bytes`)
  and `metrics_handler` (closure over the registry).
- `crates/toto-compiler/src/stdlib.rs`: 19→20 modules;
  `crates/toto-compiler/tests/project.rs`: 19→20 count pin.
  No `toto-jit` refuse arms (pure Toto — the JIT suite stays
  green unmodified); no `docs/ffi.md` change (no new linked
  names); `docs/stdlib.md`: new `metrics` section + suite
  count 16→17.
- `stdlib/tests/test_metrics.gzv` (new, sequential): version
  pins, empty/unobserved renders, exact multi-series render
  (counters/gauges/two histograms), escaping round-trip,
  alias isolation (mutating caller arrays post-registration
  changes nothing), the handler value over a synthetic
  `HttpRequest` (status + media type + body byte-match), and
  every fixed error string. stdlib `toto test` 17/17.
- `tests/corpus/project/proj19_metrics/` (new): the stated
  scenario — fixed direct observations seed the series, two
  live `/work` requests add handler-side observations, and a
  scrape of `/metrics` through the running service must
  byte-match a local render with the pinned media type
  (lowercase lookup proves case-insensitivity). Exact goldens;
  green 3/3 plus `--workers 4` byte-identical.

### Tests + evidence (all passing at the Stage 4 tip)

- Exposition validation: no reference Prometheus parser is
  available offline (`prometheus_client` absent; offline cargo
  cannot fetch a crate) — so the golden's 21-line exposition
  (3 metrics, 5 series) was validated with a strict checker
  written from the 0.0.4 spec text (name grammar, HELP-before-
  TYPE, HELP/TYPE per metric, label grammar + no-duplicates +
  `le`-first on bucket lines only, cumulative counts,
  terminal `+Inf`, one `_sum`/`_count` per series, value
  grammar, escape validity). It accepts the golden and rejects
  three structural mutations (non-cumulative counts, dropped
  `+Inf`, bad label escape). Recorded gap: spec-text
  validation, NOT reference-parser interop — no interop
  invented. The checker is a scratch probe
  (`target/sandbox-tmp/check_exposition.py`), not committed.
- Package gates at the Stage 4 tip: `cargo fmt --check`
  clean; `cargo clippy --workspace --all-targets -- -D
  warnings` clean; `cargo test` green for toto-compiler
  (incl. the 20-module table), toto-runtime (incl.
  `fmt_equiv`, which formats and runs `proj19` under GC
  pressure), toto-cli (corpus incl. `proj19` + `diag13`
  unchanged + `http_interop`), toto-jit; stdlib `toto test`
  17/17. Full `check.sh` left to the final verifier per the
  brief. Two environmental notes (both proven pre-existing on
  the clean tree): `/tmp` writes fail in this sandbox
  (EDQUOT — captures use `target/`, `TMPDIR=target/sandbox-tmp`
  set for the suites) and `fmt_equiv` needs
  `RUST_MIN_STACK=33554432` here (stack-overflow abort at the
  default 8 MB thread stack on the pristine base too;
  green with the override, with and without Stage 4).

### Follow-ups (recorded, not done)

Float observations (needs the bytes story); `_total` suffix
convention enforcement (allowed but not required in v1);
timestamped samples (omitted — 0.0.4 parsers ignore them);
reference-parser interop run (needs network/crate);
exemplars and `__`-reservation lifting (spec revisions);
per-series scrape filtering; bench rows; macOS ARM64 +
Linux ARM64 legs unrun here (same portable-Rust surface;
CI tier-1 owns the verdicts).

---

## Stage 5 — reference service + benchmarks + acceptance

Status: STAGE 5 COMPLETE (final verifier — full `check.sh`
scope run here, not deferred). Earlier section text is
preserved verbatim.

### 5.1 Reference service (location, does / does-not, client proof)

Location: `tests/corpus/project/proj20_refsvc/` — `main.gzv`
(the service + client proof in one deterministic driver),
exact goldens (`main.stdout` 49 lines, `main.stderr` empty,
`main.status` 0), pinned fixtures (`fixtures/ca.pem`,
`server-cert.pem`, `server-key.pem`, `other-ca.pem` — byte
copies of the `proj15`/`proj16` set, `openssl verify` lineage
recorded in the Stage 1 section). A corpus project, not an
`apps/` entry, deliberately: every exchange is golden-pinned
and runs in the CLI corpus gate (3/3 plus `--workers 4`
byte-identical), so the service is a regression test, not a
sample.

What it does: serves `GET /hello`, `GET /users/{id}`
(template capture, pct-decoded at use), `POST /echo`
(binary echo), and `GET /metrics` over TLS on loopback
(`http_serve_tls`, port 48522, `server_name "toto-refsvc"`).
Every request passes the logging middleware (one `info`
line, method + origin-form target) and each handler
counts/gauges/observes into one shared registry
(`refsvc_requests_total{method,route}` counter,
`refsvc_inflight` gauge bracketed add/sub, fixed 1ms
`refsvc_latency_ms{route}` observations, buckets
`[5,25,100,500]`); `/metrics` is `metrics_handler` over
that registry. The request trace is installed once
(`ctx_set_current` with `trace_id "refsvc-1"`) and flows by
spawn inheritance into accept-loop and per-connection
tasks — every golden log line carries it with no
per-handler plumbing, which is the observable proof of the
Stage 3 inheritance rule. The Toto-native client verifies
the chain against the pinned CA and exercises every route,
checks the scrape byte-matches a local render with the
pinned media type, proves wrong-CA rejection and
post-failure survival, then drains the buffer sink with
`ts` normalized (same normalizer shape as `proj17`).

What it deliberately does NOT do: no hostname
verification, no client certificates, no SNI/ALPN/
resumption (TLS follow-ups, Stage 1); no non-loopback bind
and no `Host`-based virtual hosting (service waves);
no auth, no persistence, no graceful in-flight drain on
`net_close(listener)` (close aborts parked accepts; served
connections run to close — the abort-then-survival
semantics are `proj16`'s); no file sink (needs an append
builtin, Stage 3 follow-up); no wall-clock timestamps in
logs (`ts` is monotonic ms by design); no float observations
(bytes story follow-up, Stage 4); no keep-alive on the
client side (one-shot `Connection: close` — each request
is a fresh TCP connection, and a fresh TLS handshake for
`https`); handler latency observations are a fixed 1ms
stand-in so the exposition golden stays exact (the wiring —
observe in the live handler path, scrape the shared
registry — is what is proven; real latencies are
deployment data, measured in §5.2, never pinned).

Evidence: golden pins `hello=200:hello from refsvc`,
`user=200:user:42`, `echo=200:ping-bytes`, `neg=yes`,
`after=200:hello from refsvc`, `metrics=200:match`, the
full 31-line exposition (2 `GET /hello` samples from the
survival re-request, gauge `0`), 10 buffer-sink log lines
(startup + middleware/handler pairs + the bare
`mw GET /metrics` scrape line), `log-lines=10`, and
`Ok(REFSVC-OK)`.

### 5.2 Benchmarks + methodology

Machine (all Stage 5 numbers): Linux x86_64, AMD Ryzen 5
5600G (12 threads), 27 GB RAM, `rustc 1.100.0`, OpenSSL
3.5.5, debug `toto` binary unless stated. Clocks: shell
`date +%s%N` wall (precise totals) + in-Toto
`clock_monotonic_ms` (excludes process startup from RPS).
Method: sequential one-shot clients (N=200, port series
48611/48612), one 8×25 concurrent run (port 48711), raw
substrate control (N=200 echo ping-pong, port 48721),
`gc_perf` in debug + release, `exec-gc-*` bench workloads
×3. Probes are scratch (`target/sandbox-tmp/loadproj/`,
not committed); the golden service itself is never timed.

| workload | N | total | RPS | per-request |
|---|---|---|---|---|
| HTTP seq, plain | 200 | 15.791 s | 12.7 | mean 79.0 ms; all 200 in [50,100) ms |
| HTTP seq, TLS | 200 | 18.552 s | 10.8 | mean 92.8 ms; 199 in [50,100) ms, max 102 ms |
| HTTP conc 8×25, 1 worker | 200 | 15.200 s wall | 13.2 | worker-sum 118.7 s |
| HTTP conc 8×25, 4 workers | 200 | 4.893 s wall | 40.9 | worker-sum 38.5 s |
| raw TCP echo, seq | 200 | 0.112 s | 1786 | mean 0.56 ms |

Stability: the sequential pair repeated 3× (plain
15775/15776/15791 ms; TLS 18563/18548/18552 ms); the
127.0.0.1-vs-localhost control is identical (DNS ruled
out); per-request ms clock is coarse, so percentiles are
published as bands, not false precision (plain p50 and
p99 both lie in [50,100) ms; TLS p99 ≈ 100 ms).

Reading: the one-shot client dominates — every request
pays a fresh TCP connect (plus a fresh TLS handshake for
`https`, +13.8 ms/req over plain). The raw control
(0.56 ms/req through the same executor) proves the
remainder is the interpreted HTTP path: Toto-level
framing + executor park round-trips per send/recv in the
debug interpreter, not the substrate. Concurrency scales
the wall (15.2 s → 4.9 s at 4 workers for the same 200
requests) while per-request cost rises under contention
(worker-sum 118.7 s → 38.5 s), the expected M:N
contention signature in a debug build. No claim is made
about release/AOT service throughput — see §5.3.

GC pauses vs the Wave 0 budget (p99 ≤ 10 ms at ≤100k
live, production/release), freshly measured here:

| live set | release med | release p99 | debug med | debug p99 |
|---|---|---|---|---|
| ~1k | 24 µs | 34 µs | 115 µs | 175 µs |
| ~10k | 298 µs | 396 µs | 1146 µs | 1213 µs |
| ~100k | 5434 µs | 6539 µs | 14156 µs | 15356 µs |

Release p99 at 100k is 6.5 ms against the 10 ms budget
(1.5× headroom — same standing as the Wave 0 6.8 ms
measurement, re-derived on this machine); the debug leg
passes its 30 ms guard bound (15.4 ms p99). Throughput
under GC pressure, same machine, n=3 medians:
`exec-gc-alloc` 2766 ms vs baseline 2682 (1.03×),
`exec-gc-chan` 3670 ms vs baseline 3553 (1.03×) — both
far below the 5× CI flag.

CI bench rows + flagging rule (unchanged, `bench/baseline.txt`
+ `scripts/bench.sh --baseline`): every timed label is
compared against its recorded median after the run; any
median above 5× its baseline prints FLAG and fails the
suite (exit 1). CI runs `--runs 3` with this flag; the
manual n=5 run stays the source of published numbers. The
full `bench.sh` suite (~25 min: cold debug + release AOT
links) runs in the CI `bench` job, not in this session.

### 5.3 Unbenchmarked-surface decision (recorded, not silent)

Not measured here, with rationale each: (a) full
`bench.sh` re-run — ~25 min wall, CI `bench` job owns it
under the 5× rule (§5.2); spot rows re-measured instead.
(b) Release/AOT service RPS — the service is a
correctness golden (debug interpreter); release/AOT
throughput needs a calibrated deployment harness, not a
corpus driver. (c) TLS handshake throughput in isolation
— one-shots and handshakes do not fit the calibrated-
workload methodology (Stage 1 follow-up, still open).
(d) GC pauses above ~100k live — outside the published
budget by definition (Wave 0 disposition, carried).
(e) Reference-parser interop for the exposition — no
reference parser available offline (Stage 4 gap, still
open). (f) macOS ARM64 + Linux ARM64 legs of every new
measurement — same portable surface; CI tier-1 owns the
verdicts.

### 5.4 Test totals (fresh full run, per-binary + census)

`cargo fmt --all -- --check` clean; `cargo clippy
--workspace --all-targets -- -D warnings` clean.
`cargo test --workspace` green: 66 binaries, 240 passed,
0 failed (full binary→count map retained at
`target/sandbox-tmp/testmap.txt`, scratch — per-crate
roll-up below). Overrides used (both proven pre-existing
on the clean tree): `TMPDIR=target/sandbox-tmp` (`/tmp`
EDQUOT) and `RUST_MIN_STACK=33554432` (`fmt_equiv`
stack abort at default 8 MB).

| crate / suite | passed | failed |
|---|---|---|
| ffi-smoke-test | 7 | 0 |
| spikes (1, 1b, 2, 3) | 31 | 0 |
| toto-cli lib + cli + corpus + http_interop + lsp + packages | 51 | 0 |
| toto-compiler lib + 9 integration binaries (incl. tls_interop 3, project 15) | 47 | 0 |
| toto-jit lib + 13 integration binaries | 68 | 0 |
| toto-runtime (blocking 4, concurrent 3, ctx_stage3 2, fmt_equiv 1, gc 11, gc_perf 3, mn 5, net_hammer 1, sequential 1, stack 1, tls_openssl 3) | 35 | 0 |
| zero-test harness shells | 1 line each, 0 | 0 |
| cargo total | 240 | 0 |
| stdlib `toto test` | 17 | 0 |
| wordfreq `toto test` + interp==built identity | 1 + identical | 0 |

Corpus census: 181 cases green inside the single corpus
harness test — 160 top-level `.gzv` (concurrent 22,
diagnostics 16, interp 30, lexing 5, parsing 9, results
13, typing 65) + 21 project mains (`proj01`–`proj20`
plus the second `proj11` net variant; 9 project support
libs ride along, never run standalone). 181 `.status`
+ 181 `.stderr` + 179 `.stdout` goldens: `conc18_fneq`
and `int04_modzero` pin only stderr/status (no stdout
file — pre-existing shape, harness green).

### 5.5 Acceptance checklist (§7 of the brief)

1. Reference service in the repo, served over TLS, with
   logging + metrics wired (§5.1: location, does/
   does-not, 49-line golden) — DONE.
2. Client talks to it as integration proof (chain-verified
   Toto client over `https`, every route + scrape +
   wrong-CA + survival in the golden) — DONE.
3. RPS, latency percentiles, and GC pauses under load vs
   the Wave 0 budget, with methodology (§5.2: workload,
   concurrency, duration, machine) — DONE (release p99
   6.5 ms ≤ 10 ms at 100k; bands published at ms
   resolution, no false precision).
4. CI bench rows with the stated flagging rule (5×,
   fail-closed, §5.2) — RECORDED (suite runs in CI).
5. Unbenchmarked-surface decision — RECORDED (§5.3,
   six items with rationale).
6. Report complete: stage-by-stage (§§1–5), this
   checklist, TLS choice (Stage 1), API design (Stage 2),
   context record (Stage 3), benchmarks + methodology
   (§5.2), every deferral with rationale (§5.6) — DONE.
7. Test totals re-derived from a fresh run with
   per-binary account + corpus census (§5.4: 240 + 17 +
   wordfreq, 181 cases) — DONE.

Hard constraints held (violation = STOP, none
triggered): D1–D8 frozen; 27 builtins frozen (zero new
in this wave — TLS/context/log ride linked names +
pure-Toto modules); Phase 5 module model intact (path
imports, flat namespace); ADR-009 + spec §17 APPROVED
as written 2026-11-21, implemented exactly (§17/§18
untouched); static linking only; no structs-by-value,
no `dlopen`, no binding generation; no Toto-owned
crypto/TLS primitives; HTTP/1.1 only; no Waves 3–5
work; Windows deferred; no release/tag/artifact; no
force-push; named files staged only; no merge to main;
no spec-silence encountered (nothing assumed).

### 5.6 Deferrals carried with rationale (consolidated wave close)

- Handler/middleware `Err` → connection abort, not the
  designed 500 mapping; `expose_errors` reserved
  (Stage 2) — needs Phase 2 destructuring or a gated
  builtin; handlers return explicit 500s.
- `?`-unwrap of `Result`s transitively holding function
  values panics E0204 (Stage 2) — runtime edge, all
  framework call sites structured around it.
- Hostname verification, SNI/ALPN, resumption, client
  certs (Stage 1); non-loopback bind, `Host` virtual
  hosting, `getpeername` peer field (Stages 1–3) —
  service waves each need their own proposal.
- Preemptive mid-wait deadline wakeups; message-scoped
  `send_with` (needs user generics — Wave 4); file sink
  (needs append builtin).
- Response-side chunked/streaming (needs a streaming
  handler shape — separate proposal).
- Float observations, `_total` enforcement, timestamped
  samples, exemplars, per-series filtering (spec
  revisions); reference-parser interop (needs network).
- Release/AOT service throughput + full `bench.sh`
  here (CI owns both); >100k-live GC (outside budget);
  per-request ~79 ms uniformity not root-caused beyond
  the substrate contrast (profiling follow-up, not a
  claim).
- macOS ARM64 + Linux ARM64 legs of all new tests and
  measurements (CI tier-1 owns the verdicts); Windows
  stays DEFERRED per the POSIX-compat decision.
- No real release, tag, or artifact publication
  (forbidden).

## Wave verdict

Wave verdict: ACCEPTED-WITH-FLAGS — all five stages
landed on `enterprise/wave-2-services-http-logging-metrics`
(TLS via system libssl, HTTP/1.1 framework, context +
clock + logging, Prometheus metrics, reference service
+ benchmarks + this report), full gates green locally
(258 executed checks, 0 failed; corpus 181/181). Flags:
(a) macOS ARM64 + Linux ARM64 legs unrun here — CI
tier-1 probe owns the verdict; (b) the full `bench.sh`
suite and release/AOT service numbers run in CI, not in
this session (§5.3). No frozen-decision change, no new
builtin, no spec change, no Waves 3–5 work, no release
artifact, no merge to `main`.

## Fix-forward dated addendum (2026-11-22, on branch; append-only)

Earlier section text is preserved verbatim; the corrections
below are the record. Stage commits (correct hashes — §9
conflated Stage 4 with Stage 2): `9446e60` Stage 1 TLS,
`4228270` Stage 2 HTTP, `672af88` Stage 3 context/logging,
`56cff63` Stage 4 metrics, `dac9c6a` Stage 5
refsvc+benchmarks+report; fix-forward commits on top:
`3c12a8b` Fix 1 (build.rs), `d48a603` Fix 2 (fmt_equiv
harness). Branch `enterprise/wave-2-services-http-logging-metrics`,
from `main` at `bcc0824`, no merge.

(a) Verdict. The header status and the wave verdict above
are corrected to the true state: NOT YET ACCEPTABLE —
STOP. CI run 44886709131 (head `dac9c6a`, single attempt)
conclusion failure: `check` failed, macOS probe
(`tier1-probe macos-latest`) failed; `release-gates`,
`bench`, Linux ARM64 probe green; Windows probe fails as
the expected informational failure under
`continue-on-error` — but Windows is NOT the only
failure, so the gate is violated. The verdict changes to
accepted only when both red gates close and the tip check
is green on a new run (§4 of the brief).

(b) The missing Stage 0 (starting-state verification +
carry-forward disposition; no Stage 0 section existed).
`main` at `bcc0824` (verified `git rev-parse main`,
clean tree pre-branch). `check.sh` exit 0 on `main`
(Wave 1 merge record: 66 result lines, 0 failed).
CI run 44832805780 green on its current attempt
(re-verified this turn: conclusion success, head
`bcc0824`; only the Windows probe failing, informational).
PDF verified this turn: title exactly "Toto (.gzv)
Implementation Roadmap v2.5 and Phase 0 Decision Doc
(D1-D8)", 41 pages (`pdfinfo`), md5
`819bece4eb63aa13cd88f164bad94b75`, from
`/home/gerald/Downloads/toto-implementation-roadmap-v2-5.pdf`.
Correction: the §9 claim that the corrupt `/tmp` PDF copy
is "recorded in the report" is false — it appears nowhere
in this file. The record is made here: the `/tmp` copy
(53248B) is corrupt (`pdfinfo` exit 1, missing
trailer/xref); verification used the intact Downloads
copies. Dispositions (none dropped): CLOSED in Wave 1,
no carry — E0216 lost-wakeup race (queue-LAST fix),
`rebuild_cancels_parked_generation` E0216/E0214 race
(closed empirically), multi-worker ABBA stall (`ac6933c`
+ `net_hammer_mw`). Answered in this wave — bench rows
for new surfaces (§5.2 reference-service benchmarks;
remainder in §5.3). Carried by name — intermittent CI
stall (known open follow-up; harness/checker are frozen
territory); rendezvous-latency measurement (dedicated
measurement, not done here); TOML empty-table reporting
(stdlib scope, needs its own proposal); native AOT
net/crypto/gzip (executor is the reference,
refuse-by-design stands); crypto/compression on Windows
(Windows deferred); all-interfaces bind (lifting
loopback-only needs a gated proposal/ADR plus
STOP-and-ask, not smuggled in); Group C raw-sockets /
packet work / full bytes / WebSockets / HTTP-2 /
security-toolkit libs (separate gated proposals);
Windows Tier-1 POSIX-compat; packaged differential
coverage; GC past 100k + minimal-tunable-set; policy
approvals, release practice, per-platform matrix,
dispatch-404s, reproducibility-compare. No
fix-now-if-small item (no TOML scope change: not
trivially safe).

(c) Checklist mapped to the brief's §7 (the §5.5 list
below is Stage-5-internal and stands as historical;
this mapping is the honest one). 1. Starting state —
DONE (recorded in (b) above). 2. TLS shipped (server +
client via FFI to system libssl, X.509 from Wave 1, no
Toto TLS implementation; interop green both directions)
— DONE, CI re-proof pending. 3. HTTP framework + server
+ client shipped (routing, middleware, handlers on green
threads; HTTP/1.1 only; conformance + curl interop
green) — DONE, with the two v1 deviations flagged for
user acceptance review (`Err` → connection abort, not
the designed 500 mapping; `?`-unwrap of `Result`s
holding function values panics E0204) — NOT redesigned
here. 4. Context implemented per ADR-009 exactly
(`gzv.std/context`, spawn inheritance, scope-failure
cancel, deadlines + Wave 2 clock) + structured logging
shipped (leveled JSON lines, context values without
per-call-site plumbing; goldens match) — DONE. 5.
Metrics shipped (counters, gauges, histograms,
Prometheus-format exposition, goldens match) — DONE
WITH FLAG: the exposition was validated with a
hand-written spec-text checker only; reference
Prometheus-parser interop was NOT run — no
`prometheus_client`, empty cargo cache, no network on
this machine — carried explicitly as a flag, not
claimed. 6. Reference service over TLS + load
benchmarks published (RPS, latency percentiles, GC
pauses vs the Wave 0 budget, methodology in §5.2) —
DONE; full gates green on all supported platforms —
NOT MET (run 44886709131 red, see (a)/(e)). 7. Report
complete with checklist, TLS choice, API design,
context record, benchmarks, deferrals — NOT YET (this
addendum corrects it; complete when §4 closes).

(d) Totals bridge. Fresh full run on the fixed tip
(`d48a603` + `3c12a8b`): `check.sh` exit 0, 66 result
lines, 240 passed, 0 failed (only override:
`TMPDIR` redirect for the pre-existing sandbox `/tmp`
EDQUOT; the `RUST_MIN_STACK` override is no longer
needed after Fix 2). Per-crate: ffi-smoke-test 7;
spikes 8 + 6 + 10 + 8 = 32 (the §5.4 "31" is corrected
— the bridge: 7 + 32 + 51 + 47 + 68 + 35 = 240);
toto-cli 51; toto-compiler 47; toto-jit 68;
toto-runtime 35 (attributed per binary from the gate
log build paths; doc-tests 0 each). 19 zero-test
result lines (8 doc-test shells + 11 empty
unit/integration binaries), 0 passed each — part of
the 66 lines, not additive. Stdlib `toto test` 17/17;
wordfreq `toto test` 1/1 + interp==built identical.
Corpus census 181 (160 + 21) checks, unchanged.

(e) Failure record. Run 44886709131 (head `dac9c6a`,
single attempt): `check` failure, macOS probe failure,
`release-gates` / `bench` / ARM probe success, Windows
probe informational failure. Fix 1 cause (Wave 2 code
defect, confirmed from the file): build.rs lines 46–49
pair `("libcrypto.dylib", "libcrypto")` /
`("libssl.dylib", "libssl")` while lines 66–67 symlink
`libcrypto.dylib` / `libssl.dylib` into OUT_DIR, so
line 76's `cargo:rustc-link-lib=libcrypto` makes rustc
pass `-llibcrypto`, which seeks the never-created
`liblibcrypto.dylib` (precision: those constructed
strings never literally appear in the file). CI log
evidence (job 136793446308, build-workspace step):
`2026-11-21T08:51:01.3756900Z ld: library 'libcrypto'
not found`. Fix: link names `crypto` / `ssl` (as
`main` had them), commit `3c12a8b`; proof is the macOS
probe on the re-run. Scope note kept:
`spikes/ffi-smoke-test/build.rs` emits no crypto/ssl
directives — untouched. Fix 2 cause, Step 1 (from the
CI log, job 136793446111, no assertion without it):
`2026-11-21T08:56:12.1696497Z fatal runtime error:
stack overflow, aborting`, `error: test failed, to
rerun pass '-p toto-runtime --test fmt_equiv'`,
SIGABRT → exit 101. The failing test is
`fmt_equiv_concurrent_and_projects` (the single test
in `tests/fmt_equiv.rs`). Step 2 classification
(only a CI probe on `main` decides this): temporary
branch at `bcc0824` (zero commits on top) → run
44901038722 → conclusion success, `check` success at
the default stack — so NOT pre-existing (hypothesis
(a) falsified); the abort is Wave-2-triggered
(hypothesis (b)): `proj16_http` (`main.gzv` 565
lines, by far the largest corpus file) overflows the
test thread inside `project::load_and_check`, bisected
locally (parse ok, format ok, mirror ok, load aborts).
Probe branch deleted (local + remote) after the
verdict. Step 3 fix: the test body now runs on a
32 MiB harness thread (`std::thread::Builder`,
panics preserved via `resume_unwind`), commit
`d48a603` — green locally at the default stack, no CI
environment change (no `RUST_MIN_STACK` in CI).
Checker-recursion reduction was rejected as
disproportionate (frozen-territory-adjacent, riskier).
Stage 4 hash correction: `56cff63`, not `4228270`
(the §9 message conflated Stage 2's hash; this file
names no hashes — recorded correctly here).

(f) The unnamed gap. The §9 message's "derived result
labels file ... truncated filename" names no
identifiable artifact: no such file exists in this
report or on the branch (searched). The claim is
dropped with this one-line correction; no unnamed gap
remains.

## Fix-forward second dated addendum (2026-11-22, on branch; append-only)

Earlier text preserved verbatim, including the first
addendum above. Fix commit on top of `743dd55`: Fix 4
`04faa4a` (`crates/toto-runtime/src/exec.rs` frame
thinning, this turn — diagnosis and diff in (e) below).
Branch still
`enterprise/wave-2-services-http-logging-metrics` from
`main` at `bcc0824`, no merge. PDF gate re-verified this
turn: title exactly "Toto (.gzv) Implementation Roadmap
v2.5 and Phase 0 Decision Doc (D1-D8)", 41 pages
(`pdfinfo`).

(a) New failure record. Run 44902121760 (head `743dd55`,
single attempt, 2026-11-21T12:04:54Z) conclusion failure
— NOT in the first addendum (it postdates the `743dd55`
push). Per-job: `check` success (7m32s), `release-gates`
success, `bench` success, `tier1-probe
(ubuntu-24.04-arm)` success, `tier1-probe (macos-latest)`
failure, `tier1-probe (windows-latest)` failure
(informational under `continue-on-error`). Windows is NOT
the only failure, so the gate stays violated. macOS job
136840022937: `build workspace` success (28s), Tier-1
conformance success, `corpus gate` failure (3s), final
`conc01` step skipped — a test-RUNTIME failure, not a
build failure. Fix 1 proved (macOS links clean — the
failure moved from build time to test runtime) and Fix 2
proved (`check` green at the default stack, no
`RUST_MIN_STACK` override); both stay closed, not
re-opened. Reported shape (relayed account, per-program
text unconfirmed — see the log note): the six
serve/listen corpus programs abort identically with want
exit=0 got exit=-999 (signal 10, SIGBUS) —
`conc20_net_hammer`, `proj11_net_echo`, `proj15_tls_echo`,
`proj16_http`, `proj19_metrics`, `proj20_refsvc` — every
program that serves/listens; pure-compute, crypto,
logging, and error-path cases green. Regression
baseline: run 44901038722 (temporary probe branch at
`bcc0824`, since deleted) concluded success with the
macOS corpus gate green (2s) — main green, branch red,
same gate, runs 14 minutes apart (no runner-image drift).
Log note (honest gap, §1): the CI log text could NOT be
retrieved with this token — `gh api
repos/iamstoick/toto/actions/jobs/136840022937/logs` and
`gh run view --log` return HTTP 403 (admin rights
required); check-run annotations confirm only `Process
completed with exit code 101` on `macos arm64` runners.
The crashing SITE is therefore established by local
repro + measurement instead (see (e)); the signal (10)
is the relayed account, consistent with all of it. The
per-program log-text confirmation stays open and is NOT
claimed here.

(b) Two slip corrections to the first addendum. (i) (b)
cites "Wave 1 merge record: 66 result lines" — Wave 1
was 62 result lines; 66/240 is Wave 2's gate. Corrected:
62. (ii) (b) dispositions list the E0216 race as "CLOSED
in Wave 1, no carry" — its root cause is still an open
Phase 4 follow-up (closed empirically, NOT root-caused).
Carried by name as an open follow-up; not closed.

(c) §7-mapped checklist updates (nothing else changes
mark without evidence). Criterion 2's "CI re-proof
pending" closes: the `check` job went green on run
44902121760 (recorded in (a)). Criterion 6 stays NOT MET
until the new (fix-tip) run goes green. Criteria 1, 3, 4,
5, 7 unchanged.

(d) Totals. The §2 fix changes no test counts (two
private methods, no new/removed tests), so the (d)
bridge stands unchanged — verified identical on the
fixed tip locally: `check.sh` exit 0, 66 result lines,
240 passed, 0 failed (7 + 32 + 51 + 47 + 68 + 35);
stdlib `toto test` 17/17; wordfreq `toto test` 1/1 +
interp==built identical; corpus census unchanged.
Re-derive only if a later change moves a count.

(e) Diagnosis and fix (this round). Root cause: Wave 2
Stage 3 grew the executor's serve-path frames past the
spec-fixed 64 KiB coroutine budget (ADR-002/ADR-008,
ratified — the budget is unchanged by this fix) on
ARM64, faulting the guard page (silent abort; Darwin
reports the protection fault as SIGBUS). Evidence chain
(all local, x86_64 debug builds):
- Coroutine high-water probe (the Wave 1 instrument,
  per-task pattern-fill scan): branch-tip serve programs
  read 61,160 / 65,536 bytes — 93.3% of budget, identical
  across conc20/proj11/proj15/proj19/proj20 (a
  program-independent engine-path peak).
- Bisection by stage commit (same probe, fresh
  worktrees): `bcc0824` 60,536; Stage 1 (`9446e60`)
  60,536; Stage 2 (`4228270`) 60,536; Stage 3
  (`672af88`) 61,160. The full +624-byte growth is
  Stage 3 (context/clock/logging).
- Crash-mode repro (the Wave 1 method — shrink the
  stack default): conc20 at 56 KiB and 32 KiB coroutine
  stacks dies with a silent guard fault (SIGSEGV on
  Linux; the macOS ARM64 equivalent is SIGBUS); 64 KiB
  passes. Shallower programs are unaffected — hence
  serve-only. Main-thread stacks excluded by `ulimit -s`
  sweep on the real binary (8 MB default vs ≤2.2 MB
  needed — huge margin). No new macOS-only unsafe
  exists in the plain-TCP path (`set_blocking` is
  TLS-only); TLS FFI signatures are all long-stable and
  link-proven; guard math verified (corosensei adds a
  full page guard on top: 64 KiB usable on both page
  sizes); link/startup causes ruled out by
  serve-only-ness.
Fix (spec-compliant, the Wave 1 pattern — thin the
frames, never the budget), in
`crates/toto-runtime/src/exec.rs` only: (1)
`check_deadline` hot frame thinned — cold expiry
`format!` outlined to `#[inline(never)]
deadline_expired`; (2) `exec_foreign` dispatcher thinned
— generic pool tail outlined to `#[inline(never)]
exec_foreign_pool`. Behavior identical (code moved, not
changed). Post-fix high-water: 56,952 bytes on all six
serve programs — 3,584 bytes UNDER the
Wave-1-proven-green baseline (60,536), 8.6 KiB of
headroom to the budget. Local proof: six serve programs
exit 0; `check.sh` exit 0 (66/240/0, per (d)); release
`stack_jit` + `timings` green; workspace clippy
`-D warnings` clean. CI re-proof (macOS gate green on
the fix tip) is the remaining proof — §4 chat report.
Wave 1 precedent on record: the identical class (ARM
guard fault at ~1 KiB margin, 64,512/65,536) was fixed
the same way (thin `call_builtin` dispatch over
`#[inline(never)]` group helpers).

(f) Re-proof. CI run on the fix tip (id assigned at
push; verdict reported in the §4 chat report, recorded
in the file next turn — the round-1 pattern: this
addendum records the motivating run 44902121760, the
re-proof run follows).
