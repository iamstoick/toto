# Toto package manager (v1, local-only)

No public registry. No remote publish. Everything below works
offline once packages are in the local cache.

## Manifest (`toto.toml`)

```toml
[package]
name = "myapp"
version = "0.1.0"

[dependencies]
greet = { path = "../greet" }          # local directory
util  = { git = "https://example/u", rev = "v1.2.0" }  # tag or commit
tool  = { package = "tool-1.2.0.gzv-pkg" }             # packed file
```

No `[dependencies]` table means exactly today's behavior:
single-project build, no resolution, no lockfile.

- Requirement strings use `^` / `~` / `=` only
  (`^1.2.3`, `~1.2.0`, `=1.2.0`). No pre-releases, no build
  metadata. `0.x` is NOT special-cased: `^0.2.3` means
  `>=0.2.3, <0.3.0` like any other minor range.
- A git source MUST pin `rev` to a tag or a commit hash.
  A branch name, `HEAD`, or a missing `rev` is an add-time
  error. A requirement with no source at all is an add-time
  error too.

## Resolution

One version of each package per project (single-version
rule). The resolver takes every requirement on a package —
direct requirements from your manifest plus transitive
requirements read from each selected dependency's own
`toto.toml` — and picks the HIGHEST version satisfying all
of them. Same inputs always produce the same lockfile.

A conflict (no version satisfies every requirement) is a
loud error naming every requirement and where it came from.
A dependency cycle is an error. A dependency's own
`[dependencies]` table is always honored (full transitive
resolution); there is no direct-only mode.

## Lockfile (`toto.lock`, checked in)

Exact versions only, plus for each package the source and a
content hash over its `.gzv` files. The lockfile is the ONLY
source of versions at build time.

- Missing lockfile at build/test/run: the toolchain resolves
  and writes one, LOUDLY (stderr notice). Creating a lockfile
  may use the network; building from an existing lockfile
  never does.
- Lockfile/manifest disagreement (a requirement the locked
  version no longer satisfies, a removed or added dependency):
  ERROR, never a silent re-resolve. Run `toto update` to
  re-resolve.

## Sources

- `path`: a directory containing `toto.toml` + `.gzv` files.
- `git`: fetched at add/update time into the local package
  cache. At BUILD time a locked git dependency resolves from
  the cache only — no network. If the locked revision is not
  in the cache, the build fails loudly naming the missing
  revision. Fetching happens at add/update time, never at
  build time.
- Packed `.gzv-pkg` files (see `publish`): `toto add` unpacks
  one into the local cache and records it as a path source.

## Imports

Relative `./` and `../` imports are unchanged. `gzv.std/`
is reserved. A bare import's first segment names a declared
dependency (`import greet` → the `greet` package). If that
first segment matches BOTH a declared dependency AND a
project-root path, that is a loud error naming both — adding
a dependency never silently rebinds an existing import.
Two packages exporting the same top-level name is an error.

## Commands

- `toto add <spec>`: add a dependency (path dir, `git+URL@rev`,
  or `.gzv-pkg` file), re-resolve, rewrite the lockfile.
- `toto update`: re-resolve ALL requirements to the highest
  satisfying versions, rewrite the lockfile loudly (prints
  added/changed/removed, or "already up to date").
  `toto update <pkg>` re-resolves only that package's
  subtree; requirements outside it act as pins (an
  unsatisfiable pin rejoins free selection with its bounds
  still enforced).
- `toto publish`: packs the manifest + `.gzv` files (never
  `target/`, never the lockfile) into a `.gzv-pkg` file for
  local sharing. Nothing is uploaded anywhere.

## Offline behavior

Warm the cache while online (`toto add` / `toto update` /
one online build), then build offline: `toto build --offline`
uses the lockfile + cache only. A cold cache with `--offline`
fails loudly naming what is missing. See
[production-builds](production-builds.md) for the full
warm-cache workflow.

## Failure modes (all loud, exit 3)

Ambiguous import, version conflict, lockfile disagreement,
changed content at a locked version, unpinned git, missing
cached revision, unknown package on update — every one names
what failed and why. There is no silent fallback, no silent
default, no silent re-resolve.
