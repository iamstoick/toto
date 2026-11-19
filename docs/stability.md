# Stability policy (Enterprise Wave 0 — PROPOSED, awaiting user approval)

What the project promises not to break, and what it explicitly
reserves the right to change. Takes effect from the first
tagged release; until then everything is pre-1.0 and may
change with a report entry.

## Covered (breaking changes follow the deprecation policy)

- **Language surface**: the `docs/spec.md` grammar, semantics,
  and the exit-code contract (0 = `main` return, 1 = panic,
  2 = static diagnostics, 3 = CLI errors). The frozen 25
  builtins keep their names, arities, and semantics.
- **`gzv.std`**: module paths (`gzv.std/...`), public function
  names, signatures, and documented behavior. Additive-only:
  new modules and new functions may appear in any release;
  removals and signature changes are breaking.
- **External C FFI ABI** (D6 boundary): the declared `extern`
  function signatures user programs bind against, the string
  rule (UTF-8, length-prefixed, NUL-terminated copy at the
  boundary), error-code/last-error slots, the handle rule for
  retained pointers, and panic boundaries (no unwind through C).
- **Package manager surface**: `toto.toml`/`toto.lock` formats
  (backward-compatible reads: old lockfiles keep working),
  `add`/`update`/`publish` semantics, SemVer resolution rules.
- **CLI surface**: `toto run/build/test/dev/init` subcommands
  and flags that exist at 1.0; output formats meant for humans
  (diagnostic text) are NOT covered, exit codes are.

## Explicitly NOT covered

- Compiler internals: HIR/MIR shapes, `mir_cases` dumps,
  interpreter internals, checker implementation details.
- **Internal runtime ABI**: the `toto_*` helper set, value
  traffic conventions, entry-point lists beyond the frozen
  D8 contract — D8 stays frozen per ADR-008, but "frozen"
  means "no silent change", not "public API": it may evolve
  only through a new ADR with stop-and-ask.
- Diagnostic message wording and the exact set of error
  codes: new codes arrive in fresh ranges (precedent:
  E06xx, E0311–E0326); existing codes keep their meaning.
- `toto dev` behavior details (single-file-only is a known
  open limitation), benchmark absolute numbers, timing.
- Anything marked experimental in its own doc.

## Rationale

Services pin toolchains; silent breakage in the language,
stdlib, FFI boundary, or package format destroys that pin.
Internals stay free so the implementation can improve
(Cranelift backend evolution, GC tuning) without a
compatibility vote each time. The boundary is the user
contract; everything behind it is ours to change loudly.
