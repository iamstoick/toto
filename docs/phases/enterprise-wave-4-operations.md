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

## §7 acceptance-criteria checklist (honest, updated per stage)

1. Starting state verified; follow-ups dispositioned — DONE
   (Stage 0 above).
2. Distributed tracing — NOT YET (Stage 1).
3. Profiling (CPU + heap, green-thread-aware) — NOT YET
   (Stage 2).
4. Debugger (DWARF + green-thread dumps) — NOT YET (Stage 3).
5. Graceful shutdown (signals + drain under load + health/
   readiness) — NOT YET (Stage 4).
6. Deployment (static binaries + container images, measured
   targets) + reference application end to end + gates —
   NOT YET (Stage 5).
7. Report complete — NOT YET (appended per stage).
