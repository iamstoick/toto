# Phase 7 — Production AOT and ecosystem (GATED ON BENCHMARKS)

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
