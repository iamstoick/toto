# Support window (Enterprise Wave 0 — PROPOSED, awaiting user approval)

## The promise (PROPOSED — user approval required)

- Each **minor release line** (`1.x`) is supported for
  **12 months** after the next minor ships (i.e. N and N−1
  are supported; N−2 is end-of-life).
- **Patch releases** (`1.x.y`) are cumulative bugfix-only
  updates to their minor line; the latest patch of each
  supported minor is the supported artifact.
- **Security fixes** are backported to every supported minor;
  unsupported lines get an upgrade advisory, not a patch.
- End-of-life is announced in the release notes of the
  release that starts the 12-month clock AND in the final
  patch of the retiring line.

"Supported" means: security backports, critical-bug
backports at maintainer discretion, and a CI-tested upgrade
path to the current minor. It does not mean new features on
old lines.

## What this assumes

A tagged-release practice that does not exist yet (Wave 0
Stage 4 proves the signing/SBOM pipeline by dry run only;
no real release is cut in this wave). The window above
takes effect from the first tagged release; until then the
only supported artifact is `main` tip. If the release
cadence turns out faster or slower than annual-ish minors,
revisit the 12-month number — it is the commitment being
asked for, flagged here rather than buried.

## Rationale

Fleets cannot track tip. N/N−1 with a year of overlap lets a
service skip one minor without ever running unsupported,
while bounding backport work to two lines. Tying EOL to an
announcement in both the new release and the retiring line's
last patch means no one discovers EOL from a missing patch.
