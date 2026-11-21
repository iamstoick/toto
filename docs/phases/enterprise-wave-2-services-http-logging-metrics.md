# Enterprise Wave 2 report — services, HTTP, logging, metrics

Status: STAGES 1–3 COMPLETE (pending final-verifier `check.sh`).
Later stages append below; earlier section text is preserved
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
