# Release signing, SBOM, and reproducibility (Enterprise Wave 0, Stage 4)

## Mechanism chosen: GitHub artifact attestations (Sigstore-backed)

Releases are signed with `actions/attest-build-provenance`,
which binds each artifact to its exact build provenance
(commit, workflow, runner) through Sigstore — no long-lived
keys to custody, rotate, or leak. Verification is one command
(`gh attestation verify <file> --owner <org>`) with no key
distribution. Rationale against the alternatives:

- **GPG rejected**: key custody and rotation burden for a
  pre-1.0 project with no release engineers; verification
  requires out-of-band key discovery.
- **cosign rejected**: overlaps attestations feature-for-feature
  here while adding key management; revisit if artifacts ever
  ship outside GitHub (registries that speak OCI natively).

## Pipeline (`.github/workflows/release.yml`)

Trigger: version tags (`v*`) for real releases; `workflow_dispatch`
with `dry_run=true` for evidence runs (builds, SBOMs, attests —
skips the publish step, cuts no tag, uploads nothing). Steps per
platform: checkout → stable toolchain → `cargo build --locked
--release -p toto-cli` → SPDX SBOM of the binary (`anchore/sbom-action`)
→ attest provenance → (real runs only) attach to the GitHub Release.

Releases ship for **supported Tier-1 platforms only** (Stage 1
verdicts decide the matrix; Linux x86_64 is the baseline).

## SBOM: SPDX

Format: **SPDX** (ISO/IEC 5962). Rationale: ISO standard,
GitHub-native (dependency-graph ingestion, advisories
matching), tool-agnostic readers everywhere. Generated per
release binary by the SBOM step above. Coverage: the full
cargo dependency closure of the shipped binary (name, version,
license, supplier where recorded) plus the toolchain version;
system libraries of the runner image are recorded as
`externalRef`s where the generator captures them, not
audited line-by-line (the SBOM is an inventory, not a vetting).

## Reproducibility

Carries over from Phase 7: the production build was proven
byte-identical (full sha256 of the named artifact, two builds
same lockfile). The release pipeline adds no transformation —
checkout → `cargo build --locked --release` → SBOM/attest
ALONGSIDE the binary, never modifying it — so pipeline
reproducibility reduces to build reproducibility (proven) plus
SBOM determinism. End-to-end proof bar: two dry-run pipeline
executions on identical inputs must produce byte-identical
binaries; SBOM timestamps are the known non-determinism source
(record the outcome per run in the Wave 0 report — if SBOMs
differ only in timestamps, that is stated, not hidden).
