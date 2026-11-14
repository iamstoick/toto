//! Tier-2 ABI gate (Phase 3, Stage 3): compile a documented SUBSET of
//! real MIR with Cranelift and execute it against the production heap
//! through the frozen ADR-008 entry points.
//!
//! This is a conformance gate, NOT the Phase 4 JIT backend: the subset
//! is exactly what the Tier-2 list needs (integer arithmetic with
//! explicit checks, calls, one heap family with element access, root
//! publication at safepoints, panic paths, coroutine-stack execution).
//! Anything outside the subset is refused with a precise error; Phase 4
//! extends coverage from here.

pub mod compile;
pub mod runtime;

pub use compile::{Jit, SubsetError};
pub use runtime::{JitRt, decode, encode};
