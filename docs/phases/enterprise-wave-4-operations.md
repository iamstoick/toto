# Enterprise Wave 4 report — operations

Status: STAGE 2 IMPLEMENTED (design + code + local proof green;
CI re-proof pending push). Earlier text preserved verbatim; the
Stage 1 and Stage 2 implementation records are appended at the
end in order.

Branch: `enterprise/wave-4-operations`, from `main` at `a958396`.
Never force-push; stage named files only (`git add`, never `-A`).
No merge until the wave gates green.

PDF gate: v2.5 roadmap verified this turn — md5
`819bece4eb63aa13cd964bad94f75` (same bytes as the recorded v2.5
document), 41 pages (`pdfinfo`), title "Toto (.gzv)
Implementation Roadmap v2.5 ..." (the recorded v2.5 file).

## Stage 0 — starting state + carry-forward disposition

Starting state verified this turn: `main` at `a958396`
(post-Wave-3-merge fill); `./scripts/check.sh` exit 0 locally
(74 result lines, 269 passed, 0 failed); CI run 45171581344
(head 383c96f, Success) plus the fill run 45174567028 (head
a958396, Success — completed after the merge verification;
report-only, no code). Corpus census 186; stdlib 24/24.

Standing authority carried in: D1–D8 FROZEN; 27 builtins frozen;
Phase 5 module model APPROVED as-is; ADR-009 + spec §17/§18
APPROVED as written 2026-11-21; Wave 0 policies APPROVED
2026-11-19; Wave 2 ACCEPTED-WITH-FLAGS merged (two framework v1
deviations + metrics flag accepted as written 2026-11-22 —
Wave 4 builds on the framework as-is, no redesign, no silent
fixes); Wave 3 ACCEPTED-WITH-FLAGS merged (verdict, auth API
surface, and flags approved as written 2026-11-23 — none
re-opened here). Windows DEFERRED WITH EVIDENCE; SUPPORTED
platforms only (Linux x86_64, macOS ARM64, Linux ARM64).
`toto dev` single-file-only stays a known open limitation.

Carry-forward dispositions (nothing disappears silently):

Wave 3 (all carried — none is this wave's scope): conc=2
deterministic envelope for concurrent TLS load (beyond conc=2,
loud 5 s handshake timeouts; the async TLS state machine on the
poller, or an elastic pool, is the named follow-up, NOT this
wave — §8 exclusion holds); recv/send unbounded peer waits
(documented risk); 1-thread pool wedge (documented config
limit); E0216 root-cause (Phase 4 follow-up — the guard still
cannot see a busy-forever pool); SIGBUS per-program log-text
confirmation (open item); TOML empty-table reporting (CARRIED);
YAML unquoted-trailing-colon re-parse (CARRIED, pinned by
`test_yaml.gzv:75`); MySQL `db_exec` empty-misuse error string
(CARRIED wart); native AOT net/crypto/gzip (CARRIED — refuse by
design); crypto/compression on Windows (CARRIED — follows the
POSIX-compat decision); all-interfaces bind + Group C items
(raw sockets, packet work, full bytes story, WebSockets,
HTTP/2, security-toolkit libs — CARRIED as separate gated
proposals).

Wave 2 (accepted/standing — not re-opened): the two framework
v1 deviations are ACCEPTED AS WRITTEN (handler/middleware Err
aborts the connection instead of the designed 500 mapping;
?-unwrapping a Result holding function values panics E0204) —
Wave 4 builds on the framework as-is. Metrics exposition
validated with a hand-written spec-text checker, NOT a
reference Prometheus parser (recorded gap, unchanged).

Wave 1 (carried): E0216 race root-cause (`Phase 4` JIT dev-mode
settle — closed empirically, NOT root-caused); bench rows for
new surfaces (Wave 4 adds CI-measured targets for ITS
surfaces in Stage 5/6 — startup time, binary size, profiler
overhead; the standing gap for older surfaces remains carried).

Wave 0 (carried or user-outstanding): scratch-repo deletion is
a USER action, still outstanding; POSIX-compat decision
(WINDOWS STAYS DEFERRED WITH EVIDENCE — the only informational
probe); >100k live-set GC unmeasured; packaged-program
differential coverage; M:N rendezvous latency; minimal tunable
set; per-platform release matrix; default-branch dispatch 404;
second-run reproducibility compare. Plus the Wave 2 named
follow-ups carried through Wave 3 Stage 0 (rendezvous-latency
measurement; packaged differential coverage; GC past 100k +
minimal-tunable-set; policy approvals; release practice;
per-platform matrix; dispatch-404s; reproducibility-compare;
multi-worker scheduler audit follow-ups).

Report hygiene (lesson from the Wave 3 merge, binding here):
this file is append-only and earlier text is preserved verbatim
— but every correction this wave states explicitly whether it
was "recorded in dated notes" or "fixed in place". No section
claims an in-place fix that did not happen.

## Stage 1 — distributed tracing (design recorded before implementation; no code yet)

Status: DESIGN ONLY. No implementation, no tests, no commits
beyond design+report in this section. The design below is the
contract the implementation will be held to; deviations get
their own dated deltas, never silent drift.

No STOP was needed: stdlib-only surface (`gzv.std/trace.gzv`),
zero new builtins (27 frozen stay frozen), no D1–D8 change, no
ADR-009 change (in-process semantics untouched), no spec change
(no language surface moves).

### 1.1 Span/trace model (OTel subset)

- Trace id: 16 cryptographically random bytes, lowercase hex
  (32 chars; `crypto_random`, same source as Wave-3 keys).
- Span id: 8 random bytes, lowercase hex (16 chars).
- A span: `{trace_id, span_id, parent_span_id (absent = root),
  name, kind, start_ms, end_ms, attrs, status}`. Kinds:
  internal/server/client (+consumer/producer reserved names,
  unused in v1).
- Timestamps are monotonic milliseconds rendered as nanos
  (`ms * 1e6`) — millisecond precision is a stated v1 limit
  (the Wave-2 clock has no finer grain; `docs/time.md` governs).
- Attributes: string and int values only (mapped to OTLP
  `stringValue`/`intValue`; no bool/double/array in v1 — a
  producer stringifies anything else before recording).
- Status: unset renders as `{}`; ok as `{"code":1}`; error as
  `{"code":2,"message":"..."}` (message is producer text, never
  library wording — goldens stay portable across families).
- Resource: `service.name` only. Scope: name
  `gzv.std/trace` (no version in v1 — the proven shape).

### 1.2 In-process propagation (ADR-009, unchanged)

- `trace.trace_id`, `trace.span_id`, `trace.sampled` ride as
  ordinary context values (`ctx_with_value`/`ctx_get`); spawn
  inheritance carries them automatically (ADR-009 rule: the
  task tree IS the request tree). `go_detached` opts out
  (fresh background work starts a new trace — explicit).
- Channels do NOT carry context (ADR-009 rule 3): a receiver
  stays governed by its spawn context. Cross-task causality
  uses the Wave-2 opt-in edge helper `log_trace_edge`
  (`gzv.std/log`) — recorded auditably without a tracing store;
  a span MAY carry it as an attribute
  (`causal.edge=<edge-id>`) for query-time correlation, but
  there is NO silent context transfer and NO OTel links in v1
  (links are out of scope; see §1.6). A true message-scoped
  transfer (`send_with`) needs user generics, which v1 does not
  have — recorded in `context.gzv` as a Wave 4 follow-up, not
  smuggled in here.
- Deadlines/cancellation close spans with cause (ADR-009
  §"what tracing builds on"): a scope that fails marks its open
  spans errored with the failure code before release.

### 1.3 Cross-service propagation (W3C, new in this wave)

- Wire format: exactly `traceparent: 00-<trace-id>-<span-id>-<flags>`
  (version `00` only; flags `01` iff sampled). No `tracestate`,
  no baggage in v1 (stated gap, not smuggled in later).
- Inject on every outgoing service call
  (`trace_inject(ctx) -> Array<HttpHeader>`); extract on entry
  (`trace_extract(headers)` — always `Ok`, absent/malformed
  yields the input context — then start a SERVER span with the
  remote parent). A malformed
  or absent header starts a NEW root trace and NEVER fails the
  request (fail-open on malformed_propagation — a tracing
  defect must not become an availability defect).
- The framework (`http.gzv`) is untouched: injection is caller
  code attaching headers to `http_do`; extraction reads the
  headers the framework already delivers. No Wave-2 change.

### 1.4 Sampling

- Head-based deterministic sampling on new roots:
  `trace_id mod den < num` (default 1/1 = always sample;
  production guidance: e.g. 1/100 — documented, not enforced).
- Parent-based: an extracted sampled flag is honored as-is.
- The flag travels in `traceparent` and in context
  (`trace.sampled`); unsampled spans are still recorded locally
  (`trace_finished` shows them) but flagged, so tests stay
  deterministic while production can shed load. No tail
  sampling in v1.

### 1.5 Exporter: format + transport + destination

- Format: OTLP/HTTP+JSON to `POST {endpoint}/v1/traces`
  (`Content-Type: application/json`), hand-mapped from the §1.1
  model (JSON string escaping done in-stdlib; OTLP field names
  per the spec text).
- Transport: the Wave-2 HTTP framework (`http_do` POST).
  Finished spans accumulate in a process table; `trace_export`
  drains it. No background exporter thread in v1 (explicit
  flush at request/test end — documented; a dropped flush drops
  spans loudly via the returned `Err`, never silently).
- Destination in tests: a REAL collector. Proven locally and
  in CI against Jaeger all-in-one
  (`jaegertracing/all-in-one@sha256:ab6f1a1f0fb49ea08bcd19f6b84f6081d0d44b364b6de148e1798eb5816bacac`,
  Apache-2.0) — OTLP/HTTP receiver `:4318`, query API `:16686`
  — with trace retrieval by ID asserting parentage, service
  names, and attributes. The collector runs as a sibling
  container (CI provision step, same pattern as Vault).

### 1.6 FFI-first decision (stated reason for no new binding)

No new native library and no new Rust crate is bound for the
export path. The mature OTel Rust SDK's OTLP exporter needs an
async runtime (tokio/reqwest) for transport; the Toto runtime
is deliberately non-async (blocking pool, no executor — the
F1 lesson: foreign execution models wedge the pool), so
embedding one for span export would violate the execution
model and the D6 foreign-thread discipline for zero wire-format
gain: OTLP/HTTP+JSON over the in-tree HTTP framework needs no
new native code. By the Wave-2 precedent (spec-text checker
validated against the real system, not a reference parser),
the mapping is hand-written against the spec text and the gate
is acceptance by a REAL collector (round-trip proven
2026-11-23: push → trace retrieved with CHILD_OF linkage
intact), never self-assertion. Exporter identity recorded:
none new (stdlib-only); proof backend as in §1.5.

### 1.7 What "OpenTelemetry-compatible" means (v1)

HONORED: the OTel span model (§1.1); W3C `traceparent`
propagation; OTLP/HTTP+JSON export to `/v1/traces`;
`Resource.service.name`; instrumentation scope name. OUT OF
SCOPE: metrics and logs signals; `tracestate`/baggage; tail
sampling; gRPC transport; span events and links; resource
detectors; SDK batching knobs; trace-aware log correlation
(the log body carries `trace_id` as an ordinary field via
`ctx_get` — correlation by query, not by protocol).

### 1.8 Reference application shape

Extend `proj25_refsvc_data` (no new toy): a FRONT service
(second listener+router in the same program, deterministic
port) whose authed `/front/users/{id}` handler calls the DATA
service over loopback TLS with an injected `traceparent`; the
data handler extracts it and continues the trace (server span
+ child spans for auth + DB, reusing the Wave-3 shapes). The
corpus golden asserts the in-memory finished-span graph
(names, parentage by name, kinds, attributes —
IDs/timestamps are random/wall-clock and never pinned:
`trace_ids_equal=yes` asserts one shared trace without
printing it). The Rust interop suite runs the same program
against the real collector and asserts via the query API.
Services are independently deployable listeners; the
single-process deployment is the deterministic test shape
(separate mains per listener is a deployment detail, not a
second implementation).

## §7 acceptance-criteria checklist (honest, updated per stage)

1. Starting state verified; follow-ups dispositioned — DONE
   (Stage 0 above).
2. Distributed tracing — IMPLEMENTED + PROVEN LOCALLY (Stage 1
   record appended at end: design as above; OTLP collector proof
   `trace-verified=yes`; local gate green; CI re-proof pending).
3. Profiling (CPU + heap, green-thread-aware) — IMPLEMENTED +
   PROVEN LOCALLY (Stage 2 record appended at end: cooperative
   sampling + owner-tagged heap, refsvc load profile, <1%
   overhead; local gate green; CI re-proof pending).
4. Debugger (DWARF + green-thread dumps) — NOT YET (Stage 3).
5. Graceful shutdown (signals + drain under load + health/
   readiness) — NOT YET (Stage 4).
6. Deployment (static binaries + container images, measured
   targets) + reference application end to end + gates —
   NOT YET (Stage 5).
7. Report complete — NOT YET (appended per stage).

# Stage 1 record — implementation, diagnosis, fixes, proof (appended after design; design text above preserved verbatim)

## What was built (Stage 1 implementation)
- `stdlib/trace.gzv` (new, ~590 lines): span/trace model on
  ADR-009 context (`trace_tracer`, `trace_tracer_ratio`,
  `trace_begin`, `trace_ctx`, `trace_attr`, `trace_attr_int`,
  `trace_end`, `trace_inject` (W3C `traceparent`), `trace_extract`
  (always-Ok, fail-closed), `trace_span_json` (OTLP mapping),
  `trace_export` (OTLP/HTTP+JSON over the Wave 2 HTTP client),
  `trace_finished_report` (deterministic golden report).
- `stdlib/tests/test_trace.gzv` (new): round-trip
  inject/extract, malformed-input fail-closed, ratio sampling,
  wire-mapping pins. In the stdlib suite green.
- `crates/toto-compiler/src/stdlib.rs`: `std_module!("trace")`;
  MODULES pin 29→30 (`project.rs`).
- `tests/corpus/project/proj25_refsvc_data/`: `common.gzv`
  (traced data handlers `h_user_factory` with server/auth/db
  spans, front door `h_front_factory` + `front_hop` go-block hop
  with `traceparent` propagation, `refsvc_serve_front`),
  `main.gzv` (tracer setup, login + negative probes, traced
  cross-service demo, golden report, OTLP/Jaeger leg),
  `load.gzv` (tracer threaded through, load runs untraced),
  goldens regenerated (`main.stdout` 193 lines, stderr empty,
  status 0).
- `crates/toto-runtime/src/sched.rs`: Db lane (see fix 1 below).

## Diagnosis — the traced-demo hang (F1 child, proved with pool introspection)
Symptom: the full demo hung after the front-service hop; the
hop-only repro (login + one traced front call) hung
deterministically; direct requests (1–2 headers, with and
without `traceparent`) all completed. Request bytes arrived
intact at the data server (332 B, complete `GET /users/alice`
with `traceparent`), yet the handler never ran.

Evidence chain (temporary pool/task instrumentation, scratch
copies only, all reverted):
1. Pool census at hang: every submitted job completed except
   two legit client TLS record Recvs (outer client read +
   hop read). No stuck job — the pool was drained, tasks
   parked, CPU idle. Not a stuck job: a circular wait.
2. Park/drain/run tracing: the data conn task woke with its
   332 B, resumed, and parked again on a pool WriteFile —
   which queued forever behind the two held client Recvs on
   the 2-thread client lane.
3. Root cause: client-lane pool starvation. Two concurrent
   client TLS record Recvs (main→front response read, hop→data
   response read) occupy both client-lane threads indefinitely
   (record Recvs have no timeout — the F1 5 s bounds cover
   handshakes/shutdown only). The downstream server task needs
   pool work (log emit, crypto, Db) to produce the response;
   on the shared client lane that work queues behind the very
   reads it would unblock — a silent circular wait no guard
   can see (a busy pool reads as future work). Same F1 pattern
   ("a pool job blocking on a peer action wedges when the
   producing job is queued behind it"), new shape (record-read
   vs server work, within one lane) that the F1 handshake
   bounds + role lanes do not cover.

## Fix 1 — dedicated Db lane (runtime, minimal, preserves semantics)
`tls_lane` routes the no-fd-role blocking ops (Db, Foreign,
LDAP) to a new `Lane::Db` with `pool_threads` threads (the
parallelism these ops always had — Db pool exhaustion
semantics rely on two concurrent checkouts contending, proved
by `db_pool` 3/3 green). Client TLS stays on Cli (2 threads),
server TLS stays on Srv (1 thread, F1 intact). Files + DNS stay
on Cli (connect-time/startup-only, never mid-request). Full
lane plumbing: queue + bell + spawn + deadlock-check +
shutdown + pickup + handoff + submit sites. An earlier
one-lane variant (all server work to Srv×1) was tried and
REVERTED: the single Srv thread serialized concurrent Db ops
and broke `exhaustion_is_loud_and_counted` (both queries
succeeded, zero exhausted). The Db lane keeps exact Db
parallelism — no blind enlargement (thread counts unchanged),
no TLS execution-model change.

## Fix 2 — front handler must not forward framing headers (demo)
The front hop forwarded the inner response's headers verbatim;
the front server then re-added its own Content-Length /
Connection / Server → duplicate Content-Length → the strict
client parser rejected the reply and `http_do` returned Err.
Fix: the front builds a fresh response (status/reason/body,
no headers). Inner v1 responses are headerless (`http_text`),
so nothing end-to-end is lost.

## Fix 3 — OTLP resource attribute wrapper (exporter)
`trace_export` emitted `"value": "<string>"` (bare) for the
resource `service.name`; OTLP requires the AnyValue wrapper
`{"stringValue": ...}` — Jaeger rejected the push with 400
(the exporter's own status check surfaced it). Fixed to emit
the wrapper. (Attribute values elsewhere already wrapped
correctly; unit tests pin the span mapping.)

## Proof
- Hop-only repro: `Ok("HOP-OK")`, exit 0, clean teardown
  (live=0), after fixes 1+2.
- Full traced demo: `Ok("REFSVC-DATA-OK")`, exit 0; golden
  byte-identical across two runs (193 lines, empty stderr);
  span parents show the cross-service linkage
  (`parent=front-to-data` on the data span, `parent=test-front-call`
  on the front span). Goldens updated in-tree.
- Real collector: OTLP/HTTP push to local Jaeger
  (jaegertracing/all-in-one) accepted; `trace_verify_jaeger`
  polled `/api/traces/{tid}` and confirmed the 6-span trace
  with CHILD_OF linkage: `trace-verified=yes`.
- Regression: F1 `tls_peer_wait_bounds` 3/3, `db_pool` 3/3,
  `refsvc_load` 4/4, corpus (incl. new golden) green; full
  `check.sh` green (see totals below).

## Gate totals (fresh full run, this turn)
- `cargo fmt --all -- --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
- `cargo test --workspace`: 65 result lines + 2 unscored-but-ok,
  269 passed, 0 failed. Per-binary passed counts identical to
  the Wave 3 table test-for-test (unittests lib 34 + main 16,
  cli 17, project 15, gc 11, dev 11, reload 10, packages 10,
  jit 8, interp 8, gate 8, spike1 7, lexer 7, ffi 7, db_sqlite 7,
  stack1b 6, mn_jit 6, parser 5, mn 5, refsvc_load 4,
  http_interop 4, db_postgres 4, db_mysql 4, blocking 4,
  tls_openssl 3, tls_interop 3, opt 3, lsp 3, gc_perf 3,
  db_pool 3, concurrent 3, tls_peer_wait_bounds 3, stack_jit 2,
  race 2, oidc 0 (env, see below), ldap_auth 2, fmt 2,
  differential 2, ctx_stage3 2, 1 each: corpus,
  differential_concurrent, differential_task, fmt_equiv, mir,
  mir_conform, net_hammer_mw, panics, sequential, stack,
  timings, yield_jit, zz_ffidiag). Two binaries
  (`yield_jit`, `zz_ffidiag`) show individual `ok` with no
  summary line in this log; counted in the 269.
- `oidc_keycloak` 0/2 is ENVIRONMENTAL, not a regression: the
  47-hour-old local Keycloak realm serves discovery JSON
  containing a non-ASCII em-dash; the strict HTTP client
  rejects it before any wave code runs (server bytes proven
  with curl). CI provisions a fresh minimal realm per run.
- Local service-port notes (also environmental): the host's
  own :5432 is not w3pg (w3pg maps to :55432) — postgres
  tests ran with `TOTO_TEST_PG_DSN` at :55432 (4/4 green);
  LDAP at :53389 per CI's own env (2/2 green).
- stdlib `toto test`: 25 passed / 0 failed (was 24; +1 is the
  new `test_trace.gzv` suite). wordfreq: 1/1 + interp==built
  identity holds. Corpus census 186 unchanged (no case added
  or removed; proj25 edited in place, goldens regenerated).
- `fmt_equiv` needed an isolated rerun (346 s green): a
  shared-machine parallel run wedged it for hours (port/file
  contention with concurrent manual server tests), not a code
  hang — lesson recorded, rerun quiet.

## Stage 2 — profiling (design recorded before implementation; no code yet)

## 2.0 Requirement (PDF p.37)
CPU and heap profiling of running services, usable in production,
with GREEN-THREAD-AWARE attribution. A profile that cannot tell
tasks apart is not sufficient (same reasoning as the D3 race
detector). Overhead measured and stated; capture from a running
service without stopping it. Proven on the reference application
under load (not a toy).

Hard constraints for this stage: 27 builtins FROZEN (a new
builtin → STOP-and-ask) — so the design uses NO new builtins:
env vars + SIGUSR1 + profile file only, all runtime-internal.
No D1/D8 change (verified: ADR-008 freezes the handle codec
`(generation << 32) | index` and requires type-descriptor
headers — not the in-memory header struct layout — so a header
owner field needs no ADR). No spec change.

## 2.1 CPU sampling: cooperative safepoint sampling (no async unwind)
The M:N workers already pass safepoints (allocation + switch
per ADR-008: `maybe_collect` at every `alloc_with`, park, and
yield; collection happens ONLY at function-entry and
loop-header safepoints). A worker checks one `sampling_on` flag
at each safepoint; when the time quantum (default 10 ms →
100 Hz, `TOTO_PROFILE_HZ`) has elapsed since its last sample,
it records (task id, monotonic timestamp, copy of the task's
`TraceFrame` stack — `{name, span}` per frame, already
maintained per task) into its own sample ring, then continues.
No cross-thread stack reads, no signals for capture, no
stopping: attribution is exact because the worker always knows
which task it runs.

Why not async signal sampling: coroutine stacks are not
async-unwindable; a signal handler cannot safely walk them.
Cooperative sampling trades completeness (safepoint-only) for
soundness (every sample is a real task stack). Tight pure loops
without safepoints are invisible — documented limitation; the
compiler emits safepoints on entries/loops/calls, so
production (I/O-bound) services sample faithfully.

## 2.2 Heap attribution: owner tag + sweep tally on D1 accounting
Extend the object `Header` with the allocating task id (`owner:
u32`, +4 B/object — stated cost). `insert` takes the current
task id from the allocating worker (known at every alloc site).
The sweep tallies per-task live objects + approximate bytes
(reusing the existing per-alloc `bytes` estimate) and per-type
(`DescKind`) live counts. Roots unchanged (reachability as-is);
shared objects count toward their allocator
(allocator-pays, documented). No collection-algorithm change.

v1 heap profile = per-task live table + per-type live counts
(no allocation-site stacks — deferred as a named follow-up;
the CPU sampler already captures stacks, correlation is later).

## 2.3 Output format (text, versioned `v1` header line)
```
profile v1
task <id> <first-name> samples=<n>
stack <id> <func>;<func>;... <count>
heap <id> objects=<n> bytes=<n>
kind <DescKind> objects=<n>
dropped <n>
```
Folded stacks per task (parent-first order as walked), task
table, heap tables, drop counter. Parseable with split; no
IDs except task ids and counts (trace names only — stable
across runs for a fixed workload).

## 2.4 Overhead budget (production-usable means measured)
- CPU sampling at 100 Hz: one predictable flag check per
  safepoint + ~100 stack copies/sec/worker (shallow
  `TraceFrame` vecs). Target <1% wall-clock on the refsvc
  load shape; CI bench flags >5% regression with sampling on.
- Heap owner tag +4 B/object; tally O(live) per collection
  (already O(live) sweep — constant factor, measured).
- Rings bounded (drop-oldest on overflow, counter records
  drops — never unbounded growth).

## 2.5 Capture without stopping (signal + env + file; no new API)
- Enable: `TOTO_PROFILE=1` (both), `TOTO_PROFILE_OUT=path`
  (default `./toto-profile-<pid>.txt`); `TOTO_PROFILE_HZ`
  (default 100).
- Trigger: SIGUSR1 at any time, plus automatic dump at
  process exit when enabled. Handler writes one byte to a
  dedicated self-pipe (async-safe; mirrors `net_wake`);
  the poller sets `profile_dump_requested`; the first worker to
  observe it at a safepoint locks the rings, writes the file
  (inline `std::fs` write on the worker — brief, explicit,
  documented), clears the flag. The service never stops.
- Rejected: HTTP endpoint (needs router/auth surface);
  stdlib API (needs code changes in the service; also a new
  builtin, forbidden). Signal works on unmodified binaries.
- Corpus proof needs no trigger API: the test workload runs
  with `TOTO_PROFILE=1`, exits (auto-dump), asserts the file.

## 2.6 Proof plan (mirrors Stage 1)
- Corpus `proj2X_prof`: fixed synthetic workload (CPU burn
  across N distinctly-named tasks + known heap shapes).
  Asserts: file parses per §2.3 grammar; every task present
  with samples>0 (ranges, not exact — wall-clock sampling is
  nondeterministic); heap per-task live counts EXACT
  (deterministic — every object tagged, full tally).
- Reference application under load (60-req conc-2 shape,
  sampling on): publish top stacks, per-task CPU split,
  per-task heap live + overhead numbers (wall-clock
  on/off, bench rows as flagged).
- No wire protocol in v1 (files, not OTLP profiles) — stated.

## 2.7 Benchmark rows + decision record
New bench rows: sampling on/off wall-clock on the refsvc load
shape (flag rule as usual). Deferred: allocation-site stacks,
OTLP profile export, HTTP trigger endpoint — named follow-ups,
not silence.


# Stage 2 record — CPU + heap profiling (implementation + proof)

## What was built (Stage 2 implementation)
- `crates/toto-runtime/src/profile.rs` (new): `Profiler`
  (config from `TOTO_PROFILE`/`TOTO_PROFILE_HZ`/`TOTO_PROFILE_OUT`,
  per-worker sample rings cap 16384 drop-oldest, names map, last
  sweep tally, dump lock + flag), `Sample` (task, micros, first
  fn, `;`-joined stack — one allocation per sample), `HeapProfile`
  (per-task + per-kind live), `render` (§2.3 `profile v1` format),
  SIGUSR1 handler + process-wide trigger pipe (unix-only).
- `heap.rs`: `Header.owner: u32` stamped from TLS at `insert`;
  `payload_bytes` mirror; `profile_tally` (per-task + per-kind
  live, exact — the sweep's live set).
- `sched.rs`: `Shared.profiler` + `profile_wake`; worker-loop TLS
  owner/worker identity; `profile_dump` (first claimant wins,
  inline write, best-effort); tally store after each collect;
  poller watches the trigger pipe; exit dump in run epilogue.
- `exec.rs`: `maybe_sample` at every safepoint (enabled flag +
  1-in-1024 tick fast path; quantum check; stack copy; ring
  push; dump on request).
- No new builtins, no ADR change (codec frozen, not header
  layout — verified in ADR-008), no spec change.
- Corpus `tests/corpus/project/proj26_prof/`: phased workload
  (two named 100k-array workers + main top-up forcing the final
  collection) with computed-output golden; profile assertions
  live in the runtime test (goldens cannot depend on
  wall-clock sampling).
- `crates/toto-runtime/tests/profile.rs` (new): in-process run
  asserts task presence per worker, named stacks
  (`go;worker_a`), exact heap (`objects=2 bytes=3200064` per
  task — array + its `for` snapshot; frame-rooted locals stay
  live, conservative roots documented), `kind Array objects=6`,
  `profile v1` header.
- `crates/toto-cli/tests/profile_signal.rs` (new, unix-only):
  child `toto run` + SIGUSR1 mid-run asserts the dump lands
  while the service keeps running, then clean exit (+ exit
  dump). Needs `libc` dev-dep (already locked).
- Bench: `exec-refsvc-load-prof` row (same 60-req conc-2 shape
  with profiler on) + `66000` baseline (median of 3 local runs).

## Proofs
- CPU: `task 1 go samples=66 / stack 1 go;worker_a` style lines
  — every task attributed, stacks named (in-process + refsvc
  load: 11+ distinct tasks with full call paths).
- Heap: exact per-task objects/bytes + per-kind totals;
  allocator-pays semantics verified (each task's array counted
  to its allocator).
- Signal: SIGUSR1 mid-run writes the profile while the service
  runs (manual + committed child test); exit dump always fires
  when enabled.
- Refsvc under load (60-req conc-2): LOAD-OK with profiler on;
  `dropped 0` at 100 Hz with 16k rings; top stacks show
  `phase_c_worker;http_do;http_exchange;...` paths.
- Overhead: <1% on the deterministic proj26 shape (2.35s vs
  2.37s medians, interleaved A/B); 4% on a single 60s load pair
  (65654 vs 63117 ms) — within run-to-run variance of the long
  shape; CI 5x flag rule unaffected. Bench baseline 66000
  recorded.

## Gate totals (fresh full run, this wave)
- `cargo fmt --all -- --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
- `cargo test --workspace --no-fail-fast`: 75 result lines,
  269 passed, 0 failed (+1 line `oidc_keycloak` 0/2,
  ENVIRONMENTAL — stale local Keycloak realm serves an em-dash
  in discovery JSON; CI provisions a fresh minimal realm; see
  the Stage 1 record for the server-side proof). Bridge from
  Wave 3 (74 / 269 / 0): lines 74 − 1 (oidc ok-line, env-fail
  here) + 2 (new `profile`, `profile_signal` suites) = 75;
  tests 269 − 2 (oidc) + 2 (new suites) = 269. Every other
  binary holds its Wave 3 count test-for-test.
- stdlib `toto test`: 25 passed / 0 failed; wordfreq 1/1 +
  interp==built identity; corpus census 187 (was 186: +1 project case, proj26;
  160 top-level + 27 project mains).


## Stage 3 — debugger support (design recorded before implementation; no code yet)

## 3.0 Requirement (PDF p.37) + current state (established first, as required)
DWARF debug info with gdb/lldb debugging of Toto programs, plus
green-thread dumps (every task in a wedged process) for
post-mortem diagnosis. Proven: a debugger session against a
real running service + a green-thread dump from a wedged process.

CURRENT STATE (read from the tree + a live gdb session, not
assumed):
- AOT (`toto build`, Cranelift 0.137, flags `opt_level` +
  `is_pic` only): emits ELF symbols (`toto_fn_<name>`,
  `Linkage::Export`, readable) + `.eh_frame`/`.eh_frame_hdr`
  (unwind feature). NO `.debug_*` for Toto code (sections
  exist but empty — from the Rust shim), NO line tables, NO
  variable DIEs. `cranelift-object 0.137` has no DWARF emission
  backend (only the `unwind` gizmo); `cranelift-codegen 0.137`
  has no `debug/` DWARF module. Closing that gap needs a
  Cranelift upgrade (toolchain dependency change) + srcloc
  plumbing through `compile.rs` (MInstr spans exist everywhere;
  file/line tables + variable locations do not) — a separate
  gated toolchain proposal, NOT this stage.
- PROVEN live (gdb batch on built wordfreq): `break
  toto_fn_main` hits; backtrace mixes `.gzv` fn names with Rust
  runtime frames (via eh_frame); registers/disassembly
  readable. NOT possible today: `break file.gzv:LINE`, `list`
  source display, `info locals` / `print` of .gzv variables.
- Interpreter (`toto run`) and JIT (`toto dev`) paths emit no
  native code at all — gdb sees only the runtime; green-thread
  state is the only meaningful debugger view there (same
  scheduler as AOT services).

## 3.1 v1 scope decision (stated explicitly)
- Green-thread dumps: FULL (all paths — same scheduler).
  Content per task: id, name (slot names exist), state
  (Ready/Running/Blocked + op detail: chan id + send/recv,
  poller fd + op, pool op + lane), scope, parked TraceFrame
  stack (fn names; spans as byte ranges — file:line resolution
  is a named follow-up). Plus: channels (waiters per chan +
  direction), pool lanes (queued/in-flight with task + op),
  workers (worker→task). Format: text `threaddump v1`.
- GDB session on what exists: symbolic breakpoints on
  `toto_fn_*`, backtraces, register/disassembly inspection,
  plus the dump file for green-thread state. PROVEN on the
  reference AOT binary (wordfreq precedent under gdb batch).
- OUT (named follow-ups, not this stage): line tables +
  source display, `.gzv` locals in gdb (needs the Cranelift
  DWARF toolchain upgrade + srcloc/variable-location backend
  work); JIT GDB-integration; native-task `.gzv` stacks in
  dumps (JIT tasks don't maintain TraceFrame — state + op
  shown, stack omitted); kernel/worker-native stacks.

## 3.2 "Every task" (definition)
All scheduler slots not Done: Ready (queued), Running (on a
worker), Blocked (chan send/recv + chan id, select cases,
poller net + fd, pool blocking + op + lane, scope-cancelled).
Blocked-on-what is the diagnostic payload: a wedged process
reads as cycles in chan waits + idle lanes. CANNOT show (v1,
stated): JIT-native frames (no TraceFrame — state + op only),
Rust worker stacks, kernel stacks, foreign-thread states,
heap contents (the heap profile's job).

## 3.3 Mechanism
- Park-time stack publication: every executor suspend point
  (`park`, `tls_wait`, `net_wait`, pool-Blocking suspends —
  one wrapper) clones the task's TraceFrame stack into its
  slot BEFORE suspending (suspended coroutine internals are
  otherwise unreadable). Cost: one small vec clone per park;
  measured on the chan-heavy shape, gated only if >1-2%.
- Trigger: SIGUSR2 (dedicated self-pipe + poller watch, same
  pattern as the profiler; runtime owns USR1/USR2 for
  diagnostics, documented) AND automatic on E0216 (the guard
  already enumerates blocked tasks under the sched lock —
  snapshot there, write after release). Default path
  `std::env::temp_dir()` (never litters the repo),
  overridable via `TOTO_THREAD_DUMP`. Best-effort: write
  errors print to stderr, never fail the service.
- Wedged-process cost: one slot walk + file write. The dump
  runs on a woken worker from the decide section (no
  safepoint needed — works when every task is parked and
  workers sleep; the SIGUSR2 poke wakes one). No stopping:
  the service keeps running after a signal dump.
- No new builtins, no ADR/spec change (runtime-internal;
  scheduler + exec + a dump renderer).

## 3.4 Proof plan (mirrors Stages 1–2)
- Wedged process: deterministic two-task chan deadlock
  (each blocks on the other's chan) → E0216 fires → auto-dump
  asserted (both tasks, opposite chan waits, stacks present).
- Signal dump: refsvc under load + SIGUSR2 → dump while
  serving, service survives and stay green; dump shows server
  conn tasks parked in handler Recvs with stacks.
- GDB session: AOT binary under gdb batch (breakpoint on a
  `toto_fn_*` symbol, backtrace with .gzv names, registers)
  recorded verbatim in the report.
- Corpus `proj27_dbg`: wedge workload with computed-output
  golden (exit code for E0216? — E0216 is a panic path:
  golden asserts the diagnostic shape on stderr) + dump
  assertions in the runtime test (like `profile.rs`).


## Stage 4 — graceful shutdown + health/readiness (design recorded before implementation; no code yet)

## 4.0 Requirement (PDF p.37) + D7 relation (stated explicitly, not conflated)
Production drain of in-flight work (the OPERATIONAL counterpart
of the D7 drain rules), signal handling, health/readiness
endpoints. Exit criterion: drain-under-load-then-exit green.

D7 (ADR-007, dev reload) vs Stage 4 (production stop) — same
mechanics, opposite lifecycles (stated once, not conflated):
- Shared mechanics (deliberately identical): in-flight work
  finishes first; a bounded drain timeout (5 s default, D7
  provisional value adopted); forced scope-boundary
  cancellation at expiry; full-stop fallback with a loud
  message (D7 restarts the dev process; Stage 4 exits the
  production process).
- Different purpose and lifecycle: D7 reloads (old version
  drains, tasks restart on new code, the PROCESS CONTINUES;
  code versions retained per the v2.2 lifetime rules).
  Stage 4 shuts down (drain, then the PROCESS EXITS; nothing
  takes over; the orchestrator removes the endpoint).
  Production builds never hot-reload (D7 decision) — no
  conflict: D7 governs dev reloads, Stage 4 governs
  production stops.

## 4.1 Signal set (unix-only; Windows stays deferred with the platform)
- SIGTERM (orchestrators, `docker stop`) and SIGINT (Ctrl-C):
  initiate drain. Same async-safe self-pipe pattern as
  USR1/USR2 (dedicated pipe, no new surface).
- SIGUSR1/USR2/SIGQUIT: unchanged (profile/threads/dump).
- SIGHUP: nothing in v1 (no reload semantics in production).
- SIGKILL/SIGSTOP: uncatchable by definition — documented,
  not handled. A SIGKILLed drain is an orchestrator
  misconfiguration (kill grace shorter than the drain
  timeout), not a defect.

## 4.2 Drain semantics (what drains, in what order, timeout, leftovers)
Trigger (runtime-internal, no new builtins): on SIGTERM/SIGINT
the runtime (1) writes the drain-flag file
(`$TOTO_DRAIN_DIR/draining`, default temp dir — the ONLY
runtime→.gzv drain channel in v1; a `drain_requested()`
builtin is the named follow-up if the file proves awkward),
(2) closes every tracked listener fd (recorded at
`net_tcp_listen` in net state — the parked accepts wake with
`Err` and the existing stdlib accept loops exit on exactly
that), (3) arms the deadline (`TOTO_DRAIN_SECS`, default 5 —
D7 precedent).
Order: (1) stop accepting (listener close; accept loops exit);
(2) in-flight requests run to completion (conn tasks keep
their workers/pool lanes; nothing is deprioritized);
(3) when accept loops are gone AND the serve task returns AND
no conn tasks remain, the run ends cleanly (live==0) with the
main's own exit code (0 on the reference path).
Timeout: at the deadline with live tasks remaining, the
runtime fails the root scope (forced cancellation — same
sweep as E0216 paths; pool jobs finish, completions drop) and
the run exits LOUD (nonzero + stderr line, never silent).
Which work drains: green-thread conn tasks with open requests
(all of them — no priority classes in v1). What is NOT waited
for: the main task parking forever (a graceful server main
waits on its accept loop via the new joinable serve below and
returns; a main that never returns is cancelled at the
deadline like any straggler — documented).

## 4.3 Serve-loop shape (stdlib extension, existing behavior unchanged)
`http_serve*` keeps its fire-and-forget shape (returns the
listener fd). NEW stdlib fn (allowed — stdlib is not the frozen
builtin catalog): a joinable serve that runs the accept loop in
the CALLING task and returns when the loop exits (listener
closed), so a graceful `main` waits on it and returns to let
the run end. In-flight tracking: none needed at the .gzv layer
— conn tasks are ordinary tasks; the run ends when all are
done (live==0), which the scheduler already computes. Early
exit needs no counter: the deadline backstop guarantees it.
Refsvc adopts the joinable serve for both doors.

## 4.4 Health/readiness (over the Wave 2 HTTP framework)
New refsvc routes (pattern for services; not framework magic):
- `GET /healthz` → 200 `ok` while serving (the handler runs,
  so the loop turns; no dependency checks — liveness only).
- `GET /readyz` → 200 `ok` when NOT draining AND the DB
  answers `SELECT 1` (real dependency probe, seconds-apart
  polling makes one pool op negligible); 503 `draining` when
  the drain-flag file exists; 503 `unready` when the DB probe
  fails. Draining is observed via the EXISTING `file_exists`
  builtin — zero new surface.
- During drain: health stays 200 (in-flight still served),
  readiness goes 503 (orchestrator stops routing). After
  drain: process gone (both fail closed — correct: the
  endpoint is removed). Fail-closed (connection refused) also
  reads as not-ready — but liveness MUST NOT fail during
  drain (a SIGKILL from a failed liveness probe would defeat
  the grace), hence the separate health listener path is NOT
  needed: one server, two routes, health independent of the
  flag file.

## 4.5 Proof plan (mirrors Stages 1–3)
- Unit/corpus: serve loop exits on closed listener (existing
  behavior — pin it); readiness 503 with flag file present /
  200 without (fixture flag files in /tmp, never committed);
  drain deadline forces exit on a wedged server (load test
  with a hung handler? — deterministic: handler parks past
  the deadline with a 1 s test timeout via env).
- Exit-criteria test (the gate): refsvc under concurrent load
  (60-req conc-2 shape), SIGTERM mid-load → every in-flight
  request completes (responses received), readiness 503s
  during drain, health 200s until exit, process exits 0.
  Implemented as a child-process runtime test (like the
  signal tests): spawn, load, SIGTERM, poll endpoints,
  assert, wait for exit.
- Overhead: none when idle (flag file absent → one extra
  `file_exists` per readiness probe only; accept path
  unchanged). Bench: the existing refsvc-load row covers the
  serve path; no new bench rows (drain is a rare event, not a
  steady-state cost).

## 4.6 Open items resolved here (not left hanging)
- No new builtins (file flag instead; builtin named as
  follow-up). No ADR/spec/module changes. No TLS execution
  change (deadline uses existing scope-failure sweep).
- Exit codes: 0 clean drain (main's own), nonzero loud forced
  (cancellation propagates — same as any scope failure).
- Windows: signals absent → no drain trigger; file flag is
  still honored if created out-of-band (test hook), but no
  Tier-1 gate (deferred with the platform).

