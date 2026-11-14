//! Spike 3: Cranelift JIT from a toy IR against the Spike 1 GC.
//! ISOLATED PROTOTYPE — not the production runtime.
//!
//! # Internal ABI as exercised (D8 pieces, toy scale)
//!
//! - **Value representation:** untagged native. Ints are i64.
//!   Handles (`GcRef`) cross the boundary as u64 bit patterns
//!   (`(generation << 32) | index`); object bytes are never touched
//!   by generated code — all heap access goes through runtime entry
//!   points operating on the shared Spike 1 heap (descriptors in
//!   headers, traced by the normal marker).
//! - **Runtime context:** every JIT function takes `ctx: i64` first —
//!   a raw pointer to the owning `JitRuntime` (heap + shadow stack).
//!   Runtime helpers take it too. No hidden globals.
//! - **GC-root publication:** generated code publishes via
//!   `toto_shadow_push`; unrooted values stay in JIT SSA values and
//!   are collectable (the native stack is never scanned).
//! - **Safepoint rule for generated code:** every allocation helper
//!   checks the byte threshold BEFORE inserting (same rule as Spike
//!   1); `toto_gc_collect` is an explicit safepoint. Live values
//!   must be published (pushed) before either.
//! - **Calling convention:** Cranelift's default (platform ABI is
//!   Cranelift's job). Root discipline for calls: callees push,
//!   callers/top-level reset (see `toy.rs`).
//!
//! Arithmetic uses Cranelift's default wrapping semantics
//! (`iadd`/`isub`/`imul`); checked overflow is NOT implemented here.

pub mod lower;
pub mod runtime;
pub mod toy;
