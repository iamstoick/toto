//! Task-facing context: the only way task code touches the heap, its
//! own shadow stack, channels, or the log.
//!
//! `TaskRt` carries the coroutine `Yielder`, so suspending is a method
//! call (`yield_now` / `send` / `recv`) that works from any call depth —
//! nested functions just take `&TaskRt`. All methods take `&self`:
//! shared state goes through short `RefCell` borrows that are never
//! held across a suspend point.

use std::cell::RefCell;
use std::rc::Rc;

use corosensei::Yielder;
use spike_1_gc_scheduler::heap::GcRef;

use crate::sched::ToSched;
use crate::world::{ChannelId, TaskId, World, collect_roots};

#[derive(Clone)]
pub struct TaskRt<'a> {
    world: Rc<RefCell<World>>,
    task: TaskId,
    yielder: &'a Yielder<(), ToSched>,
}

impl<'a> TaskRt<'a> {
    pub(crate) fn new(
        world: Rc<RefCell<World>>,
        task: TaskId,
        yielder: &'a Yielder<(), ToSched>,
    ) -> Self {
        Self {
            world,
            task,
            yielder,
        }
    }

    fn with_world<R>(&self, f: impl FnOnce(&mut World) -> R) -> R {
        f(&mut self.world.borrow_mut())
    }

    /// Nested root frame on this task's shadow stack. The closure form
    /// keeps `RefCell` borrows short: the frame opens, `f` runs (each
    /// op borrowing freshly), then the frame closes.
    pub fn with_frame<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        let (base, frames_len) = self.with_world(|w| {
            let s = &mut w.shadows[self.task];
            (s.open(), s.frames_len())
        });
        let r = f(self);
        self.with_world(|w| w.shadows[self.task].close(base, frames_len));
        r
    }

    /// Publish one live heap reference. Must precede the next safepoint.
    pub fn root(&self, r: GcRef) {
        self.with_world(|w| w.shadows[self.task].push(r));
    }

    pub fn shadow_len(&self) -> usize {
        self.world.borrow().shadows[self.task].len()
    }

    pub fn shadow_contains(&self, r: GcRef) -> bool {
        self.world.borrow().shadows[self.task].contains(r)
    }

    fn allocation_safepoint(&self) {
        let mut w = self.world.borrow_mut();
        if w.heap.bytes_since_gc() >= w.threshold {
            collect_roots(&mut w);
        }
    }

    fn alloc<F>(&self, f: F) -> GcRef
    where
        F: FnOnce(&mut spike_1_gc_scheduler::heap::Heap) -> GcRef,
    {
        self.allocation_safepoint();
        self.with_world(|w| f(&mut w.heap))
    }

    pub fn alloc_int(&self, v: i64) -> GcRef {
        self.alloc(|h| h.alloc_int(v))
    }

    pub fn alloc_str(&self, v: &str) -> GcRef {
        self.alloc(|h| h.alloc_str(v))
    }

    pub fn alloc_pair(&self, head: GcRef, tail: GcRef) -> GcRef {
        self.alloc(|h| h.alloc_pair(head, tail))
    }

    pub fn alloc_array(&self, elems: &[GcRef]) -> GcRef {
        self.alloc(|h| h.alloc_array(elems))
    }

    /// Allocate and root atomically: no safepoint can intervene between
    /// the insert and the publish.
    pub fn alloc_rooted_int(&self, v: i64) -> GcRef {
        let r = self.alloc_int(v);
        self.root(r);
        r
    }

    pub fn alloc_rooted_pair(&self, head: GcRef, tail: GcRef) -> GcRef {
        let r = self.alloc_pair(head, tail);
        self.root(r);
        r
    }

    /// Suspend until the scheduler requeues this task.
    pub fn yield_now(&self) {
        self.yielder.suspend(ToSched::Yield);
    }

    /// Move `value` to a receiver (parks if none waits). Enforces
    /// publish-before-switch: panics if the value is not published on
    /// this task's shadow stack.
    pub fn send(&self, ch: ChannelId, value: GcRef) {
        assert!(
            self.shadow_contains(value),
            "publish-before-switch: send of unpublished root",
        );
        self.yielder.suspend(ToSched::Send(ch, value));
    }

    /// Take a sender's value (parks if none waits). The value lands in
    /// this task's inbox root; root it before the next safepoint.
    pub fn recv(&self, ch: ChannelId) -> GcRef {
        self.yielder.suspend(ToSched::Recv(ch));
        self.take_inbox()
            .expect("scheduler delivers the value before resume")
    }

    /// Take the value delivered to this task's inbox. The inbox slot
    /// is a GC root while occupied.
    pub fn take_inbox(&self) -> Option<GcRef> {
        self.with_world(|w| w.inboxes[self.task].take())
    }

    pub fn heap_get_int(&self, r: GcRef) -> Option<i64> {
        self.world.borrow().heap.get_int(r)
    }

    pub fn heap_get_pair(&self, r: GcRef) -> Option<(GcRef, GcRef)> {
        self.world.borrow().heap.get_pair(r)
    }

    /// Copy a heap string out (the `RefCell` borrow cannot outlive
    /// the call, so `&str` cannot be returned).
    pub fn heap_get_string(&self, r: GcRef) -> Option<String> {
        self.world.borrow().heap.get_str(r).map(str::to_owned)
    }

    pub fn heap_desc_name(&self, r: GcRef) -> Option<&'static str> {
        self.world.borrow().heap.desc_name(r)
    }

    /// Append to the scheduler's deterministic event log.
    pub fn log(&self, msg: impl Into<String>) {
        self.with_world(|w| w.log.push(msg.into()));
    }

    /// TEST-ONLY forced collection from inside a task. Same hook as
    /// `Scheduler::gc_collect_for_test`.
    pub fn gc_collect_for_test(&self) -> spike_1_gc_scheduler::heap::GcReport {
        collect_roots(&mut self.world.borrow_mut())
    }
}

/// Address of a fresh local: approximates the current native stack
/// pointer. Tests use it to prove tasks run inside their own stack
/// ranges and to cross-check measured high-water marks.
pub fn stack_probe() -> usize {
    let x = 0u8;
    std::ptr::addr_of!(x) as usize
}
