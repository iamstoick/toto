# Toto (.gzv) — language implementation in Rust

> Phase 0 (foundations + de-risking spikes) work in progress.
> See [Implementation Roadmap v2.2](docs/) and `docs/decisions/`.

## Layout

- `crates/toto-runtime/` — production runtime scaffold (empty until Phase 3).
- `crates/toto-compiler/` — production compiler scaffold (empty until Phase 1/2).
- `crates/toto-cli/` — `toto` CLI scaffold (empty until Phase 1/4).
- `spikes/spike-1-gc-scheduler/` — **isolated prototype** for Spike 1
  (single-threaded cooperative scheduler + stop-the-world tracing GC).
  Throwaway code: it may be deleted, and it is NOT the production runtime.
- `spikes/spike-1b-stack-switching/` — **isolated prototype** for
  Spike 1b (real stackful switching via corosensei on the Spike 1 GC
  contract). Same throwaway status.
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

Repo host is not known yet, so there is no CI config; `scripts/check.sh`
is the gate (fmt check + clippy + test). This assumption is recorded in
the Spike 1 report.

## License

TODO: license not chosen. Ask the project owner before publishing anything.
