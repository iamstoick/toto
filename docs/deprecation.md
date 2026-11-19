# Deprecation policy (Enterprise Wave 0 — PROPOSED, awaiting user approval)

## The rule

Nothing covered by the stability policy is removed or
incompatibly changed without: (1) a deprecation notice, (2) a
migration path, (3) the notice period below.

1. **Notice**: the deprecation is announced in the release
   notes AND as a compiler diagnostic (`toto run`/`build`
   warns on use of the deprecated item, naming the
   replacement) starting no later than the first release
   containing the replacement.
2. **Migration path**: every deprecation ships documented
   before/after examples; codemod guidance where mechanical.
   A deprecation without a worked migration path is not
   accepted — it stays a proposal.
3. **Notice period (PROPOSED — user approval required)**:
   the longer of **90 days** or **one minor release** after
   the notice first ships, before removal. Security-driven
   removals may shorten this to 30 days with a written
   rationale in the release notes; the shortened clock never
   goes below the next patch release.

## What this does not do

- Experimental or explicitly uncovered surface needs no
  deprecation (it was never promised).
- New additions are never deprecations.
- The timetable is the commitment being asked for: if 90
  days / one minor is wrong for how Toto ships, say the
  number before approving. Until approval, no covered item
  is removed, period.

## Rationale

Fleet upgrades are planned, not improvised. A notice with a
working migration path turns a breaking change into a
scheduled task; a bare removal turns it into an incident.
The compiler warning (not just release notes) matters
because it reaches every build, not just readers of notes.
