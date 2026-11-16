# Versioning policy (Phase 5, Stage 5)

Three things version independently: the language, the compiler,
and the standard library. Only the stdlib's scheme is mechanical
today; the rest is policy for humans and `toto.toml`.

## Standard library: SemVer

`gzv.std` versions as `MAJOR.MINOR.PATCH` in lockstep with the
compiler release that embeds it (one repo, one release). Within a
major:

- PATCH: comment/wording fixes, new examples, performance-only
  changes to helpers. Drop-in safe.
- MINOR: new modules, new wrapper functions, wider input domains.
  Additive only: no signature changes, no removals, no behavior
  change on previously valid inputs.
- MAJOR: anything else — signature changes, removals, changed
  semantics, new reserved words that collide with module APIs.

Rationale: Toto programs name stdlib functions directly with no
import-version syntax (there is no package manager in v1), so the
only compatibility lever is stdlib discipline. The frozen-builtins
rule (new builtins need stop-and-ask) is the MAJOR gate for the
bottom layer; this policy is the gate for the Toto layer above it.

## Language and compiler

The language has no version number yet. Breaking language changes
arrive as **editions** (reserved, not designed): an edition names a
set of spec behaviors, the compiler accepts one edition per
program, and `toto.toml` will carry the edition when editions
exist. Until then, the spec (`docs/spec.md`) plus the frozen D1–D8
decisions are the compatibility contract — anything the spec
guarantees stays working.

The compiler (`toto` binary) reports its release with `toto
--version`. Compiler releases may change diagnostics text, error
codes for NEW errors, codegen, and timing, but never the exit-code
contract (0 main value / 1 panic / 2 static diagnostics / 3 CLI
errors) and never the observable behavior of spec-conforming
programs.

## What this does NOT do

No enforcement machinery: `toto init`/`toto build` do not check
versions, ranges, or lockfiles. No registry, no dependency
resolution (Phase 7 reserves the package manager). This document
is the whole mechanism — discipline plus review.
