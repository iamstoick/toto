//! Shadow-stack roots (D1, frozen ADR-008): one explicit root stack
//! per task, published before every safepoint.
//!
//! The MIR executor uses rebuild-scan discipline: before any safepoint
//! (allocation threshold check) or park request, it clears its slot and
//! republishes every live heap reference from its frames. The set
//! published is exactly what manual rooting would publish — publication
//! is automatic rather than instruction-driven — so a missed-root
//! control cannot exist at this layer (there is no path that holds a
//! live heap reference off-stack). Tests assert the observable
//! properties instead: live values survive forced collections across
//! suspensions, unreachable objects are reclaimed with exact counts.

use crate::heap::GcRef;

/// One task's published roots. Single-threaded Stage 1: owned by the
/// shared table, written only by its own task before parking or
/// collecting, read only during a stop-the-world collection.
#[derive(Clone, Debug, Default)]
pub struct ShadowStack {
    roots: Vec<GcRef>,
}

impl ShadowStack {
    pub fn new() -> Self {
        Self { roots: Vec::new() }
    }

    pub fn publish(&mut self, roots: Vec<GcRef>) {
        self.roots = roots;
    }

    /// Push one root (codegen-emitted publication, ADR-008: the
    /// backend publishes each live handle around safepoints; the MIR
    /// executor keeps its rebuild-scan `publish` discipline).
    pub fn push(&mut self, root: GcRef) {
        self.roots.push(root);
    }

    /// Release roots pushed after `len` (generated epilogues). Never
    /// panics: truncating past the end is a no-op, so a codegen bug
    /// here leaks roots instead of corrupting state (tests assert
    /// exact depths, so leaks surface immediately).
    pub fn pop_to(&mut self, len: usize) {
        self.roots.truncate(len);
    }

    pub fn clear(&mut self) {
        self.roots.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = GcRef> + '_ {
        self.roots.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}
