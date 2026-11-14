//! Spike 2: drain-and-restart hot reload with simulated code
//! versions. ISOLATED PROTOTYPE — not the production runtime.
//!
//! # Reload protocol as implemented
//!
//! Versions are explicit IDs over hand-built function tables. A task
//! enters a scope pinned to the current version (`Hctx::in_scope`);
//! entering roots the version's heap token in the task shadow and
//! counts a frame, exiting releases both — including across
//! suspensions, since guards live on native stacks. New scopes bind
//! the new version; old frames finish on the old one.
//!
//! Drain waits for quiescence (no frames) plus no callable refs,
//! then retires: the registry drops the version and the holder task
//! unroots its token, so the next forced GC reclaims it. The drain
//! timeout cancels precisely the tasks still pinning the old version
//! (tracked per restartable entry); they exit at the next scope
//! boundary and are respawned on the new version with clean state.
//! Layout-typed changes skip drain and restart the whole (simulated)
//! process instead: fresh scheduler, fresh heap, new initializers.
//!
//! The native stack is never scanned; version tokens and callables
//! are ordinary heap objects rooted exactly for their lifetime.

pub mod hctx;
pub mod registry;
pub mod runtime;

pub use hctx::{Cancelled, Hctx, LayoutMismatch, Scope};
pub use registry::{ChangeKind, Disposition, Func, FuncId, VersionId, classify};
pub use runtime::{EntryFn, HotRuntime, Outcome, PendingReload, PumpResult, ReloadReport};
