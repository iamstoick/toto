//! FFI smoke test: one tiny C file through hand-written
//! `extern "C"` declarations (stand-ins for future Toto
//! `unsafe extern` blocks). ISOLATED PROTOTYPE.
//!
//! D6 rules as implemented:
//!
//! - Rooting covers ONLY the duration of a call: heap objects are
//!   rooted in an explicit `ShadowStack` before deriving a pointer,
//!   and the borrow never outlives the call. Nothing is rooted
//!   implicitly.
//! - Retained pointers use the handle rule: C never sees a raw
//!   pointer past a call. Retention is an integer handle
//!   (`HandleTable`) whose root lives exactly as long as the entry.
//! - Callbacks are static `extern "C"` functions that touch only a
//!   plain message queue — no scheduler/GC re-entry. The Rust side
//!   drains the queue after the C call returns.
//! - Panic boundary: Rust logic behind an `extern "C"` boundary
//!   runs under `catch_unwind`; a panic becomes an error code and
//!   never unwinds through C.
//! - Strings are length-prefixed on the Rust side, copied +
//!   NUL-terminated for C, and validated as UTF-8 on the way back.
//!
//! Panic setting: this crate uses the default (`panic = "unwind"`),
//! which is what makes `catch_unwind` work. Under `panic = "abort"`
//! a Rust panic would terminate the process instead of converting
//! to an error — production panic policy is a Phase 5 decision.

pub mod boundary;

pub use boundary::{
    ERR_C_FAILURE, ERR_DIV_ZERO, FfiError, HandleTable, add, bad_bytes, drain_queue, emit_all,
    fallible_double, gc_collect, greet, read_first_byte, rust_div,
};
