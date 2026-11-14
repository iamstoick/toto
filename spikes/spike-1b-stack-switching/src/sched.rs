//! Single-threaded cooperative scheduler over real native stacks.
//!
//! Each task owns a `corosensei` coroutine (own stack, own instruction
//! pointer, callee-saved registers). The scheduler loop resumes
//! coroutines round-robin; a task suspends only via `TaskRt`
//! (`Yield` / `Send` / `Recv`), which is a safepoint: everything the
//! task still needs is published to scheduler-owned roots (its
//! shadow stack, its inbox, or the channel slot) before the switch.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use corosensei::stack::{DefaultStack, Stack as _};
use corosensei::{Coroutine, CoroutineResult};
use spike_1_gc_scheduler::heap::{GcRef, GcReport, HeapStats};

use crate::measure;
use crate::task::TaskRt;
use crate::world::{Channel, ChannelId, TaskId, World, collect_roots};

/// What a task asks the scheduler when it suspends. Suspending is
/// itself the safepoint: the scheduler may collect before resuming
/// anyone, so every live handle must already be published.
pub enum ToSched {
    /// Suspend; requeue at the back.
    Yield,
    /// Rendezvous `GcRef` to a receiver (parks if none waits). The
    /// sender's root is moved into channel/receiver roots.
    Send(ChannelId, GcRef),
    /// Take a sender's value (parks if none waits).
    Recv(ChannelId),
}

/// Join token. Joining drives the scheduler round-robin until the
/// named task completes; it is idempotent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct JoinHandle {
    task: TaskId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskState {
    Ready,
    Blocked,
    Done,
}

struct Slot {
    name: String,
    co: Option<Coroutine<(), ToSched, (), DefaultStack>>,
    state: TaskState,
    stack_base: usize,
    stack_limit: usize,
    writable_start: usize,
    stack_len: usize,
    high_water: Option<usize>,
}

pub struct Scheduler {
    world: Rc<RefCell<World>>,
    slots: Vec<Slot>,
    ready: VecDeque<TaskId>,
    switches: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            world: Rc::new(RefCell::new(World::new())),
            slots: Vec::new(),
            ready: VecDeque::new(),
            switches: 0,
        }
    }

    /// Auto-collect once `bytes_since_gc` reaches this at an allocation
    /// or switch safepoint. Default is "never" so tests stay
    /// deterministic unless they opt in.
    pub fn with_threshold(self, bytes: u64) -> Self {
        self.world.borrow_mut().threshold = bytes;
        self
    }

    pub fn spawn(
        &mut self,
        name: &str,
        stack_bytes: usize,
        body: impl FnOnce(TaskRt<'_>) + 'static,
    ) -> JoinHandle {
        let id = self.slots.len();
        {
            let mut w = self.world.borrow_mut();
            w.shadows
                .push(spike_1_gc_scheduler::roots::ShadowStack::new());
            w.inboxes.push(None);
        }
        let stack = DefaultStack::new(stack_bytes).expect("mmap task stack with guard page");
        let base = stack.base().get();
        let limit = stack.limit().get();
        let writable_start = measure::writable_start(limit);
        assert!(
            writable_start < base,
            "stack must have usable space above the guard page",
        );
        // Pattern-fill before first resume for high-water measurement.
        unsafe { measure::fill_pattern(writable_start, base) };
        let world = Rc::clone(&self.world);
        let co = Coroutine::with_stack(stack, move |y, ()| {
            body(TaskRt::new(world, id, y));
        });
        self.slots.push(Slot {
            name: name.to_owned(),
            co: Some(co),
            state: TaskState::Ready,
            stack_base: base,
            stack_limit: limit,
            writable_start,
            stack_len: base - writable_start,
            high_water: None,
        });
        self.ready.push_back(id);
        JoinHandle { task: id }
    }

    pub fn channel(&mut self) -> ChannelId {
        let mut w = self.world.borrow_mut();
        let id = w.channels.len();
        w.channels.push(Channel {
            sender: None,
            receiver: None,
        });
        ChannelId(id)
    }

    pub fn task_count(&self) -> usize {
        self.slots.len()
    }

    pub fn is_done(&self, h: JoinHandle) -> bool {
        self.slots[h.task].state == TaskState::Done
    }

    pub fn event_log(&self) -> Vec<String> {
        self.world.borrow().log.clone()
    }

    pub fn heap_stats(&self) -> HeapStats {
        self.world.borrow().heap.stats()
    }

    /// Native stack range (top, guard-page base) of a task, recorded
    /// at spawn. Used to prove tasks run on separate stacks.
    pub fn stack_range(&self, h: JoinHandle) -> (usize, usize) {
        let s = &self.slots[h.task];
        (s.stack_base, s.stack_limit)
    }

    /// Usable bytes above the guard page.
    pub fn stack_usable(&self, h: JoinHandle) -> usize {
        self.slots[h.task].stack_len
    }

    /// Measured high-water after the task completes (`None` before).
    pub fn high_water(&self, h: JoinHandle) -> Option<usize> {
        self.slots[h.task].high_water
    }

    /// Coroutine resumes performed (one per task poll).
    pub fn switches(&self) -> u64 {
        self.switches
    }

    /// TEST-ONLY forced collection hook. Every task is parked at a
    /// safepoint here; the native stacks are never scanned. Not a
    /// stdlib API.
    pub fn gc_collect_for_test(&mut self) -> GcReport {
        collect_roots(&mut self.world.borrow_mut())
    }

    /// Poll a single ready task. Exposed for tests that must force a
    /// GC at an exact point (e.g. while a sender is parked with a
    /// value in flight, or a task is suspended mid-computation).
    pub fn drive_once(&mut self) -> bool {
        match self.ready.pop_front() {
            Some(id) => {
                self.drive(id);
                true
            }
            None => false,
        }
    }

    fn drive(&mut self, id: TaskId) {
        assert_eq!(
            self.slots[id].state,
            TaskState::Ready,
            "scheduler drives ready tasks only",
        );
        self.switches += 1;
        let req = {
            let slot = &mut self.slots[id];
            match slot
                .co
                .as_mut()
                .expect("ready task has a coroutine")
                .resume(())
            {
                CoroutineResult::Yield(req) => Some(req),
                CoroutineResult::Return(()) => None,
            }
        };
        match req {
            None => self.finish(id),
            Some(req) => self.handle(id, req),
        }
    }

    /// The task returned: reclaim its stack after measuring high-water.
    fn finish(&mut self, id: TaskId) {
        self.slots[id].state = TaskState::Done;
        let co = self.slots[id]
            .co
            .take()
            .expect("finishing task has a coroutine");
        let stack = co.into_stack();
        debug_assert_eq!(stack.base().get(), self.slots[id].stack_base);
        let hw = unsafe {
            measure::scan_high_water(self.slots[id].writable_start, self.slots[id].stack_base)
        };
        self.slots[id].high_water = Some(hw);
        drop(stack);
    }

    fn handle(&mut self, id: TaskId, req: ToSched) {
        match req {
            ToSched::Yield => {
                self.slots[id].state = TaskState::Ready;
                self.ready.push_back(id);
            }
            ToSched::Send(ch, value) => {
                // Move-on-send: the sender gives up its root here. The
                // `send` call site already asserted it was published.
                self.world.borrow_mut().shadows[id].remove_one(value);
                let rx = self.world.borrow_mut().channels[ch.0].receiver.take();
                match rx {
                    Some(r) => {
                        let mut w = self.world.borrow_mut();
                        w.inboxes[r] = Some(value);
                        self.slots[r].state = TaskState::Ready;
                        self.ready.push_back(r);
                        self.slots[id].state = TaskState::Ready;
                        self.ready.push_back(id);
                    }
                    None => {
                        let mut w = self.world.borrow_mut();
                        assert!(
                            w.channels[ch.0].sender.is_none(),
                            "spike channel holds one parked sender",
                        );
                        // Parked: the channel slot roots the in-flight value.
                        w.channels[ch.0].sender = Some((id, value));
                        self.slots[id].state = TaskState::Blocked;
                    }
                }
            }
            ToSched::Recv(ch) => {
                let tx = self.world.borrow_mut().channels[ch.0].sender.take();
                match tx {
                    Some((s, value)) => {
                        self.world.borrow_mut().inboxes[id] = Some(value);
                        self.slots[s].state = TaskState::Ready;
                        self.ready.push_back(s);
                        self.slots[id].state = TaskState::Ready;
                        self.ready.push_back(id);
                    }
                    None => {
                        let mut w = self.world.borrow_mut();
                        assert!(
                            w.channels[ch.0].receiver.is_none(),
                            "spike channel holds one parked receiver",
                        );
                        w.channels[ch.0].receiver = Some(id);
                        self.slots[id].state = TaskState::Blocked;
                    }
                }
            }
        }
        let mut w = self.world.borrow_mut();
        if w.heap.bytes_since_gc() >= w.threshold {
            collect_roots(&mut w);
        }
    }

    /// Drive until the named task completes (other ready tasks
    /// progress too). Panics on deadlock instead of hanging.
    pub fn join(&mut self, h: JoinHandle) {
        while self.slots[h.task].state != TaskState::Done {
            assert!(
                self.drive_once(),
                "deadlock: joined task '{}' is blocked with an empty ready queue",
                self.slots[h.task].name,
            );
        }
    }

    /// Drive until every task completes. Panics on deadlock as in `join`.
    pub fn run_all(&mut self) {
        while self.slots.iter().any(|s| s.state != TaskState::Done) {
            assert!(
                self.drive_once(),
                "deadlock: ready queue empty with tasks still blocked",
            );
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}
