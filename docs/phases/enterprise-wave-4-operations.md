# Enterprise Wave 4 report — operations

Status: STAGE 0 COMPLETE (starting-state verification + carry-forward
disposition, this turn). Earlier section text is preserved verbatim;
the report is appended stage by stage.

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
  (`trace_extract(headers) -> Context`, then start a SERVER
  span with the extracted span as remote parent). A malformed
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
2. Distributed tracing — DESIGN RECORDED (Stage 1 design
   above; implementation + proof pending).
3. Profiling (CPU + heap, green-thread-aware) — NOT YET
   (Stage 2).
4. Debugger (DWARF + green-thread dumps) — NOT YET (Stage 3).
5. Graceful shutdown (signals + drain under load + health/
   readiness) — NOT YET (Stage 4).
6. Deployment (static binaries + container images, measured
   targets) + reference application end to end + gates —
   NOT YET (Stage 5).
7. Report complete — NOT YET (appended per stage).
