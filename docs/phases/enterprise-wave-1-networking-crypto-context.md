# Enterprise Wave 1 report — networking substrate, crypto, context

Status: CONDITIONAL — NOT YET ACCEPTABLE (reviewed 2026-11-20;
criterion 4 awaits user approval regardless; see the dated
fix-forward section at the end for the red-gate closures).

Branch: `enterprise/wave-1-networking-crypto-context`, from `main`
at `4f88bf3`. This report is appended stage by stage (§6); the
acceptance checklist (§7 of the brief) is answered at the end.
**Two items need the user's eye at acceptance review: the socket
API design (spec §17, Stage 1) and the context ADR
(`docs/decisions/adr-009-context-propagation.md`, Stage 3)** —
both are flagged below and are not approved by inattention.

Starting state (Stage 0): the v2.5 PDF was verified before any
work — title page reads "Toto (.gzv) Implementation Roadmap v2.5
and Phase 0 Decision Doc (D1-D8)" and the document has 41 pages
(`pdfinfo`). `main` is at `4f88bf3`; CI run `44452237453` is
green on every job except the Windows probe job, which fails
visibly under `continue-on-error` (expected, informational —
Wave 0 disposition, unchanged). `check.sh` on the branch exits
0 (see Stage 5). No D1–D8 change, no frozen-ABI change, no new
builtin, no module-model or spec-semantics change: the spec grew
two amendment-record sections (§18 crypto, §19 serialization,
same standing as the §17 record) and five stdlib modules; the
27 builtins, `close`-is-reserved, and the Phase 5 module model
are untouched.

## Wave 0 carry-forwards — disposition

Each of the seven is closed with evidence, handed to a named
stage, or carried by name. None disappeared silently.

- (a) Packaged-program differential/Tier coverage (flag c) —
  CARRIED BY NAME. The differential harness stays single-file by
  construction; packaged coverage stays in `packages.rs`
  round-trips. New Wave 1 projects (`proj11/12/13/14`) and the
  concurrent hammer run in the CLI corpus runner and
  `fmt_equiv` (exact goldens), not in differential — same
  standing as before.
- (b) GC measurement past 100k live objects — CARRIED BY NAME.
  No re-measurement on target hardware in this wave; the
  `gc_perf` gate (p99 10 ms at ≤100k) stays green in CI.
- (c) M:N rendezvous latency accounting — HANDED TO STAGE 1.
  Accounted in the poller evidence below: rendezvous cost is
  scheduler coordination, outside the collector budget; parked
  waits rejoin through the same rendezvous, and the hammer runs
  (pressure config: threshold 1, collect at every safepoint,
  fixed seed) cross it constantly. No separate rendezvous
  latency budget is set — recorded follow-up (dedicated
  rendezvous-latency measurement), not a silent drop.
- (d) Minimal tunable set if the single GC threshold proves
  insufficient — CARRIED BY NAME. No evidence of insufficiency
  in this wave; `RuntimeConfig` is unchanged (no new knob, which
  would need STOP-and-ask).
- (e) Per-platform release matrix — CARRIED BY NAME. No real
  release, tag, or artifact in this wave (forbidden).
- (f) Release-pipeline dispatch on the default branch (404s for
  branch-only workflow files) — CARRIED BY NAME, untouched.
- (g) Second-run reproducibility compare — CARRIED BY NAME as
  stated in the brief (partially superseded: the `.4`/`.5`/`.6`
  dry runs showed byte-identical binaries across runs).

## Stage 1 — networking substrate (sockets, DNS, poller)

API design FIRST (spec §17, recorded 2026-11-19 — **flagged
for user review at acceptance**): sockets are opaque `int`
handles; payloads are `Array<int>` with elements 0–255 (the
Wave 1 byte carve-out only — the full bytes story stays a
separate gated proposal; if the API ever needs more,
STOP-and-ask); every function returns `Result<_, string>`
with `strerror` text; invalid ports/negative lengths are
runtime `Err`, never diagnostics; `listen`/`bind` are
loopback-only (no address parameter, no all-interfaces bind —
a recorded follow-up, explicitly not raw sockets); no
timeouts (timeouts interact with the context ADR, designed
not implemented here). Platform-neutral shape (no POSIX-only
API surface) so deferred Windows can join later without a
redesign.

Implementation: TCP/UDP sockets + DNS on the supported
platforms (unix-gated; Windows gets clean `E0301` via
`POSIX_ONLY`, as in Phase 5). Runtime-integrated async poller:
a network wait suspends the green thread and frees its worker
(`ToSched::Net` with owned plain data; the poller thread
touches neither heap nor scheduler state, runs nonblocking
syscalls with the net lock held, posts one completion per
wait). Boundary (stated rule, spec §17.5): a wait is
poller-eligible iff its fd can be nonblocking and its
readiness is `poll()`able; everything else (DNS via
`getaddrinfo`, files) goes to the dedicated blocking pool.
No timeouts in v1; a parked net wait is a suspension point
for scope cancellation (existing rule).

Correctness proof (exit criterion): `conc20_net_hammer`
(4 clients × 25 × 16-byte messages through one echo server,
every byte accounted on both sides, 9 tasks multiplexed on 1
worker — completion itself is the no-lost-wakeup /
no-starvation evidence), plus `proj11_net_echo` (TCP echo +
UDP echo) and `proj12_net_errors` (every error path is a
value, never a panic), all with exact goldens. Evidence:
`concurrent.rs` (pressure config — threshold 1, collect at
every safepoint, seed `0x9E37_79B9_7F4A_7C15`, 1 worker),
`fmt_equiv` (same pressure on formatted sources), CLI corpus
(default config), and manual `--workers 4` runs of `proj11`
(all `Ok`, exit 0). Seeds/workers/runs are these named
configs; the hammer was additionally stress-run (5×
`fmt_equiv` green).

Race found and fixed in this stage (broken-session code):
the deadlock detector checked the completion queue BEFORE
the net registrations while the poller posts BEFORE removing
its registration — a posted-but-undrained completion could
look absent and claim a false E0216 under GC-pressure timing
(reproduced as flaky `proj11` E0216 in `fmt_equiv`). Fix: check
pool, then registrations, then the queue LAST (any completion
whose registration is already gone was posted before the regs
check, hence is visible in the later queue check); the pool
needs no order (post + `in_flight` decrement hold both
locks). Post-fix: 5 consecutive pressure runs green, plus
3× full `fmt_equiv` + `concurrent` suites green.

Portability fix in this stage (broken-session code): the
hand-rolled `sockaddr_in` lacked the BSD `sin_len` byte, so
`bind`/`connect` failed on macOS, and the recvfrom peer
parser read the family from the wrong offset. Fixed by
building loopback sockaddrs via `getaddrinfo` (the kernel's
own layout on every platform) and parsing peers with an
Apple-aware family offset. `parse_numeric_host` (also
`getaddrinfo`-based) and `resolve_blocking` were already
portable and are unchanged.

ARM64 stack-overflow root cause and fix (2026-11-19): the
ARM Tier-1 probe crashed with SIGSEGV in `proj11` after DNS
resolve completed (6 addrs) and the pool completion drained
— always between drain and the first connect, with variable
delay. `gdb` on the runner caught it inside `str::split`'s
iterator nest under `call_builtin`, and the runtime stack
high-water probe read 64,512 / 65,536 bytes on x86_64 —
~1 KiB of margin: coroutine stacks are spec-fixed 64 KiB
(spec §1, ADR-002/ADR-008), and the interpreter chain
(`run_task` → `loop_frames` → `exec_instr` → `exec_builtin`
→ `call_builtin`'s merged ~24 KiB debug match frame → the
`split` adapter nest) barely fits on x86_64 but overflows
the guard page under ARM64's larger frames. Reproduced
locally by halving the stack default (same SIGSEGV in the
interpreter chain). Fix (spec-compliant — the 64 KiB budget
is unchanged): `call_builtin` is now a thin dispatch over
four `#[inline(never)]` group helpers (`str`/`num`/`arr`/`fs`
builtins), so only one group's temporaries are live at a
time; post-fix high-water is 60,584 bytes on x86_64 and the
ARM probe is green. No builtin behavior changed (same
messages/codes/spans; full suites green). All temporary
diagnostics from the hunt (T-NET tracing, gdb CI step,
corpus per-case lines) are reverted from the tree in the
cleanup commit — verified zero markers survive; the hunt
commits themselves remain in history (never rewritten, per
policy). Two related diffs ship BY DESIGN, not as leftovers:
57cf017's try-each-address connect loop (stdlib feature,
only its CI diagnostic path was reverted) and the
diag13/14/15 stderr goldens (expanded linked-symbol list +
`Array<int>` E0302 text from the Stage 1+2 feature commits,
not from any hunt commit).

Portability fix from the same audit (2026-11-19):
`set_nonblocking` OR-ed a shared `O_NONBLOCK=0x800`, which
is Linux-only — macOS needs `0x10`, so macOS sockets stayed
blocking and the loopback tests passed only because
loopback connects complete synchronously. Now a
per-platform constant (`0x800` Linux, `0x10` macOS,
never-exercised fallback elsewhere — non-unix net entry
points refuse with `E0301` before any fd exists). Validated
by the macOS Tier-1 probe; Linux behavior is unchanged
(cfg-gated).

GATING DEFECT (reported, not promised around): with 8
workers the 4-client hammer stalls intermittently (smaller
2-client shapes stall too) — posted completions sit undrained
while workers stop deciding; the 1-worker deterministic
harness is unaffected and consistently green. Evidence
(temporary scheduler tracing, since reverted): park/post/
drain correlation (unposted parks + posted-undrained
completions, then silence), `/proc` switch counts frozen
across all threads, futex waits with no GC rendezvous
involved, `sched` observed held at watchdog time. Repro:
`toto run --workers 8` on `conc20_net_hammer` (2-client ×
few-round variants stall as well; needs ≥2 concurrent
connections + multiple workers). TRACED TO the broken-session
scheduler/poller path (single-file net programs, which the
JIT differential skips, are unaffected). Single-worker M:N
proof stands (exit criterion as worded — deterministic
harness, seeded, exact goldens); parallel starvation coverage
is the recorded follow-up (multi-worker scheduler audit
with the `conc20` repro). No Waves 2–5 work was
done for this; no timeout parameters were added to net calls
(§17.4/§17.5 stand).

## Stage 2 — crypto via FFI (bind, do not implement)

Library: the system libcrypto — OpenSSL 3 on Linux
(`libcrypto.so.3`; observed OpenSSL 3.5.5 on this machine,
runtime-detected via `crypto_openssl_version()`, never
pinned in goldens — pinning would bake one runner's distro
lib into the suite while macOS ships LibreSSL), LibreSSL on
macOS — Apache License 2.0 (OpenSSL 3.x) / original
OpenSSL+SSLeay terms plus ISC parts (LibreSSL), recorded in
`docs/ffi.md` — both mature and audited. Rationale: the
only C library that
covers all six required primitives (hashing, HMAC, AEAD,
signatures, CSPRNG, X.509 verification) with an opaque-pointer
API — no caller-allocated structs, so the no-structs-by-value
rule holds by construction. Only long-stable APIs are used
(classic `EVP_Digest*`, one-shot `HMAC()`,
`EVP_aes_256_gcm`, `EC_KEY`/`ECDSA_sign`/`ECDSA_verify`,
`RAND_bytes`, `X509_STORE` verify, `OpenSSL_version`);
`BN_num_bytes` (a C macro in OpenSSL 3, not an exported
symbol — found by linking, fixed by using `BN_num_bits`) is
the only portability workaround, recorded in the code.
Ed25519 was deliberately NOT chosen (LibreSSL EVP support
uncertain on the blind macOS leg); ECDSA P-256 verifies
everywhere.

D6/FFI fit (see `docs/ffi.md` "Wave 1 allowlist extension"):
Toto calls 13 hand-written `toto_rt_crypto_*` foreign
functions (allowlist +13 with per-binding contracts; the
`Array<int>` extern shape was authorized 2026-11-19); the
Rust bridge (`cryptolib`) holds all opaque pointers itself —
Toto sees only byte arrays, ints, and strings. No retained
state (one-shot calls; rooting covers the call), no `dlopen`
(every symbol linked at build time), no binding generation.
Link model unchanged from Phase 5 (statically-declared
linkage; load-time `DT_NEEDED`, never runtime loading): the
`toto-compiler` build script symlinks the versioned system
libcrypto it finds into `OUT_DIR` so `-lcrypto` resolves
without a dev symlink (the recorded `DT_NEEDED` stays the
SONAME); `TOTO_CRYPTO_LIB_DIR` overrides the search.

Known-answer tests (every primitive, all passing):
- SHA-256: `"abc"`, empty, million-`a` (NIST SHS).
- SHA-512: `"abc"` (NIST SHS).
- HMAC-SHA-256: RFC 4231 TC1/TC2; HMAC-SHA-512 TC1 (oracle:
  Python `hashlib`/`hmac`).
- AES-256-GCM: fixed key/nonce/plaintext/aad vector pinned
  after cross-checking with Python `cryptography` (encrypt
  golden + decrypt round-trip + tamper/`Err` + bad-length).
- ECDSA P-256: fixed key/message/DER-signature vector
  cross-checked with `openssl dgst -verify` (verify 1,
  tampered 0, fresh-sign round-trip, bad-length `Err`s).
- CSPRNG: length/exactness/uniqueness properties + range
  `Err`s (functional, not KAT — stated honestly).
- X.509: `ca.pem`/`leaf.pem` (2020–2050), `expired.pem`
  (2015–2016), `other-ca.pem`/`other-leaf.pem` fixtures
  generated with Python `cryptography` and verified with
  `openssl verify` at generation (OK / expired / wrong-issuer
  negatives); garbage PEM is a fixed `Err` (never library
  wording, so goldens hold on LibreSSL too).
All vectors were confirmed with independent oracles (OpenSSL
CLI, Python `cryptography`/`hashlib`, RFC/NIST publications)
BEFORE pinning. Pinned in `proj13_crypto_kat` (6 `Ok`
markers) + `proj14_crypto_errors` (12 stable `Err` lines) +
`test_crypto.gzv` round-trips. No home-grown crypto anywhere.

## Stage 3 — context propagation ADR (design only)

Delivered: `docs/decisions/adr-009-context-propagation.md`
(**PROPOSED — user approval is the exit gate**). It decides:
context as an immutable library value; spawn inheritance
(the task tree is the request tree); no implicit channel
transfer (`send_with` opt-in for causality); cancellation as
scope failure via a Wave 2 `cancel_scope` hook (existing
`E0214` fail-fast discipline); deadlines as absolute
monotonic-ms values checked at suspension points, with the
clock itself a Wave 2 deliverable (shared with logging
timestamps — v1 has no wall clock). Alternatives rejected
with rationale (Go-style params, dynamic scoping, message
scoping by default, deferring until the clock exists,
relative-duration deadlines). States what logging (request
IDs, shared clock, flush-on-cancel) and tracing (span IDs,
spawn-inherited trees, `send_with` edges, deadline/cancel as
span closure) build on, and explicitly what it does NOT
decide (field list, clock source/precision, hook signature,
cross-process propagation). No implementation in this wave —
none exists.

## Stage 4 — serialization beyond JSON

- YAML (`gzv.std/yaml`, pure Toto): block
  mappings/sequences, spaces-only nesting, plain/single/
  double-quoted scalars, int/bool/null/float, comments,
  inline `- k: v`; NOT anchors/tags/flow/multi-doc/tabs/`\u`.
  Strata design (dedented blocks, re-parse + validate on
  access); canonical 2-space `yaml_stringify`, re-parse-stable
  and cross-checked with PyYAML. Pinned by `test_yaml.gzv`.
- TOML (`gzv.std/toml`, pure Toto): tables (incl. dotted),
  strings/ints (with `_`/signs)/bools/floats, scalar arrays,
  comments; NOT inline tables, arrays of tables, datetimes,
  multi-line strings, hex/oct/bin, `\u`. Strata design
  (absolute-path source fragments). `toml_parse` /
  `toml_stringify` round-trip; doc cross-checked with Python
  `tomllib`. Pinned by `test_toml.gzv`.
- protobuf (`gzv.std/proto`, pure Toto): varint (non-negative)
  + length-delimited bytes, field numbers 1–2²⁹−1, explicit
  `PbField`/`PbSchema` descriptors, no codegen; unknown fields
  skipped per the rules; groups/fixed32-64/negatives are
  clear errors. Golden wire bytes hand-computed for the
  stated `Person{id=300, name="hi"}` contract and verified
  against the encoding rules independently. Pinned by
  `test_proto.gzv`.
- Compression (`gzv.std/compress`, runtime over `flate2` /
  `miniz_oxide`, pure Rust): gzip RFC 1952. Interop proven
  against system utilities both directions (`gzip -n` oracle
  vector in-suite; our encoder output verified with system
  gunzip); compressed bytes never pinned; one fixed failure
  string. Pinned by `test_compress.gzv`.
FFI-first was honored where a linkable mature library exists
(crypto → libcrypto); for serialization no statically-linkable
C library exists in this toolchain (no static archives, no
perl/cmake for vendored builds), so pure-Toto code (auditable,
AOT-compatible) plus one pure-Rust crate was chosen — choice
and rationale recorded here and in spec §19.

## Stage 5 — gates and report (this section)

Full gate on the branch: `check.sh` exit 0 — 61 result lines,
224 passed, 0 failed (re-derived from fresh runs, verified
both workspace-wide and per-package with `cargo test -p`;
per-crate account: spikes 39 (ffi-smoke-test 7 +
gc-scheduler 8 + stack-switching 6 + hot-reload 10 +
cranelift 8), toto-cli 47, toto-compiler 41, toto-jit 68,
toto-runtime 29 — sums to 224 exactly; an earlier draft of
this report had the same total with a misattributed split,
corrected here), plus the stdlib suite 13/13 and wordfreq
build-identity. Corpus census: 42 status-0 + 4 status-1
runnable single-file programs (diag16 joined in Wave 1);
22 concurrent (conc20 joined in Wave 1); 91 static negatives
(diag17 joined in Wave 1); diag10 excluded; 4 project
corpora (proj11/12 net, proj13/14 crypto).

CI: run 44606812115 (check + release-gates + bench +
Linux ARM64 tier1-probe green; Windows probe informational
under continue-on-error — Wave 0 disposition, unchanged;
macOS tier1-probe verdict below). Bench suite: no new rows
(decision: crypto one-shots are microsecond-scale and do
not fit the calibrated-workload methodology; the suite
stays at the v1 set with the 5x flagging rule;
crypto/serialization bench rows are a recorded follow-up,
not a silent drop). Corpus + conformance + differential hold
to the v1 standard (same harnesses, same exactness).

macOS tier1-probe verdict: PENDING — the run-44606812115
macOS job is queued behind the documented GitHub-hosted
macOS ARM64 capacity constraint of November 2026 ("jobs
targeting macOS arm64 runners may experience longer queue
times"; 10h+ at time of writing; three wedged pre-fix runs
were cancelled to free runners). Pre-fix evidence stands:
macOS build + Tier-1 mir_conform green on run 44587228308;
the delta since is the stack-frame refactor (strictly less
stack on every platform) and the O_NONBLOCK correction
(macOS behavior now matches the green Linux path), with the
macOS-only cfg branch compile-verified via
`cargo check --target aarch64-apple-darwin` (and the
Windows fallback via `--target x86_64-pc-windows-msvc`).
This line is updated to green/red the moment the verdict
lands; no other gate depends on it.

## Acceptance checklist (§7)

1. Starting state verified (PDF title + 41 pp, main@4f88bf3,
   CI 44452237453 green-except-Windows); all seven Wave 0
   carry-forwards dispositioned above (5 carried by name,
   1 handed to Stage 1 and accounted, 1 partially superseded
   as briefed).
2. Networking substrate shipped (sockets TCP/UDP + DNS +
   M:N-integrated poller; API designed in spec §17 — USER
   REVIEW FLAGGED); poller proven correct under M:N in the
   deterministic concurrency harness (no lost wakeups on the
   1-worker seeded pressure config; one real lost-wakeup
   race found and fixed, evidence above); conformance green
   on supported platforms. FIX-FORWARD 2026-11-20: the
   multi-worker stall is root-caused (State->SchedState ABBA
   in `net_last_error` vs the drain) and fixed (commit
   `ac6933c`), re-proven by the 18-cell
   `net_hammer_mw` regression test at workers 2/3/4 —
   evidence in the fix-forward section; the KNOWN GAP is
   closed.
3. Crypto bindings shipped via FFI to documented system
   libcrypto (name/version/license + D6-fit rationale above
   and in ffi.md); known-answer tests passing per primitive;
   no home-grown crypto; no deferred-FFI work smuggled in.
4. Context ADR delivered (`docs/decisions/adr-009-…`,
   PROPOSED); user approval is the exit gate; no
   implementation in this wave.
5. Serialization shipped: YAML, TOML, protobuf, compression,
   each with stated scope and conformance tests.
6. Full gates green (check.sh exit 0 locally; CI run
   44659508115 on the fix tip — check, ARM, release-gates
   green, bench running, macOS queued per the note above);
   corpus, conformance,
   differential to the v1 standard; totals re-derived (62
   result lines / 225 passed / 0 failed — 224 + the
   `net_hammer_mw` regression test, per-crate account
   corrected above).
7. Report complete with checklist, API design (§17, flagged),
   library choice + vector sets, ADR (appendix below),
   and every deferral with rationale (multi-worker stall,
   intermittent CI stall, no new bench rows, empty TOML
   tables, Windows, AOT-native net/crypto/gzip,
   all-interfaces bind, Group C items).
   No Waves 2–5 work (no TLS/HTTP/logging/metrics/DB/auth/
   tracing/profiling/debugger/embedding/cloud/plugin ABI);
   no frozen-decision change.

## Deferrals carried with rationale (summary)

- Multi-worker scheduler stall (gating defect, evidence +
  repro in Stage 1) → follow-up: parallel-scheduler audit
  with the `conc20` repro.
- Native AOT net/crypto/gzip (JIT/AOT refuse by design) →
  follow-up; executor is the reference.
- All-interfaces bind, Group C items (raw sockets, packet
  crafting/capture, full bytes story, WebSockets, HTTP/2,
  security-toolkit libs) → each needs its own gated
  proposal; explicitly not designed here.
- Crypto/compression on Windows, bench rows for new
  surfaces, TOML empty-table reporting → follow-ups noted
  above; Windows stays deferred per the POSIX-compat
  decision.
- Intermittent CI stall (runs 44583177946/44585320505 check
  jobs wedged 3h+ with no log output — one in fmt/clippy/
  test phase, one after typing `type_state_self_ref`; a
  macOS corpus gate also stalled): pre-existing (both runs
  predate the stack fix), sequential harness (no
  parallelism to race), unreproducible locally (the suspect
  case exits 2 three-for-three), and absent from the final
  tree (four consecutive runs green on every completed gate
  — check, ARM, bench, release-gates — plus local green).
  Recorded as a CI-stall follow-up; checker and harness are
  frozen territory, so no Wave 1 change.
- No real release, tag, or artifact publication (forbidden).

## Appendix — context ADR

See `docs/decisions/adr-009-context-propagation.md`
(PROPOSED). Open questions for acceptance (approval answers
all three): (1) spawn-inheritance as the default; (2)
absolute-monotonic-ms deadlines; (3) Wave 2 owning the
clock, the slot field, and the cancel hook.

## Fix-forward (2026-11-20) — red-gate closures

Review verdict was CONDITIONAL — NOT YET ACCEPTABLE
(criteria 2 and 6 FAIL, 4 PARTIAL). This section records the
fixes; the sections above stand un-rewritten.

Fix 1 — multi-worker stall root cause + fix (criterion 2).
The "lost wakeup in poll_wait" presents as every worker
parked with no wakeup, but the mechanism is a lock-order
inversion (ABBA), proven by gdb stacks of a hung run: one
worker held the heap-state lock inside `alloc_with` and
blocked acquiring the sched lock in `net_take_error`
(`exec.rs`, `toto_rt_net_last_error` arm), while another
held the sched lock in `drain_completions_locked` and
blocked acquiring the heap-state lock for a completion
payload. Single-worker cannot deadlock (no second thread);
two workers race rarely, four usually — the exact observed
shape. Fix (commit `ac6933c`): hoist the error-slot take
above the allocation (two sequential critical sections, no
nesting), mirroring the crypto/gzip last-error arms which
were already correct; full audit of every `alloc_with` call
site and every direct heap-state region found no other
sched-nesting (all sequential or pure reads). `alloc_with`
now documents the lock-order contract. No D1–D8, ABI,
socket-API, or ADR change; 64 KiB budget untouched.
Re-proof: new regression test
`crates/toto-runtime/tests/net_hammer_mw.rs` (18 cells:
workers 2/3/4 × seeds 42–47, exact one-line golden, 15 s
watchdog per cell, stack high-water asserted under 64 KiB
on every cell) — pre-fix it stalled (e.g. workers=4
seed=47, workers=3 seed=44); post-fix 10/10 sweeps green
(180 cell-runs) plus 10/10 CLI `toto run --workers 4`
hammer runs exit 0. Proof workloads/seeds/counts are these;
CI re-verifies on every run.

Fix 2 — tip check failure (criterion 6). Run 44641742691
(tip `aeb34ba`): `rebuild_cancels_parked_generation`
(`toto-jit/tests/dev.rs:481`) got E0216 (deadlock) where
E0214 (scope cancel) was expected — a timing-sensitive
cancel-vs-deadlock race in JIT dev-mode settle (Phase 4
code; neither the test nor its implementation was touched
by Wave 1, and the tip delta is 3 lines of report prose, so
the tip commit cannot have caused it). Reproduction: 34/34
local passes (20× single, 10× full binary serial, 4× full
binary parallel) — does not reproduce. Literal `gh run
rerun --failed` was refused (the run is still open: its
macOS job is queued); identical-code check was green on run
44635755882 and four other consecutive runs. Stated plainly:
not reproduced, not waved away — and the fix tip's check is
green on run 44659508115 with no recurrence, which closes
this item empirically.

Fix 3 — macOS Tier-1 verdict (criterion 6). Still queued at
time of writing (runs 44606812115, 44635755882, 44641742691,
44631337623; the review's 44603510550/44602323178 return
404 — phantom ids, not real runs). Queue ages corrected:
they are per-run (≈4.5h max observed on 44606812115 at
check time; the tip run ≈1h), not the cumulative "10h+"
the earlier note implied — the backlog itself is the
documented November macOS-ARM64 capacity constraint. The
verdict line in Stage 5 is updated the moment a result
lands; if no verdict proves obtainable, macOS deferral goes
back to the user for an explicit evidence-based decision
(per this section's rule, never silent).

Minor fixes (same turn): crypto version/license recorded in
`docs/ffi.md` + Stage 2 (runtime-detected via
`crypto_openssl_version()`, never pinned, rationale stated;
Apache-2.0 / OpenSSL+SSLeay+ISC lines); spec §12.1 "frozen
25" → "frozen 27" (one line; the Phase-2 report's "25" is
historical and stands); hunt-history wording sharpened
(tree verified zero markers, history never rewritten,
57cf017's try-each-address and the diag13/14/15 goldens ship
by design with attribution).

Test totals re-derived at the fix tip: 62 result lines /
225 passed / 0 failed — the Stage 5 per-crate account plus
the new `net_hammer_mw` binary (toto-runtime 29 → 30).

