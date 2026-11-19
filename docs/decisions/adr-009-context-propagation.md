# ADR-009 (PROPOSED): Context propagation — values, deadlines, cancellation

- Status: **PROPOSED — user approval is the Wave 1 exit gate.** No
  implementation exists; implementation rides with the consuming
  waves (starting Wave 2). Nothing here changes D1–D8, the frozen
  D8 ABI, the 27 frozen builtins, the Phase 5 module model, or spec
  semantics — a change to any of those still needs STOP-and-ask
  and its own ADR.
- Date: 2026-11-20 (Enterprise Wave 1, Stage 3).
- Requires: Wave 1 Stage 1 (M:N scheduler with scopes, green
  threads, channels) as built; spec §17.5 (no timeouts on v1 net
  calls — timeouts interact with this decision).

## Problem (from the v2.5 Wave 1 brief)

Logging (Wave 2) and tracing (Wave 4) both need request-scoped
values (request IDs, span IDs), deadlines, and cancellation to
flow through concurrent Toto code. Each library must not improvise
its own answer — the decision is made here, deliberately, once.

## Decision

Context is an **immutable library value** (`gzv.std/context`,
Wave 2) propagated by **spawn inheritance**, cancelled through
the **existing scope-failure machinery**, with **deadlines as
absolute monotonic-millisecond values** checked at suspension
points once the Wave 2 clock lands.

Concretely:

1. **Values.** `Context` is a user-level struct: a string→string
   map plus an optional deadline. `with_value(ctx, k, v)` returns
   a derived context; `get(ctx, k)` reads. Values are explicit
   data — no new builtin, no type-system change (the struct shape
   is settled in Wave 2; this ADR fixes the semantics, not the
   field list).
2. **Flow through `go` blocks: inherit at spawn.** A child task
   starts with its parent's context (a runtime task-slot
   reference, GC-rooted like any other task state — a Wave 2
   scheduler change, specified here). Rationale: the task tree
   IS the request tree, so inheritance is the only default that
   cannot silently drop tracing. Opting out (`go_detached`) is
   explicit and recorded in Wave 2.
3. **Flow through channels: NOT implicit.** A message received
   under context B by a task spawned under context A is governed
   by A — contexts do not ride messages by default (receiver
   scope is what deadlines and cancellation act on; implicit
   message transfer creates ordering hazards and scope confusion).
   Message-scoped causality (`send_with(ctx, ch, v)`) is an
   opt-in helper for tracing edges, specified in Wave 2.
4. **Cancellation = scope failure.** Every `go` block already
   runs in a child scope, and scope failure already releases
   parked tasks with fail-fast at the next suspension point
   (`E0214` discipline). `with_cancel()` returns `(ctx, handle)`;
   `cancel(handle)` fails that scope through a small Wave 2
   runtime hook (`cancel_scope` — the only new runtime
   capability this design needs, named here, built in Wave 2).
   In-flight channel/poller waits unwind exactly like a scope
   panic does today.
5. **Deadlines = carried values, enforced at suspension points.**
   `with_deadline(ctx, after_ms)` stores an absolute
   monotonic-millisecond deadline. Every suspension point (park,
   yield, poller wait) compares it against the monotonic clock
   and fails the scope with deadline-exceeded when passed —
   the same fail-fast path as cancellation. The CLOCK SOURCE
   itself is a Wave 2 deliverable (shared with logging
   timestamps; `docs/time.md` records that v1 has no wall
   clock): Wave 1 fixes the value shape and the check points,
   Wave 2 wires the clock. Until the clock lands, deadlines are
   carried, not enforced — stated here, not discovered later.
6. **No timeout parameters on v1 net calls.** Spec §17.5 stands:
   `net_*` takes no timeouts. Timeouts arrive as context
   deadlines enforced at the poller park once the Wave 2 clock
   exists — not as per-call parameters.

## What logging (Wave 2) builds on

- Request/correlation IDs as context values (present at every
  log call site through spawn inheritance — no threading).
- The Wave 2 monotonic clock (shared with deadline checks).
- Scope cancellation as the log-flush signal (cancelled scopes
  flush before release).
- The `cancel_scope` hook and the task-slot context field
  (Wave 2 runtime work, specified by this ADR).

## What tracing (Wave 4) builds on

- Span/parent IDs as context values with spawn inheritance
  (trace trees follow the task tree for free).
- `send_with` for cross-task causality edges (explicit,
  auditable — never silent).
- Deadline/cancellation as span-closure signals (a cancelled
  scope closes its spans with cause).
- Suspension-point checks as the span-boundary discipline.

## Options considered

- **A. Go-style explicit `ctx` parameters on every function.**
  Rejected: invasive to every signature in every library,
  trivially droppable (a forgotten parameter silently breaks
  traces), and hostile to the existing no-parameter channel
  ops. Explicitness is kept where it matters (`send_with`,
  `go_detached`) instead of demanded everywhere.
- **B. Implicit dynamic scope (global current-context).**
  Rejected: unsound under the M:N scheduler — a "global"
  context is either sharedmutable state across workers (races)
  or worker-local state that migrates with stolen tasks (wrong
  request). Task slots are the only sound home.
- **C. Contexts ride channels by default (message-scoped).**
  Rejected as the default (kept as `send_with`): when tasks
  multiplex messages from many requests, message-scoped
  context makes deadline/cancellation act on the wrong scope,
  and reordered messages apply stale deadlines. The receiver's
  spawn context governs; message causality is opt-in.
- **D. Defer the whole design until the clock exists.**
  Rejected: logging and tracing need the value shape and the
  flow rules NOW to design against. Carried-not-enforced
  deadlines are the honest interim, recorded here.
- **E. Deadlines as relative durations re-armed per hop.**
  Rejected: per-hop re-arming silently extends end-to-end
  budgets across fan-out (the classic deadline bug). Absolute
  monotonic timestamps compose; durations do not.

## Consequences

- Wave 2 implements: `gzv.std/context`, the task-slot context
  field (+ GC rooting), the `cancel_scope` hook, the monotonic
  clock, suspension-point deadline checks, `send_with` /
  `go_detached`. No Wave 1 code changes (nothing to change —
  there is no implementation in this wave).
- The M:N scheduler gains per-task context state: a Wave 2
  design detail owned by that wave, constrained by this ADR
  (inherit at spawn, GC-rooted, never global).
- Net-call timeouts are NOT added in Wave 1 (§17.5 stands);
  any per-call timeout proposal is a new gated proposal that
  must reconcile with this ADR.

## Explicitly NOT decided

- The exact `Context` struct field list (Wave 2 API design,
  following this ADR's semantics).
- The monotonic clock source/precision (Wave 2; shared with
  logging timestamps).
- `cancel_scope`'s exact signature and error codes (Wave 2).
- Context persistence across state reload (D7; out of scope).
- Cross-process / cross-service propagation format (Wave 5+
  territory; this ADR covers in-process tasks only).
- Whether `go_detached` needs a capability/permission model
  (Wave 2 call; default is inherit).

## Open questions for acceptance review

1. Is spawn-inheritance (vs explicit threading) the approved
   default? 2. Is absolute-monotonic-ms the approved deadline
   representation? 3. Is Wave 2 the approved owner of the
   clock, the slot field, and the cancel hook? Approval of this
   ADR answers all three; implementation follows in Wave 2.
