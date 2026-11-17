# Production builds (`toto build`)

Production = Cranelift AOT only (LLVM stays cut, Phase 7).
`toto build` emits an Off object (debug) or a Speed object
with a release-profile runtime link (`--release`). Production
builds carry no dev machinery and do not hot-reload.

## Offline builds (warm-cache workflow)

A production build performs **no network fetch** when the
cargo package cache is warm. `toto build --offline` passes
`--offline` to the inner cargo link, whose own offline gate
hard-errors on any network attempt — exit 0 proves zero
network dependence. Dependency resolution is pinned: `toto
build` copies the workspace `Cargo.lock` into the link shim,
so two builds can never resolve different registry versions.

What must be warmed: the cargo package cache for the
workspace dependency closure (registry index + `.crate`
files, same versions as the workspace `Cargo.lock`).

How: run any one online `toto build` (or `cargo fetch` in the
workspace) with network access. CI images and builder
machines warm once; the dev flow keeps
fetch-once-then-cached.

Cold-cache failure (exact, captured 2026-11-17): with an
empty `CARGO_HOME`, `toto build --offline` fails:

```text
toto: linking <name> (debug) ...
error: no matching package named `corosensei` found
location searched: crates.io index
required by package `toto-jit v0.1.0 (...)`
    ... which satisfies path dependency `toto-jit` (locked to 0.1.0)
    of package `toto-app-shim v0.0.0 (...)`
note: offline mode (via `--offline`) can sometimes cause surprising
resolution failures
help: if this error is too confusing you may wish to retry without `--offline`
toto: link failed (cargo exited exit status: 101)
```

(exit 3). Note the lockfile IS honored even cold ("locked to
0.1.0") — the missing piece is the `.crate` files, never
the resolution. A cold cache fails loudly; it never silently
fetches. Cold-cache offline builds are NOT supported (see
below).

## Reproducibility

Same inputs → byte-identical outputs. Method: build twice
from the same tree and compare the FULL sha256 of the named
artifact (`target/<name>`). Proven in the Phase 7 report
(three `toto build --release --offline` runs — cold link,
incremental rebuild, clean-shim rebuild — identical hash).

## Vendoring deferral (2026-11-17)

No `vendor/` directory, no prebuilt runtime link: the
lockfile pin + `--offline` already makes production builds
network-free without committing ~hundreds of MB of registry
sources into the repo. Hermetic cold-cache builds are
deferred — no current requirement. Revisit if a hermetic
cold-cache build is ever required.

## Fingerprinted rebuilds

`toto build` keys its outputs (`target/<name>` + object) on
a content fingerprint (every loaded source file, manifest,
lockfile, opt level, toto version; see the Phase 7 report)
stored at `target/.toto-fingerprint`. An unchanged project
prints a fresh-skip message instead of relinking.
