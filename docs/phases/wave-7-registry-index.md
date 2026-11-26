# Wave 7 — package index (decentralized registry)

Decision record (2026-11-26; architect-owned on explicit user
delegation to finish the leftovers). The registry gap is real
but narrower than "build a registry": everything exists
except the name→URL mapping and tag enumeration. This wave
adds exactly that — no central server, no new auth model, no
namespace ownership, no manifest/lockfile format change.

## Problem

`http = "2"` (bare version requirement) is valid manifest
syntax today but always fails at resolve time (E0606: "no
registry to fetch it from"). Git deps work but demand an
exact `--rev`/`--tag` pin per dependant — no version
selection, no sharing. `toto publish` is local-only.

## Design (no code yet)

- **Index files, user-curated** (the namespace):
  `./toto-index.toml` (project, checked in — wins on
  conflict) then `~/.toto/index.toml` (personal). Flat
  strict-subset TOML, same hand-rolled reader style as
  `toto.toml` (no new dependency):
  ```toml
  [packages]
  http = "https://github.com/example/toto-http"
  ```
  No versions in the index (selection stays in
  requirements + lockfile, single source of truth each).
  No auth model beyond git transport as today (explicit
  non-goal: no new auth, no accounts, no central hosting).
- **New `DepSource::Index { name }`** (manifest spelling
  unchanged: bare `http = "2"` keeps rendering as today).
  `toto add http [--version REQ]` (no source flags) writes
  the bare requirement; resolution consults the index.
- **Tag enumeration** (the only new network behavior):
  `git ls-remote --tags <url>` at add/update time (fetch);
  warm-cache `git tag --list` when offline (fetch=false).
  Annotated tags peeled via `^{}`; one leading `v`
  stripped; non-semver tags ignored (documented); zero
  usable tags is a loud E0602-style error naming the URL.
- **Version from tag, verified**: candidates get
  `version = tag-derived`, `source = Git{url, rev:hash}`
  with NO per-tag checkout. After selection, the winner is
  materialized through the existing exact-rev path and its
  manifest version must EQUAL the tag version (loud error
  otherwise — tags cannot lie). Then the existing flow
  takes over unchanged: highest satisfying wins, lockfile
  pins (version, url+rev), offline builds from lock+cache.
- **E0606 message** gains the index hint (both index paths).
- **Unchanged**: manifest format, lockfile format, pin
  semantics, `update`/`publish`/build/test flows, offline
  guarantees,qa the `toto add --path/--git` paths.

## Design refinements (pre-implementation; all verified against the tree)

- Version always comes from the package manifest AT the
  tag (read after checkout), never from the tag name —
  non-semver tag names simply never match a requirement;
  tags without a readable manifest are skipped
  (documented). No tag-name trust, no manifest/tag
  mismatch class.
- Checkout-per-tag reuses the existing `materialize_git`
  path (correct by construction, cache-friendly across
  runs). Cost accepted + documented: each release tag
  checks out once at add/update time. Worktree/archive
  fast paths are named future work, not this wave.
- `toto add <bare> [--version REQ]` writes the bare
  requirement unchanged (no manifest format change);
  resolution consults project `./toto-index.toml`, then
  `~/.toto/index.toml`. Unpinned `--git` WITHOUT a version
  keeps E0605 exactly (test-pinned); unpinned `--git`
  WITH `--version` enumerates like an index hit (same
  helper, same rules).
- Index files use a flat `[packages]` table
  (`name = "git-url"`), parsed by a small strict reader in
  house style (no new dependency). Selection, pinning,
  lockfile, offline builds: untouched existing machinery.

## Why not a central registry

Requires product decisions the user has not made (who runs
it, who pays, who owns names, auth model). The index
delivers versioned sharing with zero new trust: the mapping
is data the user already controls (like `.gitmodules`),
transport auth is git's, and every versioned artifact is
still pinned by hash in the lockfile. A central index
remains a possible later layer on the same candidate
machinery — explicitly not this wave.

## Acceptance

- `toto add <name> [--version REQ]` resolves through a
  project index against local git fixtures (file:// or
  plain-path remotes — no network in tests), picks the
  highest satisfying tag, writes the bare requirement and
  a pinned lock entry; `toto update` picks up newer tags
  with the existing lock-diff report.
- Unindexed name still E0606 (now naming both index files);
  non-semver-only repos error loudly naming the URL.
- Tag/manifest version mismatch errors loudly.
- Offline `toto build`/`toto test` work from lock+cache
  with no index present (index only consulted at
  add/update time).
- Full gate green + CI green; docs updated
  (`docs/package-manager.md` index section, `toto add`
  usage line); no new top-level command.

## Non-goals

Central hosting, accounts/billing, namespace governance,
`publish --to`, version ranges beyond semver `Req`,
non-git transports, index signature verification (hashes
in the lockfile already authenticate content).

## Implementation record (2026-11-26; CI pending)

Built as designed with two deviations recorded here:
(1) version comes from each tag's manifest (not the tag
name), so no tag-trust issue and no mismatch check was
needed — invalid manifests fail loud, manifest-less tags
are skipped with counts in the E0602;
(2) unpinned `git` + version also enumerates (same helper;
E0605 stays exactly for unpinned without a version, which
an existing test pins).
New: `index.rs` (strict `[packages]` reader, project-wins
lookup), tag enumeration (`ls-remote --tags` fetch /
warm-cache union offline), per-tag materialize via the
existing path, Bare + unpinned-with-version resolve paths,
E0606 index hint, `toto add` bare-name write-through.
Verified locally: 6 index unit + 4 index integration tests
(local git repos, hermetic, no network), full packages
suite (only the 5 pre-existing env failures: AOT/network),
fmt + clippy clean (real 1.100 oracle; only pre-existing
netsys warnings). Phase B-equivalent follow-through (docs
`package-manager.md` + HELP) included in this commit.
