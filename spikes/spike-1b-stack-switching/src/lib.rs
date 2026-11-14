//! Spike 1b: real stackful switching under the Spike 1 GC contract.
//! ISOLATED PROTOTYPE — not the production runtime.
//!
//! # Safepoint protocol v2 (as implemented)
//!
//! Extends Spike 1 (allocation + step boundary) to allocation +
//! switch/yield boundary. Collection sees exactly: every task's
//! shadow stack, every inbox slot, every channel in-flight slot.
//! Suspended tasks are mid-computation on their own native stacks,
//! but the native stack is NEVER scanned — a handle sitting only in
//! a register or a stack slot does not keep its object alive.
//!
//! Task-code rules:
//!
//! 1. **Allocation safepoint.** Same as Spike 1: every allocator
//!    checks the threshold before inserting. Root each live handle
//!    before the next allocation.
//! 2. **Switch/yield boundary.** A task suspends only through
//!    `TaskRt::{yield_now, send, recv}`. Before suspending, every
//!    live handle must be published: on the task's shadow stack, in
//!    its inbox, or as the `send` payload (which the scheduler moves
//!    into channel/receiver roots). No heap reference may be held
//!    only in a register across a switch.
//! 3. **Enforcement.** Where checkable, the rule is a runtime assert
//!    at the suspend site: `send` panics if the value is not on the
//!    sender's shadow stack (publish-before-switch, enforced). Yield
//!    takes no payload, so nothing checkable crosses it: it is held
//!    by construction plus the missed-root control test, which shows
//!    an unpublished handle dying across a real switch while a
//!    published one survives.
//! 4. **Shadow-stack pointer at switch time.** Each task's
//!    `ShadowStack` lives in scheduler-owned memory (inside `World`),
//!    NOT on the task's native stack. A switch saves/restores the
//!    native stack pointer and callee-saved registers; the shadow
//!    stack needs no save/restore because it never moves —
//!    suspending only changes *which* shadow the running task is
//!    lent through its `TaskRt`. The "context" that crosses a switch
//!    is therefore: native stack (corosensei) + task id (which
//!    selects the shadow). Depth and contents are asserted equal
//!    across switches in both directions by the tests.

pub mod measure;
pub mod sched;
pub mod task;
pub mod world;

pub use sched::{JoinHandle, Scheduler, ToSched};
pub use task::{TaskRt, stack_probe};
pub use world::{ChannelId, TaskId};

/// Fixed 64 KiB default under test (provisional, awaiting
/// ratification — this spike measures against it, never ratifies it).
pub const DEFAULT_STACK_BYTES: usize = 64 * 1024;
