# Toto (.gzv) — language implementation in Rust

> Enterprise Wave 1 (networking substrate, crypto, context) complete —
> ACCEPTED-WITH-FLAGS: networking substrate + crypto shipped, context
> ADR delivered; poller proven under M:N at workers 2/3/4; macOS
> Tier-1 green, Windows deferred with evidence.
> Gate green (62 result lines, 225 tests, 0 failed).
> See [Wave 1 report](docs/phases/enterprise-wave-1-networking-crypto-context.md).
> ADR-009 and spec §17 approved by the user 2026-11-21.
> Roadmap: [Implementation Roadmap v2.5](docs/) (D1–D8 + ratified decisions; Enterprise Track gated waves).

> Enterprise Wave 0 (production foundations) complete —
> CONDITIONAL: Linux x86_64 + macOS ARM64 + Linux ARM64
> supported, Windows deferred with evidence (POSIX FFI
> surface). GC targets published and measured; policies,
> signing/SBOM pipeline in place.
> Gate green (61 result lines, 224 tests, 0 failed).
> See [Wave 0 report](docs/phases/enterprise-wave-0-production-foundations.md).
> Roadmap: [Implementation Roadmap v2.5](docs/) (D1–D8 + ratified decisions; Enterprise Track gated waves).

## Policies (Enterprise Wave 0 — approved 2026-11-19)

- [Stability](docs/stability.md) — what is covered, what is not.
- [Deprecation](docs/deprecation.md) — notice, migration path, period.
- [Support window](docs/support.md) — N/N−1 minors, 12 months.
- [Security](SECURITY.md) — private disclosure process.
- [GC](docs/gc.md) — collector targets, knob, workloads.
- [Releases](docs/release.md) — attestations, SBOM, reproducibility.

## Layout

- `crates/toto-runtime/` — production runtime (GC heap, corosensei
  scheduler, channels/select/scopes, MIR executor, deterministic
  mode). No `Rc`/`RefCell`; all cross-task state is `Arc`+`Mutex`.
- `crates/toto-compiler/` — lexer, parser, diagnostics, tree-walking
  interpreter, plus the Phase 2 checker, shared `ops`, HIR, MIR, and MIR
  interpreter (differential-tested against the tree-walker).
- `crates/toto-cli/` — `toto run [--workers N]`, `toto dev [--workers N] [--drain-timeout SECS] [--opt-level LEVEL]` + `--help`/`--version`.
- `crates/toto-jit/` — full Cranelift backend (Phase 4): real MIR → production heap, Gate + Task modes, `toto dev` driver, race detector, opt pipeline. Conformance gate grown up.
- `spikes/spike-1-gc-scheduler/` — **isolated prototype** for Spike 1
  (single-threaded cooperative scheduler + stop-the-world tracing GC).
  Throwaway code: it may be deleted, and it is NOT the production runtime.
- `spikes/spike-1b-stack-switching/` — **isolated prototype** for
  Spike 1b (real stackful switching via corosensei on the Spike 1 GC
  contract). Same throwaway status.
- `spikes/spike-2-hot-reload/` — **isolated prototype** for Spike 2
  (drain-and-restart reload mechanics on the Spike 1/1b runtime).
  Same throwaway status.
- `spikes/spike-3-cranelift/` — **isolated prototype** for Spike 3
  (Cranelift JIT from toy IR against the Spike 1 heap).
  Same throwaway status.
- `spikes/ffi-smoke-test/` — **isolated prototype** for the D6 FFI
  smoke test (one C file through `extern "C"` declarations).
  Same throwaway status.
- `docs/decisions/` — ADRs (D1–D8 as ratified).
- `docs/spikes/` — spike reports.
- `scripts/check.sh` — runs `cargo fmt --check`, `cargo clippy`, `cargo test`.

## Toolchain

- Latest stable Rust. Recorded: **rustc 1.100.0** (`stable-x86_64-unknown-linux-gnu`).
- Workspace edition: **2024**.
- Spike 1 uses **zero external dependencies** (std only); see the spike
  report for the context-switching crate comparison.

## Checks

```sh
./scripts/check.sh
```

CI runs the same gate on Linux x86_64 via
[ci.yml](/home/gerald/Apps/toto/.github/workflows/ci.yml) (push and pull
request): `scripts/check.sh` (fmt check + clippy + test).

## License

Dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
See `license = "MIT OR Apache-2.0"` in each crate manifest.
