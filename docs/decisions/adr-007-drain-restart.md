# ADR-007: Drain-and-restart hot reload (D7)

- Status: recommended default from Phase 0 Decision Doc (D1-D8).
- Date: 2026-11-14 (Phase 0, Spike 2).

## Decision

V1 hot reload is drain-and-restart ONLY: in-flight scopes finish on
the old code, then tasks restart on the new code. No state is
preserved across a reload in v1; opt-in declared state with migration
is a gated Phase 6 target. Production builds do not hot-reload.

## Context

D7 depends on D1 (GC retention), D2 (task stacks), and D8 (epoch tags
in object headers, code versioning). The open risks are runtime
mechanics, not UX: detecting quiescence, enforcing a drain timeout
with forced cancellation, retaining old code exactly as long as it is
reachable, and classifying changes without silent reinterpretation.

## Options considered

- **A. Full preservation for everything** (stacks, channels, pools).
  Rejected for v1: infeasible in general, silent corruption on type
  changes.
- **B. Drain-and-restart.** Correct by construction: in-flight work
  finishes, tasks restart on new code. Costs: in-memory state is lost
  unless the program saves it. **Chosen for v1.**
- **C. Opt-in preserved state** (declared `state` blocks with
  migration). Gated Phase 6 target, not v1.

## Safety rules (drain-and-restart)

- A defined drain timeout; the provisional `toto dev` default is 5 s
  (Phase 4 UX value, still awaiting ratification).
- Forced cancellation at the scope boundary when the timeout expires;
  tasks blocked on IO or channels must be cancellable.
- Fallback to a full dev-process restart with a clear message if
  drain cannot complete. External state kept through explicit
  reconnect hooks.

## Reloadability matrix

| Change | Outcome |
| --- | --- |
| Function body change | Reloadable (drain + new version) |
| Function signature change | Restart the affected scope |
| Struct field add/remove | Migrate or restart (v1: restart, never silently reinterpret) |
| Channel type change | Restart |
| Global initializer change | Restart |

## Code-version lifetime (v2.2)

GC reachability alone never unloads code. Old machine code is
retained until (a) no stack frame executes the old version and (b)
no reachable callable reference can invoke it. Drain quiescence:
code executing during drain must finish before the old version is
unloaded. Full lifecycle proof is a Phase 6 requirement.

## Spike 2 findings (appended; decision unchanged)

Spike 2 (`docs/spikes/spike-2-hot-reload.md`) demonstrated the
mechanism with simulated versions on the Spike 1/1b runtime:
version-pinned scopes (old frames finish on old code, new scopes use
new code); quiescence observed via frame counts including suspended
tasks; 100 ms test-timeout cancellation at scope boundaries with
respawn on the new version and clean state; full-restart fallback
with fresh heap/globals; all six matrix rows enforced (layout
mismatches detected via version tags before any field is read);
callable-held versions retained across quiescence and reclaimed
after release (exact GC counts asserted). No D1/D2/GC change was
needed. No amendment to the decision; the revisit trigger stands.

## Revisit trigger

If the Spike 0 work shows even function-body reload is unreliable
with the chosen codegen, fall back to fast restart only for v1 and
keep hot reload as a research item. Never ship a reload that can
silently corrupt state.
