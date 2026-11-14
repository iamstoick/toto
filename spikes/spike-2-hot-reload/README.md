# Spike 2 prototype: drain-and-restart hot reload (THROWAWAY)

**Isolated prototype, not the production runtime.** Proves the D7
mechanics — versioned dispatch, drain with quiescence detection,
drain timeout with scope-boundary cancellation, full-restart
fallback, reloadability classification, code-version lifetime —
with SIMULATED code versions on the Spike 1/1b runtime. No compiler,
no JIT, no file watcher, no state preservation.

- Scheduler, GC, channels, shadow stacks: reused from
  `spike-1b-stack-switching` (itself on `spike-1-gc-scheduler`) via
  path dependency. Zero 1b changes were needed; nothing forked.
- New here: version registry with pinned scopes (`src/registry.rs`),
  task context with scope boundaries (`src/hctx.rs`), and the reload
  protocol with timeout/cancel/restart (`src/runtime.rs`).
- Reloads are test-only (`reload_for_test`, `begin_reload`,
  `restart_for_test`); "code versions" are hand-built Rust function
  tables behind explicit version IDs.

See `docs/spikes/spike-2-hot-reload.md` for the verdict.
