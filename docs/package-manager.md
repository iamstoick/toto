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
- A git source MUST pin `rev` to a tag or a commit hash...
  unless it carries a version requirement: unpinned
  `git = "URL"` plus `version = REQ` enumerates the repo's
  release tags and selects the highest satisfying one (same
  as an index-mapped name, below). A branch name or `HEAD`
  is always an error, pinned or not. A requirement with no
  source at all is an add-time error too, unless an index
  entry maps the name.

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

- Bare names via a package index: `./toto-index.toml` in the
  project root (checked in, wins), then
  `~/.toto/index.toml` (personal). Each maps a name to a git
  URL (`[packages]` table, `name = "url"` lines; `#`
  comments and blank lines allowed). `toto add leftpad`
  (optionally `--version REQ`) resolves through the index:
  release tags are enumerated (`git ls-remote --tags`, or
  the warm cache offline), non-version tags are skipped,
  and the highest satisfying tag wins. Tags without a
  package manifest are skipped (reported when nothing
  usable remains). Unmapped names are E0606 naming both
  index files; the index is consulted at add/update time
  only — builds use the lockfile and never read it.
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

## Release (publishing without a registry)

There is no central registry to publish TO — the release
IS the git tag, on a remote the author already owns:

- `toto release [--push]`: cut the manifest version as tag
  `v<version>` (E0610 family). Validates the version,
  requires a clean tree at the repo root (the release must
  match a commit; the tool's own pack artifact is exempt),
  creates the annotated tag, packs the `.gzv-pkg` (same
  artifact as `publish`), and prints the exact
  `git push <remote> <tag>` command. With `--push`, pushes
  exactly that tag (never `--tags`) to the current
  branch's upstream.
- Re-running is idempotent when the release content is
  unchanged ("already released" — committing the pack
  file or curating the index afterwards still counts).
  Changed sources under an existing tag are an error
  naming the files (bump the version or delete the tag
  by hand; tags are never moved).
- Consumers resolve the tag through the index + version
  requirements exactly like any other git source
  (highest satisfying tag wins); nothing is uploaded
  anywhere by toto itself.

## Index curation

The project index (`./toto-index.toml`) is user-curated
with `toto index` (E0611 family, project file only — the
user file stays hand-managed, no network in any of them):

- `toto index add <name> <url>`: append the mapping
  (creates the file with a `[packages]` table when
  missing). Same mapping twice is idempotent success; a
  different URL for an existing name is an error (no
  silent rebind — `remove` first).
- `toto index remove <name>`: drop the entry line,
  preserving comments, blanks, and order byte-for-byte.
  Missing names are an error.
- `toto index list`: print the effective mappings grouped
  by file (project first, then the user file when present).

## Commands

- `toto add <spec>`: add a dependency (bare indexed name
  with optional `--version REQ`, path dir, `git+URL` with
  `--rev`/`--tag` (or `--version` to select a tag), or
  `.gzv-pkg` file), re-resolve, rewrite the lockfile.
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

## Failure modes (all loud)

Ambiguous import renders through the existing diagnostic
pipeline (exit 2); version conflict, lockfile disagreement,
changed content at a locked version, unpinned git, missing
cached revision, and unknown package on update are CLI
errors (exit 3). Every one names what failed and why. There
is no silent fallback, no silent default, no silent
re-resolve.
