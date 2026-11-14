//! Scheduler-owned shared state. Lives behind `Rc<RefCell<_>>` so
//! task bodies (running inside coroutines that can only capture
//! `'static` data) can reach the heap, their own shadow stack, and
//! the channels. Single-threaded: only the scheduler or one task
//! runs at a time, and no `World` borrow is ever held across a
//! suspend point — each `TaskRt` op takes and drops its borrow.

use spike_1_gc_scheduler::heap::{GcRef, GcReport, Heap};
use spike_1_gc_scheduler::roots::ShadowStack;

pub type TaskId = usize;

/// Unbuffered channel identifier (same semantics as Spike 1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChannelId(pub usize);

/// Rendezvous slot of one unbuffered channel. Public because it sits
/// in the public `World`; only the scheduler touches it.
#[derive(Debug)]
pub struct Channel {
    pub sender: Option<(TaskId, GcRef)>,
    pub receiver: Option<TaskId>,
}

pub struct World {
    pub heap: Heap,
    pub shadows: Vec<ShadowStack>,
    pub inboxes: Vec<Option<GcRef>>,
    pub channels: Vec<Channel>,
    pub log: Vec<String>,
    pub threshold: u64,
}

impl World {
    pub fn new() -> Self {
        Self {
            heap: Heap::new(),
            shadows: Vec::new(),
            inboxes: Vec::new(),
            channels: Vec::new(),
            log: Vec::new(),
            threshold: u64::MAX,
        }
    }
}

/// Mark from every published root (task shadows, inboxes, channel
/// in-flight slots) and sweep the rest. All tasks are parked at
/// safepoints when this runs: true stop-the-world on one OS thread.
pub fn collect_roots(world: &mut World) -> GcReport {
    let mut roots = Vec::new();
    for s in &world.shadows {
        roots.extend(s.iter());
    }
    for r in world.inboxes.iter().flatten() {
        roots.push(*r);
    }
    for ch in &world.channels {
        if let Some((_, r)) = ch.sender {
            roots.push(r);
        }
    }
    world.heap.gc_collect_for_test(&roots)
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
