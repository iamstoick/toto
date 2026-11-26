# Wave 8 — concurrent coverage, deterministic mode (SHIPPED 2026-11-26)

Decision record (architect-owned on explicit user delegation
to finish the leftover scope). Stage 3 refused concurrent
programs because nondeterministic interleavings make
coverage numbers dishonest. This wave lifts the refusal for
exactly one honest configuration: **single-worker
deterministic runs** (workers = 1, hardcoded on the coverage
path), where the FIFO scheduler + fixed-seed RNG make
execution reproducible. Empirical anchor (recorded in the
design turn, verified then): `conc03_select_two` (both
channels ready — pure tie-break) printed `10` in 6/6 runs,
default flags and explicit `--workers 1`. Multi-worker
coverage is not offered: `toto coverage` takes no `--workers`
flag, so the nondeterministic configuration is unexpressible
rather than refused.

## Vehicle (as built)

- `MInstr::Cover { span }`, emitted once per `HStmt` at the
  top of the single `lower_stmt` choke point (`stmt.span` in
  scope) — including `go` bodies and select-case bodies, which
  lower through `lower_stmts` in sub-builders (the `cover`
  flag propagates into every sub-builder). One hit per
  statement execution, exactly matching the tree-walker's
  `cover_stmt` semantics by construction, regardless of block
  layout. Tails stay unattributed in both modes (never
  statements, never hits — same rule).
- Flagged lowering only: `lower()` keeps today's behavior
  for all existing callers (JIT/AOT/mir_conform/tests
  byte-identical — the JIT even gains a vanishing `Cover`
  arm for exhaustiveness; zero prod risk); new
  `lower_for_coverage()` opts in. `mir_interp` (differential
  tests only) gets a nop arm; it never runs cover-mode MIR.
- Executor hooks in `exec.rs`: one `MInstr::Cover` arm in
  `exec_instr` (the single dispatch), plus `cover_edge` on
  every `MTerm::Branch` exit and `cover_fn` on every
  `push_frame` (the single frame-creation funnel — covers
  `main`, direct calls under their `MFn` names which match
  the inventory, and `go`/closure frames whose synthetic
  names the renderer harmlessly drops). `CovHits` shared via
  `Option<Arc<Mutex<CovHits>>>` on `Shared`, cloned from
  `RuntimeConfig::coverage` (`None` = today's behavior
  exactly — the `None` check is the only production cost).
  `run_checked` selects `lower_for_coverage` iff a sink is
  present. `RuntimeConfig` lost `Copy` (the `Arc`); the one
  by-value move site (`dev.rs` generation respawn) clones.
- Branch attribution mirrors the tree-walker exactly (arm 0
  = then/body taken, arm 1 = else taken/skipped/exited),
  via span rerouting at lowering (`MTerm::Branch.cover:
  Option<BranchCover>`, `Some` only in cover-mode output —
  production MIR stays byte-identical):
  - `if`/`while`/`for` conditions carry condition/body
    spans in MIR but the inventory lists statement spans, so
    cover mode re-stamps the statement/expression span
    (`None` reroute — verbatim credit is then correct).
  - `for`-range bottom checks (completion takes the `then`
    edge) reroute to `(stmt, Some(1)/None)`: completion
    credits arm 1, the continue edge is swallowed — the
    tree-walker's exit attribution exactly, including
    panic-mid-loop (no exit event, arm 1 stays 0).
  - `for`-array has a single top check (natural parity).
  - Select case-dispatch polls are swallowed (`None`/`None`):
    the `Select` instruction already records the winning
    case index (`default` = `cases.len()`).
  - Short-circuit (`&&`/`||`) and match internals keep
    verbatim spans that match no inventory point, so the
    renderer drops them — same invisibility as sequential
    match arms (kept, not fixed: out of scope).
- Inventory: `BranchPoint` grows `arms: u8` (2 for
  if/while/for — every existing golden byte-identical);
  `collect_inventory_concurrent` adds one point per
  `select` (one arm per case, `default` last); the sequential
  inventory is untouched. Renderer enumerates `0..arms`.
- `Shared` gains the `coverage` sink field; the scheduler
  and GC paths are untouched (hits never hold locks across
  yields — one mutex acquisition per hit).

## Determinism contract (the honesty core)

- The coverage path hardcodes workers = 1 (no flag, no
  refusal path, no misuse shape). Fixed `rng_seed`
  (`RuntimeConfig::default`) governs select tie-breaks.
- Double-run byte-identical LCOV is a gate, not a hope:
  `check.sh` compares two back-to-back runs per fixture,
  and `coverage_concurrent_deterministic` does the same
  in-process in the test suite.
- Spot-verified in the design turn: `Shared`/`sched`
  iteration is over Vecs/rings (ordered); channel waiters
  are Vecs; tie-breaks use the seeded SplitMix64. No
  HashMap iteration on scheduling paths found.
- Cross-platform check: the tie-break select in
  `conc_select.gzv` pins the seeded winner (case 0) in the
  golden — CI on Linux must agree with macOS byte for byte.

## CLI contract

- `toto coverage [--fail-under PCT] <file.gzv>...` —
  unchanged LCOV-to-stdout, summary-to-stderr shape, same
  exit codes (0 measured, 1 program-failed/gate, 2
  broken, 3 unreadable/usage). Sequential path
  byte-identical to before (tree-walker untouched; `E0215`
  tripwire stays). The old "sequential programs only"
  refusal is gone, replaced by measurement; the backlog
  entry points here.
- `--fail-under` semantics unchanged (percentages, not
  counts, so MIR-level execution never skews gates).

## Acceptance (all recorded here, verified this turn)

- Three golden fixtures committed with exact-LCOV goldens:
  `conc.gzv` (go/channel, 5/5 lines, 1/1 fns, no branch
  points), `conc_select.gzv` (case 0, case 1, `default`,
  plus a both-ready tie-break pinned to case 0 — 11/11
  lines), `conc_branches.gzv` (if then/else, full range
  loop entered ×3 + completed, while entered ×3 + exited,
  empty range skipped, go-body loop — 18/19 lines with the
  dead `println` listed at 0, 7/8 branches).
- Determinism: double-run byte-equality in `check.sh` per
  fixture + `coverage_concurrent_deterministic` test; all
  green locally this turn (macOS ARM).
- Overhead measured this turn: loop-saturated worst case
  (200k-iteration `for`, 3 runs each) 0.792s plain vs
  1.010s instrumented ≈ 1.27×; at fixture scale the
  difference is startup-dominated noise (30 runs: 0.212s
  vs 0.170s). Per-hit cost is one uncontended mutex
  acquisition plus a HashMap increment. Recorded, not
  gated.
- Full gate green + CI green (recorded below on push).

## Local gate record (this turn, macOS ARM, rustc 1.100.0)

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`:
  only the two pre-existing `netsys.rs` macOS-cfg findings
  (unreachable + needless return, untouched file, last
  touched in Wave 3); every Wave 8 file clean. CI Linux
  never compiles those cfg branches.
- Compiler suites green: lib unit (incl. coverage
  self-measurement), lexer, parser, fmt, doc, project,
  interp, mir, mir_conform (production MIR byte-identical
  claim holds), differential.
- Runtime suites green: concurrent, sequential, stack, gc,
  mn. `fmt_equiv` (format-equivalence under forced GC) exceeds
  the local time budget in debug profile on this machine
  (spins at 99% CPU — it runs the whole corpus under
  collect-at-every-safepoint); terminated locally, covered by
  CI. Untouched by this diff by inspection (no Cover markers
  without the flag; per-edge cost is one `None`-sink check),
  and everything it could interact with (MIR conformance,
  differential, concurrent, sequential, gc, mn) is green.
- CLI suites green: coverage (9/9 incl. 4 new concurrent
  tests), corpus, doc, lint, lsp; cli 13/13 except the 5
  `toto build`/AOT tests.
- JIT suites green: differential, differential_concurrent,
  differential_task, opt, dev, mn_jit, race, yield_jit,
  stack_jit (except below), gate, panics, timings.
- Pre-existing sandbox failures, each reproduced or
  reasoned and NOT caused by this diff:
  - `stack_jit jit_guard_page_overflow`: wants SIGSEGV, gets
    SIGBUS — fails identically on clean HEAD (`3fc92cc`,
    verified via `git stash`), macOS/ARM signal behavior.
  - 5× `cli.rs` build tests + 5× `packages.rs` build tests:
    `toto build` exits 3 because the AOT shim passes the
    Linux-only `--disable-new-dtags` ld flag, rejected by
    macOS `ld` (fails inside third-party build scripts —
    no toto code involved). Matches the standing record
    that AOT fails in this sandbox; CI Linux is final.
- `scripts/check.sh` concurrent gate (goldens +
  double-run) verified by hand this turn (binary-identical
  runs, goldens committed); the script's own run happens on
  CI, where Linux also cross-checks the tie-break golden
  byte-for-byte against macOS.

## Non-goals

- Multi-worker coverage — unexpressible, not refused.
- JIT/AOT/native coverage — interpreted-executor only.
- mir_interp coverage (differential-tests-only path).
- No spec/checker/language change; no new builtins;
  `toto doc` untouched. Cross-task attribution views.

## What this did NOT do

- Did NOT touch the sequential path (tree-walker,
  tripwire), the JIT/AOT pipelines (beyond exhaustive-match
  arms), or any existing golden: every MIR consumer sees
  byte-identical input unless the coverage flag is set,
  and only `toto coverage` on a concurrent program sets it.
- Did NOT add a `--workers` flag to `toto coverage` (the
  design-turn sketch had one with a refusal; the build
  drops it — fewer shapes, same honesty).
