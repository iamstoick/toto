//! Per-task shadow-stack roots.
//!
//! Each task owns a `ShadowStack`: an explicit root stack the (spike)
//! task code maintains. Entering a nested call pushes a frame marker;
//! leaving it (via the `RootFrame` guard's `Drop`) pops every root the
//! frame registered. The collector only ever sees what is on these
//! stacks (plus channel in-flight slots and task inboxes), so a root
//! that is never pushed is a root the collector never sees — the
//! missed-root test pins this property down.

use crate::heap::GcRef;

/// Explicit per-task root stack. One per green-thread task; the
/// scheduler keeps each task's stack inside its own slot, which is what
/// preserves root state across context switches (a switch only changes
/// *which* slot the scheduler lends to the running task).
#[derive(Debug, Default)]
pub struct ShadowStack {
    roots: Vec<GcRef>,
    frames: Vec<usize>,
    max_depth: usize,
}

impl ShadowStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a frame, returning its base cookie. Pair with `close`.
    /// `Cx::with_frame` is the task-facing wrapper; this exists so the
    /// scheduler side and unit tests can manage frames without borrow
    /// gymnastics.
    pub(crate) fn open(&mut self) -> usize {
        let base = self.roots.len();
        self.frames.push(base);
        base
    }

    /// Close the frame opened by `open`, popping its roots. Frames must
    /// close in LIFO order (debug-asserted).
    pub(crate) fn close(&mut self, base: usize, frames_len: usize) {
        debug_assert_eq!(self.frames.len(), frames_len);
        debug_assert_eq!(self.frames.pop(), Some(base));
        self.pop_to(base);
    }

    /// Publish one live heap reference to the collector.
    pub fn push(&mut self, r: GcRef) {
        self.roots.push(r);
        self.max_depth = self.max_depth.max(self.roots.len());
    }

    /// Remove one occurrence (move-on-send: the sender gives up its root
    /// when the value is handed to the channel/receiver).
    pub fn remove_one(&mut self, r: GcRef) -> bool {
        if let Some(pos) = self.roots.iter().rposition(|&x| x == r) {
            self.roots.remove(pos);
            true
        } else {
            false
        }
    }

    pub fn contains(&self, r: GcRef) -> bool {
        self.roots.contains(&r)
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn open_frames(&self) -> usize {
        self.frames.len()
    }

    pub fn max_depth_observed(&self) -> usize {
        self.max_depth
    }

    pub fn iter(&self) -> impl Iterator<Item = GcRef> + '_ {
        self.roots.iter().copied()
    }

    fn pop_to(&mut self, base: usize) {
        debug_assert!(base <= self.roots.len());
        self.roots.truncate(base);
    }

    pub(crate) fn frames_len(&self) -> usize {
        self.frames.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;

    #[test]
    fn frames_pop_on_drop_nesting_stacks() {
        let mut heap = Heap::new();
        let a = heap.alloc_int(1);
        let b = heap.alloc_int(2);
        let mut sh = ShadowStack::new();
        sh.push(a);
        assert_eq!(sh.len(), 1);
        // Same machinery `enter`/`with_frame` use; the guard form
        // cannot be exercised here because it holds the borrow.
        let base = sh.open();
        let frames_len = sh.frames_len();
        sh.push(b);
        assert_eq!(sh.len(), 2);
        assert_eq!(sh.open_frames(), 1);
        let inner = sh.open();
        let inner_len = sh.frames_len();
        sh.push(a);
        assert_eq!(sh.len(), 3);
        assert_eq!(sh.open_frames(), 2);
        sh.close(inner, inner_len);
        assert_eq!(sh.len(), 2);
        assert_eq!(sh.open_frames(), 1);
        sh.close(base, frames_len);
        assert_eq!(sh.len(), 1);
        assert_eq!(sh.open_frames(), 0);
        assert!(sh.contains(a));
        assert!(!sh.contains(b));
        assert!(sh.remove_one(a));
        assert!(sh.is_empty());
        assert!(!sh.remove_one(a));
        assert!(sh.max_depth_observed() >= 3);
    }
}
