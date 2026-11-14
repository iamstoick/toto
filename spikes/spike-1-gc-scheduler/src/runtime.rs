//! Single-threaded cooperative scheduler + unbuffered channel.
//!
//! # Safepoint protocol (as implemented)
//!
//! Collection may run only at a safepoint, and at a safepoint the
//! collector sees exactly: every task's shadow stack, every task's
//! inbox slot, and every channel's in-flight slot. Generated/spike
//! code guarantees:
//!
//! 1. **Allocation safepoint.** `Cx::alloc_*` checks the allocation
//!    threshold first and collects *before* inserting the new object,
//!    so the fresh (still unrooted) object is never at risk and every
//!    older live reference must already be published: a caller must
//!    `root` (or use `alloc_rooted_*`) each live handle before the
//!    next allocation. A handle held only in a Rust local across an
//!    allocation is garbage the collector may reclaim — the
//!    missed-root test proves it.
//! 2. **Yield/step-boundary safepoint.** A task is suspended only when
//!    its `step` returns an `Action`. Before returning, the task must
//!    have published every live handle: rooted on its shadow stack,
//!    sitting in its inbox, or handed to `Action::Send` (which the
//!    scheduler moves into channel/receiver roots). The scheduler also
//!    checks the allocation threshold at every step boundary, so a
//!    threshold collection always sees all tasks parked at safepoints
//!    (true stop-the-world: one OS thread, everything suspended).
//! 3. **No heap reference is held only in a register/local across a
//!    safepoint.** Handles (`GcRef`) are plain integers, so the danger
//!    is not a hidden pointer but an *unpublished* handle; rules 1–2
//!    close that gap by construction.
//! 4. **Context switches preserve root state.** Each task's shadow
//!    stack, inbox, and blocked status live in its own scheduler slot.
//!    A switch only changes which slot the scheduler lends to the
//!    running task; nothing is copied, spilled, or merged.
//!
//! # Channel
//!
//! Exactly one unbuffered channel shape: `send` rendezvous with a
//! parked receiver or parks the sender (value rooted in the channel
//! slot); `recv` takes a parked sender's value or parks the receiver.
//! Values move: the sender's root is removed on send. No buffering,
//! no `select`, no cancellation, no scopes.

use std::collections::VecDeque;

use crate::heap::{GcRef, GcReport, Heap, HeapStats};
use crate::roots::ShadowStack;

pub type TaskId = usize;

/// Unbuffered channel identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChannelId(pub usize);

/// Join token for a spawned task. Joining is idempotent and drives the
/// scheduler (round-robin) until the named task completes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct JoinHandle {
    task: TaskId,
}

/// What a task asks the scheduler to do when its step returns.
/// Returning is itself the yield: every `Action` is a safepoint.
pub enum Action {
    /// Suspend; requeue at the back.
    Yield,
    /// Rendezvous `value` to a receiver (parks if none waits).
    Send(ChannelId, GcRef),
    /// Take a sender's value (parks if none waits).
    Recv(ChannelId),
    /// Task finished; never polled again.
    Done,
}

/// One green-thread task: a state machine the scheduler polls.
/// Steps must be short and always return; a step that loops forever
/// starves the scheduler (cooperative scheduling, by design).
pub trait Step {
    fn step(&mut self, cx: &mut Cx) -> Action;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskState {
    Ready,
    Blocked,
    Done,
}

#[derive(Debug)]
struct Channel {
    sender: Option<(TaskId, GcRef)>,
    receiver: Option<TaskId>,
}

/// Scheduler context handed to the running task. This is the only way
/// task code touches the heap, its own roots, channels, or the log.
pub struct Cx<'a> {
    heap: &'a mut Heap,
    channels: &'a mut Vec<Channel>,
    shadows: &'a mut Vec<ShadowStack>,
    inboxes: &'a mut Vec<Option<GcRef>>,
    log: &'a mut Vec<String>,
    task: TaskId,
    threshold: u64,
}

impl<'a> Cx<'a> {
    fn this_shadow(&mut self) -> &mut ShadowStack {
        &mut self.shadows[self.task]
    }

    /// Run `f` inside a nested root frame on this task's shadow
    /// stack. Every root `f` pushes dies when `f` returns, modeling
    /// what generated code must emit around nested calls.
    ///
    /// A guard object cannot be used here: it would borrow `Cx` for
    /// the frame's lifetime and block all other `Cx` use inside the
    /// frame. The closure form keeps nested-call rooting exact
    /// without borrow gymnastics.
    pub fn with_frame<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let base = self.shadows[self.task].open();
        let frames_len = self.shadows[self.task].frames_len();
        let r = f(self);
        self.shadows[self.task].close(base, frames_len);
        r
    }

    /// Publish a live handle. Must precede the next safepoint.
    pub fn root(&mut self, r: GcRef) {
        self.this_shadow().push(r);
    }

    pub fn shadow_len(&mut self) -> usize {
        self.shadows[self.task].len()
    }

    pub fn shadow_contains(&self, r: GcRef) -> bool {
        self.shadows[self.task].contains(r)
    }

    fn allocation_safepoint(&mut self) {
        if self.heap.bytes_since_gc() >= self.threshold {
            self.collect_all();
        }
    }

    fn collect_all(&mut self) -> GcReport {
        let mut roots = Vec::new();
        for s in self.shadows.iter() {
            roots.extend(s.iter());
        }
        for r in self.inboxes.iter().flatten() {
            roots.push(*r);
        }
        for ch in self.channels.iter() {
            if let Some((_, r)) = ch.sender {
                roots.push(r);
            }
        }
        self.heap.gc_collect_for_test(&roots)
    }

    pub fn alloc_int(&mut self, v: i64) -> GcRef {
        self.allocation_safepoint();
        self.heap.alloc_int(v)
    }

    pub fn alloc_str(&mut self, v: &str) -> GcRef {
        self.allocation_safepoint();
        self.heap.alloc_str(v)
    }

    pub fn alloc_pair(&mut self, head: GcRef, tail: GcRef) -> GcRef {
        self.allocation_safepoint();
        self.heap.alloc_pair(head, tail)
    }

    pub fn alloc_array(&mut self, elems: &[GcRef]) -> GcRef {
        self.allocation_safepoint();
        self.heap.alloc_array(elems)
    }

    /// Allocate and root atomically: no safepoint can intervene between
    /// the insert and the publish, so the result is always safe.
    pub fn alloc_rooted_int(&mut self, v: i64) -> GcRef {
        let r = self.alloc_int(v);
        self.root(r);
        r
    }

    pub fn alloc_rooted_pair(&mut self, head: GcRef, tail: GcRef) -> GcRef {
        let r = self.alloc_pair(head, tail);
        self.root(r);
        r
    }

    /// Take the value delivered to this task by a completed `recv`.
    /// The inbox slot is a GC root while occupied.
    pub fn take_inbox(&mut self) -> Option<GcRef> {
        self.inboxes[self.task].take()
    }

    pub fn heap_get_int(&self, r: GcRef) -> Option<i64> {
        self.heap.get_int(r)
    }

    pub fn heap_get_pair(&self, r: GcRef) -> Option<(GcRef, GcRef)> {
        self.heap.get_pair(r)
    }

    pub fn heap_get_str(&self, r: GcRef) -> Option<&str> {
        self.heap.get_str(r)
    }

    pub fn heap_desc_name(&self, r: GcRef) -> Option<&'static str> {
        self.heap.desc_name(r)
    }

    /// Append to the scheduler's deterministic event log.
    pub fn log(&mut self, msg: impl Into<String>) {
        self.log.push(msg.into());
    }

    /// TEST-ONLY forced collection from inside a task. Same hook as
    /// `Scheduler::gc_collect_for_test`, usable mid-step.
    pub fn gc_collect_for_test(&mut self) -> GcReport {
        self.collect_all()
    }
}

pub struct Scheduler {
    heap: Heap,
    channels: Vec<Channel>,
    shadows: Vec<ShadowStack>,
    inboxes: Vec<Option<GcRef>>,
    states: Vec<TaskState>,
    names: Vec<String>,
    steps: Vec<Option<Box<dyn Step>>>,
    ready: VecDeque<TaskId>,
    log: Vec<String>,
    threshold: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            heap: Heap::new(),
            channels: Vec::new(),
            shadows: Vec::new(),
            inboxes: Vec::new(),
            states: Vec::new(),
            names: Vec::new(),
            steps: Vec::new(),
            ready: VecDeque::new(),
            log: Vec::new(),
            threshold: u64::MAX,
        }
    }

    /// Auto-collect once `bytes_since_gc` reaches this at an allocation
    /// or step-boundary safepoint. Default is "never" (`u64::MAX`) so
    /// tests are deterministic unless they opt in.
    pub fn with_threshold(mut self, bytes: u64) -> Self {
        self.threshold = bytes;
        self
    }

    pub fn spawn(&mut self, name: &str, step: Box<dyn Step>) -> JoinHandle {
        let id = self.steps.len();
        self.shadows.push(ShadowStack::new());
        self.inboxes.push(None);
        self.states.push(TaskState::Ready);
        self.names.push(name.to_owned());
        self.steps.push(Some(step));
        self.ready.push_back(id);
        JoinHandle { task: id }
    }

    pub fn channel(&mut self) -> ChannelId {
        let id = self.channels.len();
        self.channels.push(Channel {
            sender: None,
            receiver: None,
        });
        ChannelId(id)
    }

    pub fn task_count(&self) -> usize {
        self.steps.len()
    }

    pub fn is_done(&self, h: JoinHandle) -> bool {
        self.states[h.task] == TaskState::Done
    }

    pub fn event_log(&self) -> &[String] {
        &self.log
    }

    pub fn heap_stats(&self) -> HeapStats {
        self.heap.stats()
    }

    pub fn max_shadow_depth(&self) -> usize {
        self.shadows.iter().map(ShadowStack::len).max().unwrap_or(0)
    }

    pub fn max_shadow_depth_observed(&self) -> usize {
        self.shadows
            .iter()
            .map(ShadowStack::max_depth_observed)
            .max()
            .unwrap_or(0)
    }

    /// TEST-ONLY forced collection hook. All tasks are parked at step
    /// boundaries here, so every live reference is published and the
    /// collection is exact. Not a stdlib API.
    pub fn gc_collect_for_test(&mut self) -> GcReport {
        let mut roots = Vec::new();
        for s in &self.shadows {
            roots.extend(s.iter());
        }
        for r in self.inboxes.iter().flatten() {
            roots.push(*r);
        }
        for ch in &self.channels {
            if let Some((_, r)) = ch.sender {
                roots.push(r);
            }
        }
        self.heap.gc_collect_for_test(&roots)
    }

    fn poll(&mut self, id: TaskId) -> Action {
        let mut step = self.steps[id].take().expect("polled task has a step");
        let action = {
            let mut cx = Cx {
                heap: &mut self.heap,
                channels: &mut self.channels,
                shadows: &mut self.shadows,
                inboxes: &mut self.inboxes,
                log: &mut self.log,
                task: id,
                threshold: self.threshold,
            };
            step.step(&mut cx)
        };
        self.steps[id] = Some(step);
        action
    }

    fn handle(&mut self, id: TaskId, action: Action) {
        match action {
            Action::Yield => {
                self.states[id] = TaskState::Ready;
                self.ready.push_back(id);
            }
            Action::Send(ch, value) => {
                // Move-on-send: the sender gives up its root here.
                self.shadows[id].remove_one(value);
                if let Some(rx) = self.channels[ch.0].receiver.take() {
                    self.inboxes[rx] = Some(value);
                    self.states[rx] = TaskState::Ready;
                    self.ready.push_back(rx);
                    self.states[id] = TaskState::Ready;
                    self.ready.push_back(id);
                } else {
                    assert!(
                        self.channels[ch.0].sender.is_none(),
                        "spike channel holds one parked sender",
                    );
                    // Parked: the channel slot roots the in-flight value.
                    self.channels[ch.0].sender = Some((id, value));
                    self.states[id] = TaskState::Blocked;
                }
            }
            Action::Recv(ch) => {
                if let Some((tx, value)) = self.channels[ch.0].sender.take() {
                    self.inboxes[id] = Some(value);
                    self.states[tx] = TaskState::Ready;
                    self.ready.push_back(tx);
                    self.states[id] = TaskState::Ready;
                    self.ready.push_back(id);
                } else {
                    assert!(
                        self.channels[ch.0].receiver.is_none(),
                        "spike channel holds one parked receiver",
                    );
                    self.channels[ch.0].receiver = Some(id);
                    self.states[id] = TaskState::Blocked;
                }
            }
            Action::Done => {
                self.states[id] = TaskState::Done;
            }
        }
        // Step-boundary safepoint: everyone is parked, so a threshold
        // collection here sees all roots.
        if self.heap.bytes_since_gc() >= self.threshold {
            self.gc_collect_for_test();
        }
    }

    /// Poll a single ready task. Returns `false` when nothing is
    /// ready. Exposed for tests that must force a GC at an exact
    /// point (e.g. while a sender is parked with a value in flight).
    pub fn drive_once(&mut self) -> bool {
        self.drive()
    }

    fn drive(&mut self) -> bool {
        let Some(id) = self.ready.pop_front() else {
            return false;
        };
        debug_assert_eq!(self.states[id], TaskState::Ready);
        let action = self.poll(id);
        self.handle(id, action);
        true
    }

    fn all_done(&self) -> bool {
        self.states.iter().all(|&s| s == TaskState::Done)
    }

    /// Drive until the named task completes (other ready tasks progress
    /// too — round-robin). Panics on deadlock: with no ready task and
    /// the target still blocked, the spike says so loudly instead of
    /// hanging.
    pub fn join(&mut self, h: JoinHandle) {
        while self.states[h.task] != TaskState::Done {
            assert!(
                self.drive(),
                "deadlock: joined task '{}' is blocked with an empty ready queue",
                self.names[h.task],
            );
        }
    }

    /// Drive until every task completes. Panics on deadlock as in `join`.
    pub fn run_all(&mut self) {
        while !self.all_done() {
            assert!(
                self.drive(),
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
