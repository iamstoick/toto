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
