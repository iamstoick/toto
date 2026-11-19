# Security policy (Enterprise Wave 0 — APPROVED 2026-11-19: user approval of the verdict and policy numbers as written; disclosure process live)

## Where to report

Use **GitHub private vulnerability reporting** on the Toto
repository (Security tab → "Report a vulnerability"). Reports
stay private between the reporter and the maintainers; do not
open a public issue for a suspected vulnerability. If private
reporting is unavailable, contact the maintainers through the
repository's listed contact and ask for a secure channel —
do not include exploit details in a public message.

## What to include

- The version or commit under test (`toto --version` output,
  or the commit hash).
- The platform (OS, architecture) and how Toto was built
  (release binary, `cargo build`, `toto build` output).
- Steps to reproduce, with the smallest program that shows
  the issue (`.gzv` source preferred over prose).
- Impact as you see it: what an attacker gains (code
  execution, information disclosure, denial of service, trust
  boundary crossed) and who is affected (authors of Toto
  programs, operators running them, end users of built
  binaries).
- Your contact and whether you want credit, plus any
  embargo needs on your side.

## Handling process

1. **Acknowledge** receipt (PROPOSED: within 2 business days —
   flagged for approval at acceptance review).
2. **Triage**: confirm, severity, affected versions. The
   reporter gets the severity assessment and the planned
   course (patch, mitigation, or wont-fix with rationale).
3. **Fix** on a private branch; regression test added to the
   suite (the fix ships with its proof, per project
   discipline).
4. **Coordinated disclosure**: release the patch, publish an
   advisory, then disclose publicly. The reporter is credited
   unless they decline.
5. Timeline proposals (flagged, not committed): acknowledgement
   2 business days; severity assessment 5 business days;
   patch for critical issues 30 days. These numbers take
   effect only on user approval — until then they are the
   proposal under review.

## Scope

Toto-the-toolchain (compiler, runtime, JIT, CLI, stdlib,
build/packaging) and the release artifacts (binaries, SBOMs,
attestations). Reports about example programs in `apps/` are
welcome but triaged as samples, not product. Out of scope:
third-party crates in the dependency tree (report upstream;
we upgrade and backport), the reporter's own Toto programs.

## No bounty program

There is no paid bounty. Credit in the advisory and our
thanks are what is offered.
