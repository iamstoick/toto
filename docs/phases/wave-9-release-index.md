# Wave 9 — versioned publishing: `toto release` + `toto index` (DESIGN)

Decision record (architect-owned on explicit user delegation
to build the registry scope). The v2.5 Group C item asked
for a registry with product decisions (hosting, auth,
namespace) that never came. The project's own observation
(backlog §3) dissolves the question instead: git remotes
the user already owns + the user-curated index + the
lockfile ARE the registry — no central service is invented,
no hosting/auth/namespace product is smuggled in. What was
actually missing is the author side (cutting a release) and
index curation (both hand-edited until now). This wave
builds exactly that; `toto publish` (local pack) is
unchanged.

## `toto release [--push]` (new; errors E0610)

Cut the manifest's version as a git release. Steps, in
order (first failure stops, nothing half-done: the tag is
created only after every check passes, the pack only after
the tag):

1. Manifest version must parse as strict semver
   (`Version::parse` — prereleases are unrepresentable, so
   any parsed version is releasable).
2. Project root must be a git repo root (`rev-parse
   --show-toplevel`, canonicalized, must equal the
   project root — subdirectory projects get a loud error,
   never a tag on the wrong tree).
3. Tree must be clean (`status --porcelain` empty apart
   from the release's own pack artifact, which is derived
   output; the error names up to 10 files plus the count).
4. Tag `v<version>`: absent → create annotated
   (`tag -a -m "toto release <name> <version>"`); present
   with unchanged release content → idempotent success
   ("already released"; the pack artifact and the project
   index don't count as content — neither ships); present
   with drifted sources → E0610 naming the files (bump the
   version or delete the tag by hand — tags are never
   moved).
5. Pack via `pack_pkg` (same artifact as `publish`),
   written next to the project; report tag + file.
6. Without `--push`, print the exact
   `git push <remote> <tag>` command (remote = current
   branch's upstream; none → print the generic form).
   With `--push`, push that single tag (never `--tags`);
   git's own stderr becomes the E0610 text on failure.

Exit 0 on released + already-released; exit 3 on every
refusal. No network except `git push` under `--push`
(and tag/ls operations never touch it).

## `toto index add|remove|list` (new; errors E0611)

Curate the PROJECT index (`./toto-index.toml` — the
checked-in, winning file) only; the user file stays
hand-managed. No network in any of them (validation is
syntactic; reachability is proven at add/update time by
the resolver, which already enumerates tags loudly).

- `add <name> <url>`: rejects empty/whitespace names and
  URLs and non-URL shapes (needs `://`, scp-like
  `user@host:path`, or a local path); same-name-same-url
  is idempotent success; same-name-different-url is a
  loud error (no silent rebind — suggest remove+add).
  Missing file is created with a `[packages]` table;
  existing content is preserved byte-for-byte (new line
  goes at the end of the `[packages]` section; malformed
  existing files refuse with the reader's error, never a
  rewrite).
- `remove <name>`: missing name is a loud error; removal
  is line surgery preserving comments, blanks, and order.
- `list`: prints the effective mappings grouped by file
  (project first, then user), missing files omitted.

## Non-goals

- No central registry, no upload, no auth, no namespace:
  documented in `package-manager.md`, not built.
- No `publish` change; no version ordering rules (an older
  tag is harmless — consumers pick highest-satisfying);
  no yank/deprecate (needs an authority that was
  deliberately not invented).
- No spec/checker/language change; the 27-builtin catalog
  untouched (`git` runs as a subprocess, like the
  resolver's `ls-remote`).

## Acceptance

- CLI tests: index round-trip (add/list/remove,
  duplicates, malformed), release happy path in a scratch
  git repo (tag created, pack written, push command
  reported), idempotent re-release, dirty-tree refusal,
  tag-taken refusal, non-repo refusal, `--push` to a local
  bare remote verified with `ls-remote`.
- `scripts/check.sh` package gate: offline-safe index
  round-trip + scratch-repo release (skipped nowhere —
  CI guarantees git).
- Docs: `package-manager.md` release + index sections,
  help text lines.
- Full gate green + CI green (recorded below on push).

## CI record

- Run 45965393378 (head `64cceda`, the Wave 9 commit):
  `check` FAILED — and only there. Every Rust test
  passed (including the 9 new registry tests and the
  coverage goldens); the failure was the new Wave 9
  gate block itself: `toto release` creates an annotated
  tag, which needs a committer identity, and CI runners
  have no global git identity (my machine does, which
  masked it locally). Diagnosis from the gate log, no
  product-code change needed.
- Commit `a21ab43` (gate-only fix: repo-local
  `git config user.email/name` before the scratch-repo
  release, the same hermetic pattern the registry
  tests use; verified locally in an identity-stripped
  environment), run 45972447313: SUCCESS — check +
  release-gates + bench + macOS/Linux-ARM probes green,
  Windows probe the known informational failure.
  Final tree CI-confirmed green.

## Local verification record (this turn, macOS ARM)

- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p toto-cli --all-targets`: zero
  findings in Wave 9 files (only the two pre-existing
  macOS-only `netsys.rs` findings via the compiler dep).
- New `registry` suite: 9/9 green. Existing suites
  unaffected: coverage 9/9, doc 3/3, lint 8/8, lsp 22/22,
  corpus 1/1, in-crate unit 22/22, packages non-build
  9/9. The only failures anywhere are the pre-existing
  sandbox set, byte-identical before and after this
  diff: 5x `cli.rs` build tests + 5x `packages.rs`
  build tests (`toto build` exits 3 — the AOT shim
  passes Linux-only `--disable-new-dtags` to macOS
  `ld`), plus the known SIGBUS guard-page test.
- Two self-found design corrections during the build,
  both with failing-then-green proof: (a) the release's
  own pack artifact must be exempt from the clean-tree
  check or no release can ever re-run; (b) tag equality
  must be content-based (diff pack/index-exempt), not
  HEAD equality, or committing the pack breaks
  idempotency and source drift under a tag reports
  success instead of E0610.
