# Toto (.gzv) — language implementation in Rust

> Phase 1 (vertical slice, runnable core) complete: `toto run` executes
> the core subset via a tree-walking interpreter with a 50-program corpus.
> See [Implementation Roadmap v2.3](docs/) and `docs/decisions/`.

## Layout

- `crates/toto-runtime/` — production runtime scaffold (empty until Phase 3).
- `crates/toto-compiler/` — lexer, parser, diagnostics, tree-walking
  interpreter (Phase 1 core subset; checker + IR arrive in Phase 2).
- `crates/toto-cli/` — `toto run` + `--help`/`--version` (Phase 1).
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
