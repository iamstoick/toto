//! Full Cranelift backend (Phase 4, Stage 1): compile real MIR
//! (`parse_and_check` + `mir::lower` output — never hand-built IR)
//! for ALL MIR constructs and execute it against the production heap
//! through the frozen ADR-008 entry points.
//!
//! Two modes share one lowering core. Gate mode (the Phase 3 Tier-2
//! gate, grown up) runs sequential programs on a standalone context
//! with diverging panics; task mode runs any program (including
//! concurrent ones) on scheduler tasks with sentinel panics and
//! suspension. The 8 gate tests pin the gate contract throughout.

pub mod compile;
pub mod dev;
pub mod runtime;
pub mod task;

pub use compile::{Jit, OptLevel, SubsetError};
pub use runtime::{JitRt, decode, encode};
pub use task::{JitSession, run_jit};
