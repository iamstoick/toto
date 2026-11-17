# Phase 5 — Tooling, stdlib core, and FFI (report, 2026-11-16)

Branch `phase-5/tooling-stdlib-ffi`. Gate: `./scripts/check.sh`
exit 0 (fmt, `clippy -D warnings`, full workspace tests, stdlib
`toto test`, wordfreq tests + build equivalence), stable across
consecutive runs, CI green on the branch.

## Verdict: exit criteria met

All 10 acceptance criteria hold with evidence below. No frozen
decision changed (D8/ADR-008 untouched; ADR-006 gained an appendix
+ one refinement note, no rule change). No new builtins (27
frozen). Multi-file work keeps every single-file program green.

## Stage 0 — repo hygiene + record fixes

- `phase-4/jit-dev` merged to `main` first; work on
  `phase-5/tooling-stdlib-ffi` off `main`. No force-push, no
  rewritten history.
- README at Phase 4 complete (kept, still accurate).
- Phase 4 measurements addendum: present (dated 2026-11-16, stack
  + timings + 127-test reconciliation + 160-probe resolution).
- Divergence dispositions (all three resolved in-tree, recorded
  here):
  - (a) `Range[i]` MIR-executor alias — FIXED by removal.
    `exec.rs` has no Range fast path; indexing a non-array falls
    through to the shared `ops::index_read` (`E0209`) like every
    other executor. Static coverage: `type37_index_range.gzv`
    pins the checker rejection (`E0310`); the runtime fallback is
    dead in checked code and correct by construction (shared op).
  - (b) `(Chan, Chan) == true` — BLESSED as defined behavior.
    Spec §5.4 records it, `conc19_chan_eq.gzv` pins it, all
    executors agree, differentials green.
  - (c) `mn_jit_racy_select_legal_outcomes` dead branch — FIXED.
    Asserts `2\n` + `E0216` (the real `E0216` branch prints `2`
    first). Unreachable at the fixed seed; correct if reached.
- Race-detector decision: the schedule-sweep classifier STANDS as
  the v1 backstop. Rationale: `conc10` positive control caught,
  1,152 sweeps green, no missed race has been observed, and
  span-level attribution needs codegen support reserved for
  later work. No span-level build in this phase (first to drop
  per the brief; dropped with this rationale, not by silence).

## Stage 1 — module system, projects, build & test

- Imports: path-relative with `.`/`..` normalization, flat
  namespace, duplicates `E0311`, circular `E0402`. New files
  `docs/project.md` + spec notes. Existing single-file programs
  unchanged (full corpus green).
- `toto init <name>` scaffolds `toto.toml` / `main.gzv` /
  `tests/example.gzv`; manifest format (`[project]
  name/version/entry`) documented in `docs/project.md`.
- `toto build` lowers checked MIR through Cranelift to objects
  and links a standalone executable (debug/release). No LLVM, no
  cross-compilation, no LTO. Output equivalence verified on
  `apps/wordfreq` (interpreter vs binary byte-identical;
  §Stage 5) and the CLI's `build_runs_ffi_programs` scratch case.
- `toto test` runs every `tests/*.gzv` through the run path,
  exit 0 = pass, per-test lines + totals. The stdlib (8 files)
  and wordfreq are tested through it; both are now in
  `scripts/check.sh`, so the gate owns them.

## Stage 2 — `gzv.std` core

Nine modules, all Toto code on the frozen builtins + FFI, no new
builtins: `io`, `fs`, `strings`, `collections`, `math`, `time`,
`json`, `ffi`, `ffi_thread` (split from `ffi`, see refinement
below). Each has
API + behavior tests through `toto test` (`stdlib/tests/`, 8/8
green) and docs with examples (`docs/stdlib.md`).

- JSON: parse + stringify round-trip including error cases
  (`t_errors` pins each message); values are structs with compact
  nested payloads; `\uXXXX` rejected (no codepoint builtin).
- Collections: per-type Array helpers (`map/filter/fold/sum/
  sort/…` for `int`, `string` variants where the spec needs
  them). Map is type-only in v1 (D5): no values, no
  constructors; the module documents the limit and does not
  invent Map values.
- Execution paths (`docs/stdlib.md` table): interpreter full
  suite green; concurrent green (`test_concurrent.gzv`); built
  executable output-equivalent (smoke binary); `toto dev`
  unsupported (single-file scope — the known carry-forward).

## Stage 3 — FFI end-to-end proof (D6)

Real system libraries (libc, libm, libpthread), no toy C file:
`stdlib/ffi.gzv` + `test_ffi.gzv` (8/8) + corpus `proj11_ffi` +
diagnostics `diag13`–`diag15` + CLI `build_runs_ffi_programs` /
`build_refuses_callback_ffi`. ADR-006 appendix holds the
per-item dispositions (proven: real library, floats,
data-carrying callbacks via the queue, blocking calls on the
dedicated pool, foreign-thread confinement; deferred with
rationale: structs-by-value, `dlopen`, binding generation;
`panic="abort"` an untested documented limitation; foreign stacks
explicitly unscanned with the no-heap-refs rule by
construction). Per-binding ownership/encoding/thread-safety docs
in `docs/ffi.md`.

Stage 5 refinement (post-proof, recorded in ADR-006): `ffi` /
`ffi_thread` split. `c_thread_spawn`'s body calls
callback-taking `pthread_create`, and whole-program lowering
refused every importer — dead code poisoned the build. The two
thread wrappers moved to `gzv.std/ffi_thread` (run/test-only);
`ffi.gzv` keeps the declarations + non-callback wrappers and
stays buildable. Rule: unused callback *declarations* never
poison a build; only a *called* callback site refuses (exit 3).
`stdlib/stdlib.rs` embed list gained `ffi_thread` (rebuild
verified by `touch`-and-test).

## Stage 4 — formatter, LSP, docs

- `toto fmt [--check]`: parse-and-reemit canonical printer
  (`fmt.rs`, prec discipline, `no_struct` handling, else-if,
  annotations). Idempotent (`fmt(fmt(x)) == fmt(x)`, pinned by
  `compiler/tests/fmt.rs`: 113 formatted / 15 refused / 40
  executed) and semantics-preserving (formatted corpus runs the
  same goldens; `runtime/tests/fmt_equiv.rs`: 26 programs under
  GC pressure). Lossy by design and disclosed: comments vanish,
  `pub` drops, hex→dec, no line wrap. Rules in `docs/fmt.md`.
- `toto lsp`: hover / go-to-definition / diagnostics over
  stdio with a dependency-free hand-rolled JSON layer, single
  open doc + project-aware imports (open buffer overrides disk,
  `gzv.std` embedded). 3 protocol tests drive the real binary
  (`cli/tests/lsp.rs`). Verification is HEADLESS/protocol-only
  (disclosed in `docs/lsp.md`): no real editor run. Columns are
  Unicode scalars, not UTF-16 (disclosed).
- Stage 4 bug, fixed with evidence: the second LSP test's
  `didChange` frame carried one extra `}` (malformed JSON). The
  server's `read_message` mapped parse failure to session EOF —
  a silent death the driver reported as EOF. Fix: test frame
  corrected AND the server hardened to skip (not exit on) a
  malformed frame, pinned by `lsp_malformed_frame_is_skipped`.
  The server was exonerated by byte-level Python probes before
  the driver was blamed — never the reverse.
- Public docs: spec (language authority, §5.4 + `E0308`/`E0302`
  recorded), `stdlib.md` (8 modules + execution paths),
  `ffi.md` (rules + per-binding contracts + dispositions),
  `project.md` (modules/manifest/build/test), `fmt.md`, `lsp.md`
  (with verification-status disclosure). `stdlib.md`'s stale
  "build refuses FFI / 7 tests" lines corrected to the enabler
  behavior + 8/8.

## Stage 5 — real application, versioning, gates

- `apps/wordfreq`: concurrent word/line statistics over
  `input.txt` + `needle.txt` → `report.json`
  (`[lines,words,hits,bytes,needle,workers]` positional schema —
  `json_array` is infallible while `json_object` is `Result` and
  v1 has no `match`/`unwrap`). Two workers each scan a chunk
  (stdlib strings + per-line libc `c_strlen` FFI), summaries fan
  in over `chan<Array<int>>`, main aggregates and writes JSON.
  Partitioned input ⇒ deterministic under any `--workers`
  (verified 1/2/4 + built binary, all `[6,23,3,138,"toto",2]`,
  hand-counted: 18+31+23+25+21+20 bytes). No CLI args in v1, so
  paths are fixed; no `ffi_thread` (app stays buildable).
  Developed with `toto run`/`test`/`build`; `toto dev` does not
  apply (single-file scope, multi-file app). Tests
  (`tests/test_scan.gzv`: unit + integration, exit-code
  contract) and docs (`README.md`) committed; `toto test` and
  interpreter-vs-binary equivalence are now gate steps in
  `scripts/check.sh`.
- `toto dev` where applicable: inapplicable to the multi-file
  app (recorded, not skipped silently).
- Versioning policy (`docs/versioning.md`): SemVer for the
  stdlib (additive MINOR, discipline + review, no machinery);
  language editions reserved (spec + D1–D8 are the contract
  until editions exist); compiler keeps the exit-code contract
  across releases. No enforcement machinery (out of scope).
- Gates: full gate green (below); Tier-1/2 + end-to-end
  milestone re-runs green (gate includes the differential,
  corpus, concurrent, M:N hammer suites unchanged).

## Measurements (edit-to-rerun + dev-loop survival)

Cold-process rerun latency (`toto run`, parse+check+run, 3 runs
each, debug binary): small single-file program 10–16 ms, wordfreq
multi-file app 21 ms, stdlib smoke (7 imports) 18–19 ms. Labeled
honestly: process-spawn reruns, not dev-watch generations.

Dev-loop survival, real `toto dev` watch loop, 100 consecutive
edits (80 good + 20 broken, each awaited): 81 generations
started (1 initial + 80, exactly one per good edit — no misses,
no wedges, no duplicates), 20/20 broken edits caught with
`source has errors; waiting for a fix` and clean recovery on the
fix edit, program outputs exact (0,1,2,3,4,6,…,98 in order; the
81st generation was correctly preempted by the immediately
following broken edit before printing — drain-then-restart, not
a skip). Shutdown by signal, exit 143 (SIGTERM, by design).
Nothing deferred: both brief measurements are evidenced.

## Exact counts

- Corpus: 148 `.gzv` files = 139 entry programs (55 exit-0 /
  12 exit-1 / 72 exit-2 by `.status`) + 9 support files (project
  `lib/` modules). Phase 5 additions: `diag13`–`diag15` (static
  FFI rejections), `proj09_stdlib`, `proj10_stdlib_io`,
  `proj11_ffi` (runnable projects). No corpus program removed or
  re-goldened except `proj11`'s import line (module split).
- Cargo: `cargo test --workspace` green, 58 suites / 154 passed
  / 0 failed (includes the 39 spike tests, unchanged). New
  suites this phase: `fmt` (compiler), `fmt_equiv` (runtime),
  `lsp` (3 tests) + 2 FFI build CLI tests; `stdlib_table` pin
  8→9 for the `ffi_thread` split.
- `toto test` projects: stdlib 8/8, wordfreq 1/1, both in
  `scripts/check.sh`.
- Differential / corpus / concurrent / M:N pins: unchanged and
  green (Stage 3 pins 65→68 static hold).

## Deviations, assumptions, open questions

- `ffi`/`ffi_thread` split: a design correction inside frozen
  scope (no builtin/rule change), recorded in ADR-006.
- `read_message` hardening (skip malformed frames): robustness
  fix adjacent to the test-frame bug, pinned by a new test.
- `check.sh` gained the wordfreq steps (tests + build
  equivalence) and `apps/wordfreq/.gitignore` covers
  `target/` + `report.json`.
- Assumption: `toto build`'s shim needs `cargo` on PATH and the
  crates.io index (one network fetch on first build); CI
  provides both, verified green.
- Open: `toto dev` multi-file scope (carry-forward, owned by the
  dev-loop workstream); `close(2)` naming restriction (spec
  note stands until lifted); span-level race attribution
  (deferred, Enterprise scope).

## Carry-forwards (Phases 6–7, gated)

State-preserving hot reload / migration (Phase 6); LLVM AOT /
production AOT, package manager + registry (Phase 7); richer
LSP, linter, doc generator, coverage (Enterprise Wave 5);
HTTP client (was STRETCH, unneeded — the app never wanted it);
user generics/traits; general optimization framework;
benchmarks suite; anything from the Part 3 Enterprise Track.
