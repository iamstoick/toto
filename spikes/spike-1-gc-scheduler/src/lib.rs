//! Spike 1: single-threaded cooperative scheduler + stop-the-world
//! tracing GC. ISOLATED PROTOTYPE — not the production runtime.
//!
//! See `src/runtime.rs` for the safepoint protocol as implemented.

pub mod heap;
pub mod roots;
pub mod runtime;

pub use heap::{GcRef, GcReport, HeapStats};
pub use roots::ShadowStack;
pub use runtime::{Action, ChannelId, Cx, JoinHandle, Scheduler, Step, TaskId};
