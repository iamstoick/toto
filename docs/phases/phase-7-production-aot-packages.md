# Phase 7 — Production AOT and ecosystem (GATED ON BENCHMARKS)

> **Phase 7 verdict: COMPLETE — production Cranelift AOT (LLVM NO-GO re-confirmed) + package manager implemented per the approved design with both amendments.** Full transitive resolution, `toto.lock`, path/git/local-publish sources, first-segment import resolution with loud ambiguity errors. Gate green (60 result lines, 219 tests, 0 failed); CI green on run 44125326885 (check + release-gates + bench); merged to main as 4734b54. Details in §§Stage 1–Stage 4 below; earlier section text is preserved verbatim.

Branch `phase-7/production-aot-packages` off `main` (`94fcdbe`).
Gate green throughout (`scripts/check.sh` exit 0).

> **Stage 1 verdict: NO-GO for LLVM — Cranelift AOT is sufficient;
> LLVM stays cut (conditional on the §2.3 Stage 2 re-confirmation).**
> Speed AOT ≈ Speed JIT (16 vs 15 ms on 1M iterations); both ≈120×
> the interpreter. Full table and threshold in §Stage 1 below; the
> evaluation text is preserved verbatim.
>
> **STOP: Stage 1 ends here. No Stage 2 work without explicit
> user authorization.**

Stage 2 + Stage 3 proposal appended below (authorization dated
2026-11-17: Cranelift-only production AOT + design proposal, then
STOP). Stage 1 text above preserved verbatim. Dated note
2026-11-17: Stage 2 complete, gate green (59 lines / 191 passed /
0 failed), proposal ready for approval — no implementation past
this report.

Note on sources: no PDF was attached to the Phase 7 kickoff turn.
The title page was verified against the local copy
(`/home/gerald/Downloads/toto-implementation-roadmap-v2-4.pdf`):
"Toto (.gzv) Implementation Roadmap v2.4 and Phase 0 Decision Doc
(D1-D8)", 37 pages via `pdfinfo`. If the attached PDF differs, STOP
and say so.

## Stage 0 — starting state + Phase 6 flag close-outs

- Verified: `main` at `94fcdbe` (Phase 6 merged + hygiene), tree
  clean, `check.sh` EXIT 0 on `main`, CI green on run 44037278586
  (both jobs). Branch `phase-7/production-aot-packages` created
  from it.
- Flag close-outs (all doc/clarity + one test; no behavior changes):
  spec §13 "28" → 27 (two spots, dated notes; `len` was
  double-counted); shared-cell concurrency semantics in §14.1 (as
  implemented); migrate-name clarification in §14.2 (name need not
  equal `S`); watermark/census/LSP/acceptance-checklist recorded in
  the Phase 6 report addendum; mixed good/broken preserving-path
  soak added (`broken_edit_keeps_run_then_next_good_preserves`).
  Stage 0 commit pushed; CI green (run number in the final report).
- D8 stays frozen; 27 builtins frozen; module-model amendments stand
  as recorded (user review still owed — out of scope here).

## Stage 1 — benchmark evaluation (STOP after this stage)

### Suite and methodology

- Fixed suite: `scripts/bench.sh` (committed; `sh -n` clean; NOT
  part of `check.sh` — ~10 min wall time, manual per release).
  Correctness first: exact output asserted on every path before
  timing (constants for `bench/*`, cross-path identity for
  wordfreq); a mismatch fails loudly, never timed blind.
- Workloads: `bench/compute.gzv` (arithmetic loop, 1M iters),
  `bench/conc.gzv` (channel fan-out/fan-in, 1M total),
  `bench/stdlib.gzv` (gzv.std math + collections, 1k elems),
  `apps/wordfreq` with a generated 12000-line input (2000× the
  stock 6 lines, deterministic `seq`+`cat` method; repo app
  untouched). Corpus programs are all millisecond-scale, so the
  first three are calibrated bench workloads (~2-3 s interpreter);
  wordfreq is the repo-real anchor (stdlib + FFI + channels).
- Paths: interpreter = `toto run` (tree-walker; runtime executor
  for concurrent programs — noted per workload, never hidden).
  JIT = `jit_probe` (gate mode sequential, `--task` concurrent —
  the differential split). `toto run` has NO JIT path
  (interpreter-or-executor only), so JIT numbers are
  compile+run; the timings suite pins compile cost separately.
  AOT = `toto build` output in isolated project dirs.
- Statistics: median + range over n=5 runs; single runs never
  reported. Machine: AMD Ryzen 5 5600G (12 threads), 27 GB RAM,
  Linux 7.0.0-42, rustc 1.100.0, debug profile throughout (gate
  parity; a release check confirms the verdict — see below).
  Environment caveat: the run window had ~1 GB free RAM with heavy
  tmpfs use; ranges stayed tight (±0.2-3%), so the numbers are
  stable, but absolute values are machine-state dependent.
- LIMIT (recorded): the probe is single-file only (clean E0401 on
  imports), so JIT has no stdlib/wordfreq number. An
  import-capable JIT runner is named as a Stage 2 scoping input,
  not built here.

### CPU time (median [range] ms, n=5, debug)

| Workload | Interp | JIT | AOT |
| --- | --- | --- | --- |
| compute | 1939 [1919-1953] | 16 [15-16] | 125 [111-127] |
| conc | 3139 [3113-3184] | 137 [136-137] | 110 [108-112] |
| stdlib | 1765 [1763-1785] | N/A (E0401) | 215 [214-234] |
| wordfreq | 1191 [1178-1220] | N/A (E0401) | see anomaly |

Gaps: JIT beats interp 121× (compute), 23× (conc). AOT beats
interp 16× (compute), 29× (conc), 8× (stdlib). AOT-vs-JIT at Off:
compute 125 vs 16 (AOT 8× slower), conc 110 vs 137 (parity).
The Off AOT/JIT gap on compute is unexplained at this level
(same opt flags, same runtime) — recorded as a Stage 2 question;
production runs optimized, where it vanishes (see below).

### Startup and size

Trivial program: interp 4 [4-5] ms, JIT 6 [6-6] ms (incl.
compile), AOT 4 [4-5] ms. No path has a startup problem.
AOT binary size (debug): ~70 MB each (compute 70268192, conc
70354256, stdlib 70434224, wordfreq 71215584, trivial 70228200).
Release compute: 2398432 bytes (2.4 MB). Debug bloat
(unoptimized + symbols + statically linked runtime) is expected;
production ships release.

### Optimized check (debug verdict control)

Same 1M-iteration loop, `toto build --release` (Speed) vs
`jit_probe --opt-level=speed`: AOT 16 ms vs JIT 15 ms
(incl. JIT compile). Parity — the release path pays no
startup-compile cost and matches the optimizing JIT.

### Cold/incremental builds (compute project, warm registry)

Cold `toto build` (target dir removed): ~23 s. Incremental
(`touch`, no content change): ~0.3 s. Same inputs rebuild
byte-identical (sha256 `93d64b9c…abdda` both times) — the
reproducibility method (rebuild-and-compare) already works on
this path. First-ever build needs `cargo` + the crates.io index
(one network fetch; pre-existing Phase 5 disclosure, unchanged).

### Phase 3 baseline finding

`docs/phases/phase-3-runtime.md` records NO benchmark figures
(only a do-not-build mention of benchmarks). Per the brief, no
baseline is fabricated: the baseline is `main` at `94fcdbe`
(= current tree modulo docs/test-only Stage 0 delta, no
perf-relevant changes), dated 2026-11-17, reproducible via
`scripts/bench.sh --runs 5`. All tables above publish against it.

### Production-AOT scoping answers

- Optimization: one flag, three levels, no passes of our own
  (`OptLevel::{Off, Speed, SpeedAndSize}`, `opt_level=none/
  speed/speed_and_size`; MIR/codec/entry-points/root publication
  identical at every level, pinned by `opt.rs` equivalence).
  Speed changes only machine-code emission (standard Cranelift
  pipeline). What LLVM would add (vectorizer maturity, LTO,
  broader target matrix) is unmeasured because no gap calls for
  it — see verdict.
- Reproducible builds: method = same inputs → byte-identical
  outputs, proven above (identical sha256 across rebuilds).
  Pinned toolchain (rustc 1.100.0 recorded) + deterministic
  codegen + same paths; no timestamps observed in outputs.
  Stage 2 hardens this into a gate (rebuild-and-compare in CI).
- Cross-compilation: `toto build` has NO `--target` flag
  (only `--release`); multi-target emission is not implemented.
  Recommendation: defer — cross-platform CI/conformance is
  Enterprise Wave 0 (explicitly out of scope); Linux x86_64 is
  the only supported ship target in this phase.
- Build-shim network: production `toto build` must not depend on
  a network fetch at build time. Today the first build fetches
  from crates.io. Stage 2 scope: vendored dependencies
  (or a prebuilt runtime link) + `--offline` builds; the
  current fetch-once-then-cached behavior stays for dev.
- Module-model boundary: the package manager (Stage 3) must work
  with the recorded model (flat namespace, path imports,
  `gzv.std/` prefix) unchanged — STOP-and-ask if any change
  looks necessary. No change needed for benchmarking.

### The wordfreq AOT anomaly (disposition, not verdict input)

AOT wordfreq scales super-linearly in input lines — 16 ms (6
lines) → 343 ms (600) → ~23 s (6000) → ~75 s (12000) — while
interp scales linearly (10 ms → ~1.2 s) and outputs agree
exactly. Isolation: a no-FFI copy of the app (only
`c_strlen` → `len` changed) runs AOT in 3 ms vs interp 1027 ms
(AOT 342× faster). So the pathology is specific to the
FFI-bearing path in task mode (per foreign-call round trip
degrading with scale), NOT codegen: LLVM would share the same
runtime suspend/pool path and cannot be credited with fixing
it. Excluded from the verdict's core comparison; recorded as
the top Stage 2 investigation item with repro
(`scripts/bench.sh` wordfreq + the no-FFI control). Not a
correctness issue (interp==AOT outputs on all paths).

### Verdict: NO-GO for LLVM (evidence-backed)

Threshold (stated): GO iff optimized Cranelift AOT trails an
optimizing reference by ≥2× on compute-heavy workloads after
Stage-2 hardening, or a required ship target is unreachable
from Cranelift. Measured: Speed AOT ≈ Speed JIT (16 vs 15 ms
on 1M iterations, JIT number including its compile), and both
≈120× the interpreter. No gap, no missing target (only Linux
x86_64 ships in this phase; Cranelift covers it). The
Off-level AOT/JIT gap (8× on compute) and the FFI-path anomaly
are runtime/codegen-pipeline questions for Stage 2 hardening,
not second-backend questions. A well-evidenced NO-GO: Cranelift
AOT is sufficient; LLVM stays cut. If a future workload shows
a ≥2× codegen-attributable gap, or a new target demands it,
re-open with numbers.

**STOP: Stage 1 ends here. No Stage 2 work without explicit
user authorization.**

## Stage 2 — production Cranelift AOT (authorized 2026-11-17)

Scope: Cranelift-only (no LLVM of any kind), Linux x86_64 only,
D8 frozen, 27 builtins frozen, no spec changes needed, no
`--target` work, no package-manager implementation. Method:
median + range over n=5 (`scripts/bench.sh`, extended this stage:
release builds + timings, 4-size wordfreq series, release sizes,
real-app startup). Machine: Ryzen 5 5600G (12 threads), 27 GB
RAM, Linux, rustc 1.100.0. Suite needs ~5 GB free in `$TMPDIR`
(a transient `EDQUOT` gate failure at 81% tmpfs use was cured by
cleanup; recorded as methodology, not a code issue).

### Step 0 confirmation (§1)

- Header already carries the NO-GO verdict (commit `7930f03`);
  Stage 1 text preserved verbatim.
- The 2 deleted lines in `b0b3d6f` to the Phase 6 report were the
  stale test-count lines (`- New tests ... 10 preserve_proofs` /
  `  (7 matrix + ... + soak) +`), replaced in the same commit by
  the 11-`preserve_proofs` + mixed-soak lines. Count update for
  the added mixed-soak test; no history rewritten.
- Baseline re-derived from a fresh gate run (not copied):
  **59 result lines / 191 passed / 0 failed**
  (`scripts/check.sh` exit 0). Per-binary account: spikes 39
  (7+8+6+10+8, the long-standing pin), toto-compiler 35,
  toto-cli 23, toto-jit 68, toto-runtime 26; sums to 191
  exactly. Delta vs the recorded 189: +1 is the new
  `build_offline_needs_no_network` gate test (this stage); the
  other +1 is an undercount in the 189 figure (tree delta since
  `8a8c03e` adds exactly one test; the per-binary sum closes at
  191 with the 39-spike pin intact).

### §2.1 FFI anomaly — mechanism, fix, proof

Mechanism (root-caused, not hypothesized). Task-mode codegen
emits a rendezvous call at every Loop/Call/Entry safepoint
(`crates/toto-jit/src/compile.rs`: `publish_all`, then
`session.call(task_gc_collect)` only in Task mode; gate mode
emits no call). `task_gc_collect` runs the stop-the-world
rendezvous join and then calls `Heap::stats()` — which scanned
all slots (`slots.iter().filter(count)`) and DISCARDED the
count (used only for the rendezvous return). So every
foreign-call round trip paid O(live heap): wordfreq holds one
live string per input line, hence O(n^2) — 16 ms / 343 ms /
~23 s / ~75 s at 6/600/6000/12000 lines. Evidence chain:
gate-mode vs task-mode FFI probe (30 ms vs 2.6 s, same flags,
same 12k calls); pre-grown 40k-string heap turning the same
12k calls 2.6 s → 30 s (cost follows heap size, not call
count); 0 collections during the runs (threshold path
innocent); strace ~99% userspace futex burn, flat 42 MB RSS
(lock + scan, not leak); interp linear 10 ms → 1.2 s while
outputs agree exactly. LLVM would share this runtime path, so
the anomaly was never a codegen question.

Fix (`crates/toto-runtime/src/heap.rs`, +13/-1 lines): `Heap`
carries an incremental `live` counter (`insert` +1, `collect`
recounts from the sweep); `stats()` is O(1). No contract
change: values identical, transitions unchanged (insert is the
only `None → Some` path; sweep recounts). The existing
exact-count asserts (`gc.rs`, incl. `objects_live`, plus
`gate.rs` stats asserts) now run through the counter — they
fail on any miscount, so the fix carries its own regression
cover; the wordfreq-12k bench row is the performance cover.
Safepoint emission is untouched (the rendezvous join stays
load-bearing for stop-the-world coordination).

Cross-path answer (§2.1 question): YES, the JIT task path
shared the pathology — `zz_ffidiag` (wordfreq, task-mode JIT,
6000 lines) measured ~23 s pre-fix, 190/224 ms post-fix
(threshold u64::MAX / 1 MB, 0 / 2 collections, output
correct). Same fix, same cure; no separate JIT work needed.

Proof (bench.sh 4-size series, median ms n=5, ranges ±1–4%):

| lines | interp | aot | aot-release |
| --- | --- | --- | --- |
| 6 | 16 | 15 | 8 |
| 600 | 75 | 36 | 15 |
| 6000 | 601 | 250 | 78 |
| 12000 | 1189 | 515 | 152 |

Linear scaling restored on all three paths; exact
interp==aot==aot-release identity asserted per size (a mismatch
fails the suite loudly). Pre-fix → post-fix at 12000 lines:
~75 s → 515 ms (Off) / 152 ms (Speed). Interp is now the
slowest path at every size (16×/7×/8× AOT-Speed advantage).

### §2.2 Off-level gap — explained (no defect, no fix)

Post-fix numbers: compute Off AOT 96 ms vs gate-JIT 16 ms
(Stage 1: 125 vs 16 — same shape, remainder is machine state;
compute holds ~0 live heap, so the stats fix cannot explain
moves here). Conc Off AOT 96 vs task-JIT 117 (parity). Compute
Speed AOT 16 vs Speed JIT 15 (parity). Conc Speed AOT 16 vs
task-JIT 117 (AOT 7× faster).

The gap is MODE, not backend: task emission pays per-safepoint
`publish_all` + the rendezvous import call + `maybe_collect`
(~80–100 ns/iteration in an Off build — dominant in a bare
arithmetic loop); gate JIT skips the call by design
(Task-only emission). Same-mode comparisons are parity at
both levels; the residual per-safepoint cost is the price of
stop-the-world coordination (weakening it is out of scope).
Stage 1's "unexplained" is now explained; production (Speed)
was parity all along.

### §2.3 NO-GO re-confirmation at production opt

Production table (median ms, n=5; JIT = gate, except conc =
`--task`; JIT times include compile):

| workload | interp | jit | aot-release |
| --- | --- | --- | --- |
| compute | 1874 | 16 | 16 |
| conc | 3115 | 117 | 16 |
| stdlib | 1761 | N/A (E0401) | 48 |
| wordfreq-12k | 1204 | N/A (probe) | 145 |

Speed AOT vs the optimizing reference: compute 16 vs 16
(1.0×), conc 16 vs 117 (AOT faster). stdlib/wordfreq have no
JIT reference (recorded limit, §2.8); wordfreq task-parity is
covered by `zz_ffidiag` (6000 lines: JIT-task 190/224 ms vs
AOT-debug 250 / AOT-release 78). No workload shows a ≥2×
codegen-attributable gap with AOT trailing. The ≥2× threshold
is not met — **NO-GO re-confirmed: LLVM stays cut.**

Debug table for the record: compute 1874 / 16 / 96,
conc 3115 / 117 / 96, stdlib 1761 / N/A / 201,
wordfreq-12k 1204 / N/A / 507.

### §2.4 Optimization levels (defined)

Production levels are the existing `OptLevel` variants, now
each with a shipped meaning: **Off** (`opt_level=none`) =
`toto build` (debug object + debug-profile runtime link);
**Speed** (`opt_level=speed`) = `toto build --release` (Speed
object + release-profile link). `SpeedAndSize` stays API-only,
unshipped: release binaries are already 2.4–3.2 MB, so no size
pressure justifies a third shipped level. Beyond the Phase 4
flagged pipeline there are no new Toto passes — same Cranelift
pipeline, same MIR/codec/entry-points/root publication at
every level (pinned by `opt.rs` equivalence); production adds
the release link plus the pinned reproducible offline build
(§2.5–2.6). Cranelift stays the default (only) backend.

### §2.5 Reproducible builds (re-proved, full hash)

The Stage 1 truncated-hash claim is superseded. New proof:
`offproof` project (`bench/compute.gzv`), three
`toto build --release --offline` runs (cold link ~49 s,
incremental rebuild, clean-shim rebuild) → identical FULL
sha256 `35a489af2db008973426afb9187e38ee96647c10b0edb64d1fa9
c130174f0478` of the NAMED artifact `target/offproof`,
correct output `1249999000000` every time. Pinning: `toto
build` now copies the workspace `Cargo.lock` into the shim
(isolated workspace adapts it with the shim root), so two
builds can no longer resolve different registry versions.

### §2.6 Vendoring / offline (implemented + proven)

`toto build [--release] [--offline]` (`--help` updated):
`--offline` passes `--offline` to the inner cargo, whose own
offline gate hard-errors on any network attempt — so exit 0
proves zero network dependence (the three §2.5 builds all ran
`--offline` on a warm cache). A cold cache fails loudly
(documented in `--help`); it never silently fetches. The
committed gate test `build_offline_needs_no_network`
(`crates/toto-cli/tests/cli.rs`) builds a scratch project
offline and asserts binary==`run` equivalence — production
offline-ness is now CI-covered. Dev flow keeps
fetch-once-then-cached. No vendored-sources directory: with
the lockfile pin + `--offline`, production builds are
network-free without vendoring ~GBs of registry sources into
the repo (stated tradeoff; revisit if a hermetic cold-cache
build is ever required).

### §2.7 Deferred (§2.7–2.8)

- Cross-compilation: DEFERRED, no `--target` work. Linux
  x86_64 is the only ship target (Wave 0 boundary).
- No JIT path for `toto run` (verified: none exists; none
  added — stop-and-ask stands).
- `SpeedAndSize` unshipped (§2.4).

### §2.8 JIT-runner decision: record-the-limit

No import-capable JIT runner is built. Rationale: the
production question (AOT vs interpreter) is fully answered
without it; AOT-vs-JIT is measured everywhere comparable
(compute/conc Speed parity; wordfreq-task parity via
`zz_ffidiag`, which is RETAINED as the wordfreq-JIT-task probe
— header comment updated from "temporary", clippy-clean).
The only remaining N/A cell is stdlib-JIT (E0401), which
cannot move the NO-GO verdict (threshold needs a ≥2× trailing
gap; Speed parity holds where measurable). `bench.sh` header
records this; building the runner would serve one verdict-
irrelevant cell.

### §2.9 Startup (§2.9)

Trivial program: interp 5 ms, JIT 6 ms (incl. compile),
AOT-debug 4 ms, AOT-release 4 ms. Real app (wordfreq release
binary, 6-line stock input): **9 ms**. No startup problem on
any path; production startup is ~4–9 ms.

### §2.10 Conformance + differential (§2.10)

- Tier-1 (`mir_conform`) + Tier-2 (`gate`, shared codegen core)
  green in the gate (191 suite).
- AOT differential green on both levels: bench correctness
  (4/4 debug + 4/4 release + three-way identity at all 4
  wordfreq sizes); `scripts/buildsample.sh` **7/7**
  (arith/closure/structs/loops/divzero-panic/pingpong/stdlibproj,
  exit 0); `check.sh` wordfreq stock-input identity.
- Finding: `buildsample.sh` as committed could NEVER complete
  — two `set -e` landmines (divzero's nonzero exit in command
  substitution; `want_head` with both `head` fallbacks missing
  since no stderr goldens exist and `$1` is the sample name,
  not the corpus filename). Fixed with two minimal hunks
  (`|| code=$?`, `|| true`); the script now genuinely runs
  7/7. Consequence: the Phase 6 "7/7 observed" could only have
  been per-program manual runs — the per-program outcomes all
  reproduce (divzero exit 1 + `panic[E0202]` head verified),
  but the script-level claim is corrected here.
- Production builds carry no dev machinery and do not
  hot-reload (unchanged architecture; stated in docs below).

### Standing rules (§2.11)

D8 frozen (no ADR-008 change, none needed); 27 builtins
frozen; no spec amendment required (help-text + scripts only);
never force-push; named files only. Full gate green on the
branch (this section written after the green run).

### Acceptance checklist (brief §6, criteria 1–5 + design part of 6)

1. Starting state verified + Phase 6 flags closed (Stage 0,
   committed `b0b3d6f`; deletions confirmed above). ✓
2. Benchmark evaluation delivered with fixed, committed,
   reproducible suite (`bench.sh` extended, not replaced);
   methodology + variance + tmp-space requirement recorded;
   STOP after Stage 1 observed (authorization `8a8c03e` →
   next work only on the §3 go-ahead). ✓
3. LLVM verdict evidence-based: GO threshold (≥2× trailing
   gap) stated in Stage 1, not met at production opt on any
   workload; well-evidenced NO-GO. ✓
4. Production AOT = authorized Cranelift-only scope; no LLVM
   exists to be a second backend. ✓
5. Reproducibility proven by full-hash named-artifact method
   (§2.5); cross-compilation explicitly deferred with
   rationale (§2.7). ✓
6. (design part) Package-manager design proposed below; STOP
   observed — no implementation. (Builds-from-clean-cache
   proof, benchmark-flagging CI rule, cold/incremental
   small/medium/large timings, and docs updates are Stage 4
   items for the next authorization.) ✓

## Stage 3 — package-manager design proposal (STOP after this section)

Premise: the module model is APPROVED (2026-11-17) — path
imports, flat top-level namespace, no `mod`/`use`, `pub`
accepted with no effect, `gzv.std/` prefix, duplicates `E0311`,
circular `E0402`, unresolvable `E0401`. The manager is designed
to work WITH it unchanged. No public registry (roadmap
condition unmet: no stability review yet, no demand evidence).

### 1. Manifest (`toto.toml` — what stays, what changes)

`[project]` stays EXACTLY (name/version/entry; the strict
reader + the forward-compat promise — every manifest written
today stays valid). ADD one table:

```toml
[dependencies]
leftpad = "1.2.3"          # caret requirement (^1.2.3)
http = { path = "../http" }
json = { git = "https://example.com/json.gzv.git", rev = "abc123…" }
```

`version` becomes SemVer-2.0-required (today: any non-empty
string). Bare `"1.2.3"` means caret `^1.2.3`; `=1.2.3` pins
exactly (tilde `~` NOT offered in v1 — one range operator is
enough). The strict reader is extended the same way it was
built: new table/keys accepted, everything else still rejected
with line-numbered exit-3 errors.

### 2. SemVer + resolution

- Versions are SemVer 2.0 (numeric MAJOR.MINOR.PATCH;
  prerelease `-alpha.1` allowed in a package's own version).
- `toto update` never selects a prerelease unless the
  requirement names one (Cargo-like, one sentence).
- **Single-version rule** (follows from the flat namespace):
  the resolver picks ONE version per package name (max
  satisfying all requirements); two incompatible requirements
  → loud error with the constraint chain, never two copies
  (two copies could not share the flat namespace without
  `E0311` collisions).
- Yanked: no yank source exists without a registry — deferred
  WITH the registry. Git deps pin immutable rev hashes, so
  silent replacement is impossible; a vanished source is a
  loud error, not a silent fallback.

### 3. Lockfile (`toto.lock`, committed)

Per package: name, version, source (path, or git URL + pinned
rev hash), content hash over the package's `.gzv` file list.
Required iff `[dependencies]` is non-empty (`init` creates
none — no deps, no lock). `build`/`test` fail loudly on a
missing or stale entry, naming `toto update`; `update`
re-resolves (all, or one named package) and rewrites the lock.
Hash mismatch on a cached package → error naming the package
(tamper/rot is never silently accepted).

### 4. Sources + what "registry" means (no public registry)

v1 sources: local path + git only. "Registry" in v1 = the
embedded `gzv.std/` prefix (exists today) + nothing else: no
index, no server, no moderation, no hosting. A file-based
local registry layout is NOT designed here (deferred with the
public registry).

### 5. Build caching

Per-project `target/` (existing; no shared cache in v1 —
stated, follow-up). Cached: compiled package objects + final
link. Key: (package name, version, source/content hash,
OptLevel, toto version). Any input-hash change invalidates;
`update` prints what it re-resolved.

### 6. `add` / `update` / `publish`

- `toto add <name> [--path/--git]` edits `[dependencies]`
  and updates the lock (one step, loud on conflict).
- `toto update [pkg]` re-resolves within requirements.
- `toto publish` with no registry = local packaging: writes
  `<name>-<version>.tar.gz` + checksum in a registry-ready
  layout for vendoring/path consumption. It does NOT upload,
  list, or reserve anything — documented as local-only until
  a registry exists.

### 7. Mapping onto the approved module model (no change)

Files load into the single flat namespace; cross-package
duplicate top-level names are the existing `E0311` (no new
error, no new rule). A dependency's own relative imports
resolve against its own files; diamond loads dedupe by
canonical path (existing rule). **Decision point for the
owner (flagged, not decided silently): dependency import
resolution.** Recommended: if the import path's first segment
matches a dependency package name, resolve inside that
package's directory; else relative-to-importer, then
`gzv.std/`. This extends resolution without changing any
recorded rule for existing programs (no existing program has
a dependency, so nothing observable changes). Alternative:
an explicit `pkg:` prefix — rejected in this proposal (new
syntax = spec change). Approval of the recommended rule (or a
direction) is needed before implementation.

### 8. Failure modes (all loud, exit 3)

Unresolvable version (constraint chain printed); missing
lockfile entry (`run toto update`); tampered/rotted cache
(hash mismatch); unreachable git source; cross-package `E0311`
(name both packages). No silent fallback, no silent
reinterpretation anywhere.

### Implementation sketch (for sizing, not authorized)

Extend the strict manifest reader (new table, same style);
`resolve` (requirements → pinned versions, single-version
rule); lockfile read/write + hash check; source fetch (path
copy, git clone at rev); import resolution first-segment rule;
`add`/`update`/`publish` CLI; committed clean-cache proof
(`toto update` from empty cache in CI). Estimated small
(roughly the manifest module ×3 + fetch) — exact plan on
approval.

### Effect on the versioning doc

Approving SemVer-required `version` amends the Phase 5
versioning policy (dated note, no enforcement machinery
beyond `init`/`build` needs).

**STOP: design proposal ends here. No package-manager
implementation without explicit user approval of this design
(especially the §7 resolution rule). No registry work of any
kind is proposed.**

## Stage 3 — implementation (authorized 2026-11-17, as amended)

### Step 0 — Stage 2 flag close-outs (dated 2026-11-17; no code)

(a) Test-count correction. The §2.11 "undercount" sentence is
withdrawn: there was NO undercount in the 189 figure.
189 + `build_offline_needs_no_network`
+ `zz_ffidiag_threshold` (the second new test, called a
"probe" in the Stage 2 section) = 191. The 189 baseline did
not include the untracked Stage-1 probe file; both tests are
counted in the 191. Do not change any test to fit numbers.

(b) `bench/ffi.gzv` disposition: DROPPED + deleted. The file
exists in this tree (22 lines, verified by full read before
deletion, 2026-11-17): a synthetic 12000-iteration `strlen`
loop whose header claims "Expected output asserted by
scripts/bench.sh" — but nothing references it (`bench.sh`,
docs, and report contain no mention; verified by search).
Rationale for drop over wire-in: the FFI path is covered on
real workloads at three levels (wordfreq 4-size series +
task/gate probes); a fixed-count synthetic loop adds no
verdict-relevant information, and wiring it would churn the
fixed suite for no decision value. Outcome: file deleted,
nothing referenced it, suite unchanged.

(c) Wordfreq startup, measured separately (method: time to
first output, not total run time). The release binary emits
exactly one `write(1)` — the final summary line
(`strace -tt -e trace=write` shows no earlier stdout write);
teardown after that write measures 1.2 ms. Bench total on
the 6-line stock input: 9 ms [8–9] (`start-aotrel-wordfreq`).
First (only) output ≈ total − teardown ≈ **8 ms**; compute
on 6 lines is negligible, so app-level init+IO ≈ 4 ms over
the 4 ms pure-process baseline (`start-aotrel` trivial).
Recorded alongside the trivial figures (§2.9), not replacing
them.

(d) Offline production-build story: documented in
`docs/production-builds.md` (new, this stage: what must be
warmed — the cargo package cache for the workspace
dependency closure — how — one online `toto build` (or
`cargo fetch` in the workspace) — and the exact cold-cache
failure, captured verbatim: `toto: linking …` then cargo's
`no matching package named 'corosensei' … location searched:
crates.io index … note: offline mode … can sometimes cause
surprising resolution failures`, exit 3 via `link failed`).
The copied workspace `Cargo.lock` is honored even cold
("locked to 0.1.0" in the error) — the missing piece is the
`.crate` files, never the resolution. Vendoring deferral
(vs `vendor/` or a prebuilt runtime link): the pin + `--offline`
already makes production builds network-free (proven §2.5–2.6)
without committing ~hundreds of MB of registry sources;
hermetic cold-cache builds are deferred — no current
requirement (CI images and builder machines warm once; dev
flow keeps fetch-once-then-cached). Do NOT claim cold-cache
offline builds work.

### Amendment 2 choice (stated before implementation): (a) full transitive resolution

Direct-only (b) would make packages non-composable (no package
could depend on another package); the transitive cost is
bounded (discovery runs at add/update time, where fetching is
allowed; build time stays offline from lockfile + cache).
Complete rule set, followed exactly:

1. Discovery: at resolve time (add / update /
   build-or-test-without-lockfile), read the root
   `[dependencies]`; for each selected package version, read
   that package's own `toto.toml` `[dependencies]` (path dir's
   manifest; git dep's checked-out rev's manifest; pkg file's
   packed manifest) and recurse BFS. Traversal order sorted by
   (package name, requirement text); visited set on
   (name, version, source-id).
2. Provenance: every requirement carries (requirer, text) —
   requirer = package name + version, or `<root>`.
3. Selection: single-version rule over the UNION of all
   requirements per package name: the highest version
   satisfying every requirement (a requirement without a
   version constraint accepts any). Candidates come from all
   reachable sources; ties broken by source-display sort
   (deterministic).
4. Conflict: no version satisfies all → loud CLI error
   (E06xx, exit 3) naming the package and EVERY requirement
   with its provenance. Never a silent pick, never two
   copies.
5. Dependency cycles (a→b→a): loud E06xx error naming the
   cycle (the flat namespace cannot represent cyclic package
   deps).
6. Lockfile records EVERY selected package (direct and
   transitive): name, version, source (path | git URL + pinned
   rev | pkg-file hash), content hash over the package's
   `.gzv` file list. Directness is derivable (manifest ∩
   lock); no extra flag.
7. `toto update <pkg>`: re-resolve pkg to the highest
   satisfying version from current sources and cascade its
   transitive subtree; all other packages stay exactly
   pinned. Full `toto update`: re-resolve everything.
8. No direct-only refusal exists: a dependency's own
   `[dependencies]` table is always honored.

### Build-caching rule (stated before implementing)

- Cached: the per-project build outputs (`target/<name>`
  binary + `.o` + shim link artifacts) — keyed, not always
  rebuilt.
- Key: content hash of EVERY loaded source file (entry +
  imports + dependency files, from the loader's file list) +
  manifest text + lockfile text (when present) + OptLevel
  (debug/release) + toto version (`CARGO_PKG_VERSION`).
- Store: `target/.toto-fingerprint` beside the binary.
- Invalidation: key differs, fingerprint missing, or binary
  missing → full rebuild. Nothing else (mtime ignored: a
  touch without content change stays fresh, correctly).
- Stale-impossible argument: the key covers every input byte
  codegen and linking consume; identical key ⇒ identical
  inputs ⇒ identical output; any change ⇒ key change ⇒
  rebuild. Dependency content flows through the loader file
  list; versions and sources through the lockfile text.
- Proof (test, with implementation): change a dependency
  source file, rebuild, observe the new behavior; no-change
  rebuild prints the fresh-skip message and leaves the binary
  untouched.

### `toto update` semantics (stated before implementing)

- `toto update`: re-resolve ALL requirements against current
  sources (path dirs re-read live; git tags re-fetched, revs
  immutable — a moved tag resolves to its new hash loudly),
  select max-satisfying per the Amendment 2 rules, rewrite
  `toto.lock`, print added/changed/removed packages (or
  "already up to date").
- `toto update <pkg>`: pkg + its transitive subtree only
  (§Amendment-2.7); unknown pkg → error naming it.
- Lockfile↔manifest disagreement: `build` / `test` / `run`
  → ERROR (E06xx, exit 3), never a silent re-resolve.
  `update` ignores the lock (rebuilds it; that is its job)
  and reports what it replaced. `add` edits the manifest
  then runs full resolution (disagreement moot — lock
  rewritten).
- Missing lockfile at build/test/run: resolve + write it,
  LOUDLY (stderr notice). Lock creation may use the network
  (like add); lock-present builds never do (§pinned rule).

### Implementation status

Implemented as committed below (separate commits). Dev driver note (no code):
`toto dev` stays single-file — a dependency import there
fails through the existing loud single-file path; packaged
programs are `build` / `run` / `test` scope.

### Implementation evidence (all in-tree, committed with this section)

New files: `crates/toto-cli/src/semver.rs` (166 lines: `^`/`~`/`=`
parsing + matching, no pre-release/build-metadata, `0.x` not
special-cased); `crates/toto-cli/src/packages.rs` (resolver with
full transitive closure, `toto.lock` read/write, git fetch into
the local cache, `.gzv-pkg` pack/unpack, `pkg_name_of`,
`build_fingerprint`); `crates/toto-cli/src/pkg_cmd.rs`
(`toto add` / `update` / `publish`); `crates/toto-cli/tests/packages.rs`
(10 integration tests, all green — see below).
Modified: `manifest.rs` (`[dependencies]` table), `project.rs`
(`DepEntry` + Amendment 1 `E0601`), `build_cmd.rs` (fingerprint
+ `resolve_deps` on the build/run/test paths +
`CARGO_TARGET_DIR` passthrough), `main.rs` (add/update/publish
wiring), `lsp.rs` (dependency files resolve; dep import of an
unknown file is still a diagnostic), `docs/spec.md` (§16
amendment, `E0601`–`E0609`).

The 10 integration tests pin every rule (each audited against
the design before blessing — outputs asserted, never
eyeballed):
`amendment_1_ambiguity_is_a_loud_e0601`,
`transitive_resolution_and_conflict` (highest-satisfying
selection + conflict naming every requirement),
`update_subtree_pins_satisfiable_pins` (pins outside the
subtree hold; an unsatisfiable pin rejoins free selection
with its bounds enforced),
`missing_cached_git_rev_fails_loudly_naming_the_rev`
(no network at build time),
`path_dep_add_run_build_and_fresh_skip` (round trip +
fingerprint fresh-skip message + untouched binary),
`lockfile_disagreement_and_missing_lock` (error vs loud
write-and-continue), `edited_source_fails_e0604_update_recovers`
(stale-serve impossible: changed content at a locked version
fails, `update` accepts the new hash),
`git_dep_builds_from_cache_after_remote_vanishes`
(add/update fetch; build never fetches),
`publish_round_trip_pack_add_build` (pack → add → build),
`unpinned_sourceless_and_conflict_add_time_errors`.

## Stage 4 — gates (2026-11-17)

### Conformance + differential

Tier-1 + Tier-2 suites green on all three backends
(interpreter, JIT, Cranelift AOT) via the existing gates —
no backend code changed in this phase, so no new divergence
is possible; `check.sh` re-confirms. Packaged programs are
NOT in the file-based differential corpus (single-file only
by construction) and are NOT in the Tier suites for the same
reason; they are covered instead by the `packages.rs`
round-trip tests, which assert `run` stdout on dependency
code (`path_dep_add_run_build_and_fresh_skip`,
`publish_round_trip_pack_add_build`). `toto run` and
`toto test` resolve dependencies through the same
`resolve_deps` path as `build` (`run_deps` /
`test_all`), so there is no build-only resolution skew.
`toto dev` cannot run packaged programs (single-file driver
by design) — stated, not a gap in the gate.

### Benchmark suite in CI

`scripts/bench.sh` takes `--runs N` (default 5) and
`--baseline <file>`; with a baseline it compares every
median against the pinned value and FAILS (exit 1) on any
ratio above 5x, printing `FLAG`. The 5x rule absorbs machine
variance (the baseline mixes Stage 1 published medians with
locally measured wordfreq-Off rows — sources recorded in
`bench/baseline.txt` itself) while catching
order-of-magnitude regressions: the Stage 1 wordfreq anomaly
was ~500x its fixed time, two orders of magnitude above the
flag line. Medians are order-statistic based and correct for
any `--runs` value (the old `NR==3` line assumed 5 runs).
CI runs the suite at `--runs 3` in a new `bench` job
(`.github/workflows/ci.yml`); the full `--runs 5` suite
remains the manual per-release gate (same honesty standard
as `buildsample.sh` / `devloop_soak.sh`).

Release benchmarks vs the Stage 1 baseline (`main` 94fcdbe,
2026-11-17): the fixed code performs at the Stage 1 Off
figures or better on every wordfreq row (Off AOT 12000-line
515 ms vs 75 s anomalous; JIT/CR numbers unchanged — no
backend code changed, only the task-mode FFI path). The
per-release full-suite re-run is the publication record;
this phase's numbers are the Stage 2 tables plus the
cold/incremental timings below.

### Cold + incremental build times (release, clean project target dir, warm cargo registry)

Machine: Ryzen 5 5600G (12 threads), 27 GB RAM, Linux,
rustc 1.100.0, `toto build --release`, `date +%s` wall time.
Sizes: small = 13-line single-file arith project; medium =
`apps/wordfreq` (52-line main + 58-line lib + stdlib
imports); large = wordfreq + a `greetlib` path dependency
(packaged: manifest + lockfile + cache resolution on every
build). "Cold" = `rm -rf` the project's `target/` only
(the cargo registry stays warm, as in Stage 1).

| project | cold | no-change rebuild | content-change rebuild |
|---|---|---|---|
| small | 48 s | 0 s ("is fresh, skipping rebuild") | <1 s |
| medium (wordfreq) | 52 s | 0 s | 2 s |
| large (packaged wordfreq) | 50 s | 0 s (fingerprint over lockfile text too) | (not re-measured; same shape as medium) |

Cold is size-independent: ~50 s is the cargo runtime link,
not Toto code. Content-change rebuilds recompile only the
shim against the warm project target dir (binary mtime
verified newer than the edited source — no stale serve).

### Clean-cache + reproducibility proof (packaged, ≥1 dep)

Project: the large variant above (wordfreq + `greetlib`
path dep), scratch-built under `target/scratch/sizes/big`
(not committed — built from the committed `apps/wordfreq`
plus a 5-line lib; the committed equivalent is the
`publish_round_trip_pack_add_build` test).
`TOTO_PKG_CACHE` emptied + `toto.lock` removed →
`./target/debug/toto build --release` exit 0 (resolution
re-created the lockfile loudly). `rm -rf target`, rebuild
from the same lockfile → FULL sha256 of the named artifact
`target/wordfreq`, twice:

`6f44011f5fc4797175d78df42e3e436af0abb187ebab7a3afaa1748ee781928c`

identical both times. The packaged binary prints the
fixture-expected `[6,23,3,138,"toto",2]`.

### Docs

`docs/package-manager.md` (manifest, SemVer, lockfile,
sources, imports incl. Amendment 1, add/update/publish,
offline, failure modes); `docs/production-builds.md`
extended with the warm-cache workflow (§1(d) close-out);
spec §16 amendment (`E0601`–`E0609`); versioning doc
unchanged (no package-manager impact on the stated policy).

### Full gates + counts

`scripts/check.sh` exit 0 (fresh run 2026-11-18: 60 result
lines, 219 passed, 0 failed; per-binary composition in the
commit message). CI green on both pre-existing jobs plus
the new `bench` job. Exact reconciliation, every line
counted once (spikes 39 + compiler/CLI 86 + runtime 26 +
JIT 68 = 219): the Phase 6 table (58 / 188) stands as
stated; Stage 2 at `0f24f81` added 3 tests + 1 line
(`tests/cli.rs` 15 → 16 via `build_offline_needs_no_network`,
toto-jit lib 28 → 29, new `zz_ffidiag` line — all
verified untouched by Stage 3, `git diff 0f24f81` empty
for those files); Stage 3 adds 28 tests + 1 line
(packages integration 10, packages unit 8, semver unit 3,
manifest unit +2, project integration +5). Corpus census
unchanged (single-file corpus untouched; packaged coverage
lives in `packages.rs`, 10 tests).

Robustness fix found by this stage's own validation: the
first `--runs 1 --baseline` rehearsal died at the startup
section — `/tmp` quota (EDQUOT), not a benchmark failure.
`bench.sh` kept all scratch under hardcoded `/tmp/` paths,
so the fix moves every bench scratch file under `$SAMPLE`
(`bench_medians.txt`, `bench_last_*.out`, wordfreq compare
outputs, `bench_trivial.gzv`), which CI leaves at `/tmp`
and low-space runs override via `TMPDIR`. The re-run with
`TMPDIR` in the workspace completed: all rows ok, worst
ratio start-aotrel 3.5x, "no regressions above 5x".

### Acceptance checklist (brief §6, all ten)

1. Starting state + Phase 6 flag close-outs: done (§Step 0,
   committed `efd7e12`; verified HEAD `0f24f81` + CI
   44087346205 before branching work).
2. Benchmark evaluation: delivered Stage 1, STOP observed
   (authorization `8a8c03e` Scope).
3. LLVM verdict evidence-based: NO-GO re-confirmed at
   production opt (§2.3; no ≥2x codegen-attributable gap).
4. Production AOT = authorized Cranelift-only scope; no LLVM
   exists anywhere in the tree.
5. Reproducible builds proven (full sha256, named artifact,
   §2.5 + packaged proof above); cross-compilation deferred
   with rationale (§2.7).
6. Design proposed, STOP observed, implementation matches
   the approved design + both amendments exactly; no
   module-model change (flat namespace, path imports,
   `gzv.std/` prefix untouched); no public registry
   (local-only `publish` packs a file; nothing uploaded).
7. Clean-cache packaged install + reproducible build proven
   (committed round-trip/cache tests + manual full-hash
   proof above with exact commands).
8. Fixed suite in CI with a stated 5x rule; release
   benchmarks vs the Stage 1 baseline; cold + incremental
   times recorded above.
9. D8 frozen throughout (no ADR-008 change); 27 builtins
   frozen (no new builtin); production builds carry no dev
   machinery and do not hot-reload.
10. This report covers all stages with benchmark tables,
    the LLVM verdict, the design as approved, and every
    deferral with rationale.

## Merge-hygiene dated notes (2026-11-18, on `main`; append-only — existing section text above is preserved verbatim)

- `docs/production-builds.md` provenance: the Stage 4 Docs
  bullet above calls the file "extended". Correction: the
  file was CREATED in Step 0 (`efd7e12`, 72 lines), which
  already contained the warm-cache workflow (§1(d)
  close-out). Stage 4 added no further content to it.
- Full-gates "verified untouched" claim: the text above says
  Stage 2 tests were "verified untouched by Stage 3, `git
  diff 0f24f81` empty for those files". Correction:
  `crates/toto-jit/tests/zz_ffidiag.rs` changed +3/−2 in
  `2ea8868` (a call-site arity fix for the new
  `load_and_check` `deps` parameter). The claim holds for
  `tests/cli.rs` (16 at `0f24f81`, empty diff) and the
  toto-jit lib count (29 at `0f24f81`); test counts are
  unaffected by the `zz_ffidiag` arity fix.
- Single deleted line in `2ea8868`'s report change (+198/−1):
  the removed line is `Pending below (separate commits). Dev
  driver note (no code):`, replaced by `Implemented as
  committed below (separate commits).` plus the evidence
  sections. Branch history is linear (`efd7e12` → `2ea8868`
  → `f5c8c7f`, then merge `4734b54`); no commit was amended
  and nothing was force-pushed — no history rewritten.
