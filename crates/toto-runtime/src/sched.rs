//! Single-threaded cooperative scheduler (Phase 3, Stage 1): corosensei
//! stackful switching, 64 KiB fixed PROT_NONE-guard stacks, deterministic
//! FIFO scheduling with a seeded RNG for `select` tie-breaks.
//!
//! Ownership (the required M:N redesign): all cross-task state lives in
//! `Shared` behind mutexes, owned via `Arc`. Task bodies own their
//! executor state outright. No `Rc`/`RefCell` anywhere in production.
//! Coroutine bodies are `'static` by construction (only `Arc`s and plain
//! data cross the boundary). `Coroutine` itself is `!Send` (corosensei
//! docs); Stage 2 decides how workers share tasks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use corosensei::stack::{DefaultStack, Stack as _};
use corosensei::{Coroutine, CoroutineResult, Yielder};
use toto_compiler::ast::FnDecl;
use toto_compiler::diag::Diag;
use toto_compiler::hir::Ty;
use toto_compiler::interp::TraceFrame;
use toto_compiler::mir::{MProgram, MirBody};
use toto_compiler::span::Span;

use crate::channel::{ChannelId, ChannelTable, RecvWaiter, SendWaiter, TaskId};
use crate::exec::{Executor, Initial, TaskResult};
use crate::heap::{GcRef, Heap};
use crate::roots::ShadowStack;
use crate::value::{RtVal, heap_refs_of, zero_value};

/// Task → scheduler requests. Handled inline (fast rendezvous) or by
/// parking the task; the response comes back on the next resume.
#[derive(Debug)]
pub enum ToSched {
    Spawn {
        body: Arc<MirBody>,
        blocks: std::sync::Arc<[toto_compiler::mir::MBlock]>,
        captures: Vec<RtVal>,
        span: Span,
        scope: ScopeId,
    },
    ChanNew {
        cap: Option<i64>,
        elem: Ty,
        span: Span,
    },
    Send {
        chan: ChannelId,
        value: RtVal,
        span: Span,
    },
    Recv {
        chan: ChannelId,
        span: Span,
    },
    Select {
        cases: Vec<SelReq>,
        has_default: bool,
        span: Span,
    },
    Close {
        chan: ChannelId,
        span: Span,
    },
    Closed {
        chan: ChannelId,
        span: Span,
    },
}

/// One lowered `select` guard with its runtime operands.
#[derive(Clone, Debug)]
pub enum SelReq {
    Recv {
        chan: ChannelId,
        span: Span,
    },
    Send {
        chan: ChannelId,
        value: RtVal,
        span: Span,
    },
}

/// Scheduler → task responses, delivered on resume.
#[derive(Debug)]
pub enum ToTask {
    Unit,
    Value(RtVal),
    Selected {
        index: usize,
        value: Option<RtVal>,
    },
    /// The scope failed while parked: unwind with E0214 at the blocked span.
    Cancelled {
        op: &'static str,
    },
    /// The operation itself panicked (send-on-closed, double-close).
    OpPanic {
        diag: Diag,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ScopeId(pub usize);

#[derive(Clone, Debug)]
pub struct Failure {
    pub diag: Diag,
    pub trace: Vec<TraceFrame>,
}

struct Scope {
    parent: Option<ScopeId>,
    tasks_live: usize,
    children_live: usize,
    failed: Option<Failure>,
}

/// Scope tree (spec §13.2/§13.4): each `go` runs in a child scope of the
/// spawner's scope. A task panic fails its scope (first-panic-wins);
/// parked tasks in the failed subtree are released with cancellation;
/// running tasks fail fast at their next park op; completion propagates
/// failures upward lazily (settle-then-propagate).
pub struct ScopeTable {
    scopes: Vec<Scope>,
}

impl Default for ScopeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeTable {
    pub fn new() -> Self {
        Self {
            scopes: vec![Scope {
                parent: None,
                tasks_live: 0,
                children_live: 0,
                failed: None,
            }],
        }
    }

    pub fn root() -> ScopeId {
        ScopeId(0)
    }

    pub fn new_scope(&mut self, parent: ScopeId) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(Scope {
            parent: Some(parent),
            tasks_live: 0,
            children_live: 0,
            failed: None,
        });
        self.scopes[parent.0].children_live += 1;
        id
    }

    pub fn add_task(&mut self, scope: ScopeId) {
        self.scopes[scope.0].tasks_live += 1;
    }

    pub fn is_failed(&self, scope: ScopeId) -> bool {
        self.scopes[scope.0].failed.is_some()
    }

    /// True when the scope or any ancestor failed (fail-fast rule covers
    /// the whole failed subtree: parking in it can never succeed).
    pub fn is_failed_subtree(&self, mut scope: ScopeId) -> bool {
        loop {
            let s = &self.scopes[scope.0];
            if s.failed.is_some() {
                return true;
            }
            match s.parent {
                Some(p) => scope = p,
                None => return false,
            }
        }
    }

    /// Mark failed unless already failed (first-panic-wins). Returns
    /// true when newly failed.
    pub fn fail_if_clean(&mut self, scope: ScopeId, failure: Failure) -> bool {
        let s = &mut self.scopes[scope.0];
        if s.failed.is_some() {
            false
        } else {
            s.failed = Some(failure);
            true
        }
    }

    /// Is `scope` a descendant-or-self of `ancestor`?
    pub fn in_subtree(&self, mut scope: ScopeId, ancestor: ScopeId) -> bool {
        loop {
            if scope == ancestor {
                return true;
            }
            match self.scopes[scope.0].parent {
                Some(p) => scope = p,
                None => return false,
            }
        }
    }
}

/// A parked `select` (no default; default never blocks). Re-evaluated
/// after every channel state change.
#[derive(Debug)]
pub struct SelectWaiter {
    pub task: TaskId,
    pub cases: Vec<SelReq>,
    pub span: Span,
    /// FIFO order among parked selects (waiter queue order).
    pub seq: u64,
}

/// Everything the collector needs, under one lock (Stage 1: no lock
/// ordering hazards by construction; Stage 2 may split this with a
/// stop-the-world barrier).
pub struct State {
    pub heap: Heap,
    pub channels: ChannelTable,
    pub shadows: Vec<ShadowStack>,
    pub select_waiters: VecDeque<SelectWaiter>,
}

pub struct Shared {
    pub state: Mutex<State>,
    pub scopes: Mutex<ScopeTable>,
    pub output: Mutex<Vec<u8>>,
    pub threshold: u64,
    pub rng: Mutex<Rng>,
    /// TEST-ONLY: mirror of `RuntimeConfig::collect_at_every_safepoint_for_test`.
    pub force_collect_for_test: bool,
}

/// SplitMix64: tiny deterministic RNG for `select` tie-breaks (spec
/// §13.3: random among ready, seeded in deterministic mode).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }
}

/// Gather every GC root: published task shadows plus channel queues,
/// in-flight sender values, and parked select send values.
pub fn gather_roots(state: &State) -> Vec<GcRef> {
    let mut out = Vec::new();
    for shadow in &state.shadows {
        out.extend(shadow.iter());
    }
    for v in state.channels.roots() {
        heap_refs_of(&v, &mut out);
    }
    for w in &state.select_waiters {
        for case in &w.cases {
            if let SelReq::Send { value, .. } = case {
                heap_refs_of(value, &mut out);
            }
        }
    }
    out
}

/// Code bundle shared by all tasks of one run (all `'static`-safe).
pub struct ExecCode {
    pub prog: MProgram,
    pub structs: std::collections::HashMap<String, toto_compiler::hir::HStructDecl>,
    pub fns: Vec<FnDecl>,
    /// Per-function block graphs, cloned once per run (frames share the
    /// `Arc`; calls never clone block vecs).
    pub fn_blocks: Vec<std::sync::Arc<[toto_compiler::mir::MBlock]>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskState {
    Ready,
    Blocked { span: Span, op: &'static str },
    Done,
}

struct Slot {
    name: String,
    co: Option<Coroutine<ToTask, ToSched, TaskResult, DefaultStack>>,
    state: TaskState,
    scope: ScopeId,
    pending: ToTask,
    stack_base: usize,
    writable_start: usize,
    high_water: Option<usize>,
}

pub struct Scheduler {
    shared: Arc<Shared>,
    code: Arc<ExecCode>,
    slots: Vec<Slot>,
    ready: VecDeque<TaskId>,
    main_task: TaskId,
    main_result: Option<TaskResult>,
    root_failure: Option<Failure>,
    park_seq: u64,
    stack_bytes: usize,
    tasks_spawned: u64,
    switches: u64,
    high_water_max: usize,
}

pub struct RunStats {
    pub tasks_spawned: u64,
    pub switches: u64,
    pub high_water_max: usize,
    pub collections: u64,
    pub allocated: u64,
    pub reclaimed: u64,
}

pub struct SchedOutcome {
    pub main: TaskResult,
    pub root_failure: Option<Failure>,
    pub stats: RunStats,
    pub output: Vec<u8>,
}

impl Scheduler {
    pub fn new(
        code: Arc<ExecCode>,
        threshold: u64,
        seed: u64,
        stack_bytes: usize,
        force_collect_for_test: bool,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                heap: Heap::new(),
                channels: ChannelTable::new(),
                shadows: Vec::new(),
                select_waiters: VecDeque::new(),
            }),
            scopes: Mutex::new(ScopeTable::new()),
            output: Mutex::new(Vec::new()),
            threshold,
            rng: Mutex::new(Rng::new(seed)),
            force_collect_for_test,
        });
        Self {
            shared,
            code,
            slots: Vec::new(),
            ready: VecDeque::new(),
            main_task: TaskId(usize::MAX),
            main_result: None,
            root_failure: None,
            park_seq: 0,
            stack_bytes,
            tasks_spawned: 0,
            switches: 0,
            high_water_max: 0,
        }
    }

    fn spawn_task(&mut self, name: String, scope: ScopeId, initial: Initial, span: Span) -> TaskId {
        // The coroutine body must be `'static`: only `Arc`s and plain
        // data cross. Lifetimes inside borrow from those `Arc`s.
        let id = TaskId(self.slots.len());
        self.shared
            .scopes
            .lock()
            .expect("scopes lock")
            .add_task(scope);
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            st.shadows.push(ShadowStack::new());
        }
        let stack = DefaultStack::new(self.stack_bytes).expect("task stack with guard page");
        let base = stack.base().get();
        let limit = stack.limit().get();
        let writable_start = super::measure::writable_start(limit);
        assert!(
            writable_start < base,
            "stack must have usable space above the guard page"
        );
        unsafe { super::measure::fill_pattern(writable_start, base) };
        let code = Arc::clone(&self.code);
        let shared = Arc::clone(&self.shared);
        // `Initial<'static>` borrows nothing: `Main` is an index and
        // `GoBody` owns its `Arc<MirBody>` plus capture values, so the
        // coroutine body is `'static` as corosensei requires.
        let co =
            Coroutine::with_stack(stack, move |y: &Yielder<ToTask, ToSched>, _init: ToTask| {
                let exec = Executor::new(y, &code, &shared, scope, id);
                exec.run_task(initial, span)
            });
        self.slots.push(Slot {
            name,
            co: Some(co),
            state: TaskState::Ready,
            scope,
            pending: ToTask::Unit,
            stack_base: base,
            writable_start,
            high_water: None,
        });
        self.tasks_spawned += 1;
        self.ready.push_back(id);
        id
    }

    pub fn spawn_main(&mut self, main_idx: usize, span: Span) -> TaskId {
        let root = ScopeTable::root();
        let id = self.spawn_task("main".to_string(), root, Initial::Main(main_idx), span);
        self.main_task = id;
        id
    }

    fn enqueue(&mut self, id: TaskId, resp: ToTask) {
        let slot = &mut self.slots[id.0];
        debug_assert!(slot.state != TaskState::Done, "enqueue a live task");
        slot.pending = resp;
        slot.state = TaskState::Ready;
        self.ready.push_back(id);
    }

    fn park(&mut self, id: TaskId, span: Span, op: &'static str) {
        self.slots[id.0].state = TaskState::Blocked { span, op };
    }

    pub fn run(&mut self) -> SchedOutcome {
        loop {
            if self.ready.is_empty() {
                if self.slots.iter().all(|s| s.state == TaskState::Done) {
                    break;
                }
                // Ready is empty but tasks live: all are blocked (the
                // running task always requeues, parks, or finishes when
                // its yield is handled). No runnable task means no future
                // wakeup (no timers or IO in v1): deadlock (spec §13.2).
                let blocked: Vec<(TaskId, Span, &'static str)> = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| match s.state {
                        TaskState::Blocked { span, op } => Some((TaskId(i), span, op)),
                        _ => None,
                    })
                    .collect();
                debug_assert!(!blocked.is_empty(), "live tasks but none blocked");
                let (_, first_span, _) = blocked[0];
                // Main may have completed while a child stays parked
                // forever: either way no task can run and no wakeup can
                // arrive (no timers or IO in v1).
                let mut msg = format!(
                    "deadlock: no runnable tasks remain ({} blocked with no one left to wake them)",
                    blocked.len()
                );
                for (TaskId(i), span, op) in &blocked {
                    msg.push_str(&format!(
                        "; task{i} `{}` blocked in `{op}` (bytes {}..{})",
                        self.slots[*i].name, span.start, span.end
                    ));
                }
                let diag = Diag::new("E0216", msg, first_span, None);
                let main = self.main_result.take().unwrap_or(TaskResult::Panic {
                    diag: Diag::new(
                        "E0204",
                        "internal error: deadlock before main finished".to_string(),
                        Span::new(0, 0),
                        None,
                    ),
                    trace: Vec::new(),
                });
                let _ = main;
                // Deadlock fails the whole run even if main returned.
                let (collections, allocated, reclaimed) = {
                    let st = self.shared.state.lock().expect("shared state lock");
                    let s = st.heap.stats();
                    (s.collections, s.allocated_total, s.reclaimed_total)
                };
                return SchedOutcome {
                    main: TaskResult::Panic {
                        diag,
                        trace: Vec::new(),
                    },
                    root_failure: self.root_failure.take(),
                    stats: RunStats {
                        tasks_spawned: self.tasks_spawned,
                        switches: self.switches,
                        high_water_max: self.high_water_max,
                        collections,
                        allocated,
                        reclaimed,
                    },
                    output: std::mem::take(&mut *self.shared.output.lock().expect("output lock")),
                };
            }
            let id = self.ready.pop_front().expect("ready non-empty");
            // Skip stale entries (a task requeued twice keeps one slot;
            // both entries are valid — no, each task appears at most once:
            // entries are pushed only on Ready transition... except wakes
            // of already-Ready tasks. Guard: only resume Ready tasks.)
            if self.slots[id.0].state != TaskState::Ready {
                continue;
            }
            self.switches += 1;
            let input = std::mem::replace(&mut self.slots[id.0].pending, ToTask::Unit);
            let co = self.slots[id.0].co.take().expect("task coroutine");
            let mut co = co;
            match co.resume(input) {
                CoroutineResult::Yield(req) => {
                    self.slots[id.0].co = Some(co);
                    self.handle_req(id, req);
                }
                CoroutineResult::Return(res) => {
                    self.slots[id.0].co = Some(co);
                    self.task_done(id, res);
                }
            }
        }
        let main = self.main_result.take().expect("main finished");
        let (collections, allocated, reclaimed) = {
            let st = self.shared.state.lock().expect("shared state lock");
            let s = st.heap.stats();
            (s.collections, s.allocated_total, s.reclaimed_total)
        };
        SchedOutcome {
            main,
            root_failure: self.root_failure.take(),
            stats: RunStats {
                tasks_spawned: self.tasks_spawned,
                switches: self.switches,
                high_water_max: self.high_water_max,
                collections,
                allocated,
                reclaimed,
            },
            output: std::mem::take(&mut *self.shared.output.lock().expect("output lock")),
        }
    }

    fn handle_req(&mut self, id: TaskId, req: ToSched) {
        match req {
            ToSched::Spawn {
                body,
                blocks,
                captures,
                span,
                scope,
            } => {
                let child = self
                    .shared
                    .scopes
                    .lock()
                    .expect("scopes lock")
                    .new_scope(scope);
                let n = self.tasks_spawned;
                self.spawn_task(
                    format!("go-{n}"),
                    child,
                    Initial::GoBody {
                        body,
                        blocks,
                        captures,
                    },
                    span,
                );
                self.enqueue(id, ToTask::Unit);
            }
            ToSched::ChanNew { cap, elem, span } => {
                if cap.is_some_and(|n| n < 0) {
                    self.enqueue(
                        id,
                        ToTask::OpPanic {
                            diag: Diag::new(
                                "E0204",
                                format!("negative channel capacity {}", cap.unwrap_or(0)),
                                span,
                                None,
                            ),
                        },
                    );
                    return;
                }
                let cap_usize = cap.unwrap_or(0) as usize;
                let mut st = self.shared.state.lock().expect("shared state lock");
                let cid = st.channels.create(elem, cap_usize);
                drop(st);
                self.enqueue(id, ToTask::Value(RtVal::Chan(cid)));
            }
            ToSched::Send { chan, value, span } => {
                self.op_send(id, chan, value, span);
                self.pump_selects();
            }
            ToSched::Recv { chan, span } => {
                self.op_recv(id, chan, span);
                self.pump_selects();
            }
            ToSched::Select {
                cases,
                has_default,
                span,
            } => {
                self.op_select(id, cases, has_default, span);
                self.pump_selects();
            }
            ToSched::Close { chan, span } => {
                self.op_close(id, chan, span);
                self.pump_selects();
            }
            ToSched::Closed { chan, span } => {
                let closed = self
                    .shared
                    .state
                    .lock()
                    .expect("shared state lock")
                    .channels
                    .get(chan)
                    .map(|c| c.closed)
                    .unwrap_or(false);
                let _ = span;
                self.enqueue(id, ToTask::Value(RtVal::Bool(closed)));
            }
        }
    }

    fn chan_missing(&mut self, id: TaskId, span: Span) {
        self.enqueue(
            id,
            ToTask::OpPanic {
                diag: Diag::new(
                    "E0204",
                    "internal error: send on a missing channel (unreachable in checked programs)"
                        .to_string(),
                    span,
                    None,
                ),
            },
        );
    }

    /// Send rendezvous (spec §13.1): pair with a parked receiver, buffer
    /// when bounded with space, else park the sender. Send on a closed
    /// channel panics (E0213).
    fn op_send(&mut self, id: TaskId, chan: ChannelId, value: RtVal, span: Span) {
        // Fast path under one lock; waiters wake via the ready queue.
        let mut wake_recv: Option<(TaskId, RtVal)> = None;
        let mut park = false;
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                self.chan_missing(id, span);
                return;
            };
            if ch.closed {
                drop(st);
                self.enqueue(
                    id,
                    ToTask::OpPanic {
                        diag: Diag::new(
                            "E0213",
                            "send on a closed channel".to_string(),
                            span,
                            None,
                        ),
                    },
                );
                return;
            }
            if let Some(w) = ch.recv_waiters.pop_front() {
                wake_recv = Some((w.task, value));
            } else if ch.buf.len() < ch.cap {
                ch.buf.push_back(value);
            } else {
                ch.send_waiters.push_back(SendWaiter {
                    task: id,
                    value,
                    span,
                });
                park = true;
            }
        }
        if let Some((rid, v)) = wake_recv {
            self.enqueue(rid, ToTask::Value(v));
            self.enqueue(id, ToTask::Unit);
        } else if park {
            self.park(id, span, "send");
        } else {
            self.enqueue(id, ToTask::Unit);
        }
    }

    /// Receive (spec §13.1): buffered items first, then a parked sender,
    /// else park — unless closed-and-drained, which yields the zero value.
    fn op_recv(&mut self, id: TaskId, chan: ChannelId, span: Span) {
        enum Out {
            Value(RtVal),
            Park,
        }
        let mut out = Out::Park;
        let mut woken: Option<TaskId> = None;
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                self.chan_missing(id, span);
                return;
            };
            if let Some(v) = ch.buf.pop_front() {
                // Freeing one buffer slot admits the earliest sender.
                if let Some(w) = ch.send_waiters.pop_front() {
                    ch.buf.push_back(w.value);
                    woken = Some(w.task);
                }
                out = Out::Value(v);
            } else if let Some(w) = ch.send_waiters.pop_front() {
                // Unbuffered rendezvous (or bounded-drained): take it.
                woken = Some(w.task);
                out = Out::Value(w.value);
            } else if ch.closed {
                // Closed-and-drained: the zero value of `T`.
                let elem = ch.elem.clone();
                let structs = self.code.structs.clone();
                let z = zero_value(&elem, &mut st.heap, &structs);
                match z {
                    Some(v) => out = Out::Value(v),
                    None => {
                        drop(st);
                        self.enqueue(
                            id,
                            ToTask::OpPanic {
                                diag: Diag::new(
                                    "E0204",
                                    "internal error: zero value of a channel/fn element (unreachable: close is E0315 there)".to_string(),
                                    span,
                                    None,
                                ),
                            },
                        );
                        return;
                    }
                }
            } else {
                ch.recv_waiters.push_back(RecvWaiter { task: id, span });
            }
        }
        if let Some(wid) = woken {
            self.enqueue(wid, ToTask::Unit);
        }
        match out {
            Out::Value(v) => self.enqueue(id, ToTask::Value(v)),
            Out::Park => self.park(id, span, "recv"),
        }
    }

    /// `select` (spec §13.3): the first ready case runs; random among
    /// simultaneously ready (seeded RNG); `default` never blocks. A send
    /// guard on a closed channel counts as ready and panics when chosen
    /// (matching Go: the send can "proceed" only by panicking).
    fn op_select(&mut self, id: TaskId, cases: Vec<SelReq>, has_default: bool, span: Span) {
        let pick = {
            let st = self.shared.state.lock().expect("shared state lock");
            let mut rng = self.shared.rng.lock().expect("rng lock");
            Self::choose_case(&st, &cases, has_default, &mut rng)
        };
        match pick {
            SelectPick::Case(i) => self.perform_case(id, cases, i),
            SelectPick::Default => {
                self.enqueue(
                    id,
                    ToTask::Selected {
                        index: cases.len(),
                        value: None,
                    },
                );
            }
            SelectPick::Block => {
                let seq = self.park_seq;
                self.park_seq += 1;
                {
                    let mut st = self.shared.state.lock().expect("shared state lock");
                    st.select_waiters.push_back(SelectWaiter {
                        task: id,
                        cases,
                        span,
                        seq,
                    });
                }
                self.park(id, span, "select");
            }
        }
    }

    /// Readiness scan: indices of cases that can proceed now.
    fn ready_cases(state: &crate::sched::State, cases: &[SelReq]) -> Vec<usize> {
        let mut out = Vec::new();
        for (i, case) in cases.iter().enumerate() {
            match case {
                SelReq::Recv { chan, .. } => {
                    let ready = match state.channels.get(*chan) {
                        Some(ch) => !ch.buf.is_empty() || !ch.send_waiters.is_empty() || ch.closed,
                        None => true,
                    };
                    if ready {
                        out.push(i);
                    }
                }
                SelReq::Send { chan, .. } => {
                    let ready = match state.channels.get(*chan) {
                        // Closed sends proceed by panicking.
                        Some(ch) if ch.closed => true,
                        Some(ch) => !ch.recv_waiters.is_empty() || ch.buf.len() < ch.cap,
                        None => true,
                    };
                    if ready {
                        out.push(i);
                    }
                }
            }
        }
        out
    }

    fn choose_case(
        state: &crate::sched::State,
        cases: &[SelReq],
        has_default: bool,
        rng: &mut Rng,
    ) -> SelectPick {
        let ready = Self::ready_cases(state, cases);
        if ready.is_empty() {
            if has_default {
                SelectPick::Default
            } else {
                SelectPick::Block
            }
        } else {
            SelectPick::Case(ready[rng.below(ready.len())])
        }
    }

    /// Perform the chosen case (readiness was established under the same
    /// lock discipline — re-checked here since state may have changed).
    fn perform_case(&mut self, id: TaskId, mut cases: Vec<SelReq>, i: usize) {
        match cases.remove(i) {
            SelReq::Recv { chan, span } => {
                // Reuse the receive path, then wrap as a selection.
                // Temporarily enqueue a marker: perform the receive inline
                // and convert the plain value response.
                self.op_recv_inner(id, chan, span, i);
            }
            SelReq::Send { chan, value, span } => {
                self.op_send_inner(id, chan, value, span, i);
            }
        }
    }

    /// Receive for a selected case: like `op_recv` but responds
    /// `Selected` instead of `Value`. Readiness held, so this never
    /// parks (a lost race re-blocks instead of panicking — the task
    /// simply waits for the next state change).
    fn op_recv_inner(&mut self, id: TaskId, chan: ChannelId, span: Span, index: usize) {
        enum Out {
            Selected(RtVal),
            Repark,
        }
        let mut out = Out::Repark;
        let mut woken: Option<TaskId> = None;
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                self.chan_missing(id, span);
                return;
            };
            if let Some(v) = ch.buf.pop_front() {
                if let Some(w) = ch.send_waiters.pop_front() {
                    ch.buf.push_back(w.value);
                    woken = Some(w.task);
                }
                out = Out::Selected(v);
            } else if let Some(w) = ch.send_waiters.pop_front() {
                woken = Some(w.task);
                out = Out::Selected(w.value);
            } else if ch.closed {
                let elem = ch.elem.clone();
                let structs = self.code.structs.clone();
                match zero_value(&elem, &mut st.heap, &structs) {
                    Some(v) => out = Out::Selected(v),
                    None => {
                        drop(st);
                        self.enqueue(
                            id,
                            ToTask::OpPanic {
                                diag: Diag::new(
                                    "E0204",
                                    "internal error: zero value of a channel/fn element (unreachable: close is E0315 there)".to_string(),
                                    span,
                                    None,
                                ),
                            },
                        );
                        return;
                    }
                }
            }
        }
        if let Some(wid) = woken {
            self.enqueue(wid, ToTask::Unit);
        }
        match out {
            Out::Selected(v) => self.enqueue(
                id,
                ToTask::Selected {
                    index,
                    value: Some(v),
                },
            ),
            Out::Repark => {
                // Lost a race between readiness and performance (only
                // under M:N; single-threaded this is unreachable). Park
                // on this channel and wait for the next change.
                let mut st = self.shared.state.lock().expect("shared state lock");
                if let Some(ch) = st.channels.get_mut(chan) {
                    ch.recv_waiters.push_back(RecvWaiter { task: id, span });
                }
                drop(st);
                self.park(id, span, "recv");
            }
        }
    }

    /// Send for a selected case: like `op_send` but responds `Selected`.
    /// A closed channel panics (E0213) when chosen.
    fn op_send_inner(
        &mut self,
        id: TaskId,
        chan: ChannelId,
        value: RtVal,
        span: Span,
        index: usize,
    ) {
        let mut wake_recv: Option<(TaskId, RtVal)> = None;
        let mut parked = false;
        let mut panicked = false;
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                self.chan_missing(id, span);
                return;
            };
            if ch.closed {
                panicked = true;
            } else if let Some(w) = ch.recv_waiters.pop_front() {
                wake_recv = Some((w.task, value));
            } else if ch.buf.len() < ch.cap {
                ch.buf.push_back(value);
            } else {
                ch.send_waiters.push_back(SendWaiter {
                    task: id,
                    value,
                    span,
                });
                parked = true;
            }
        }
        if panicked {
            self.enqueue(
                id,
                ToTask::OpPanic {
                    diag: Diag::new("E0213", "send on a closed channel".to_string(), span, None),
                },
            );
        } else if let Some((rid, v)) = wake_recv {
            self.enqueue(rid, ToTask::Value(v));
            self.enqueue(id, ToTask::Selected { index, value: None });
        } else if parked {
            // Lost a race (M:N only); wait on this channel.
            self.park(id, span, "send");
        } else {
            self.enqueue(id, ToTask::Selected { index, value: None });
        }
    }

    /// Re-evaluate every parked select after a channel state change.
    /// Ready selects perform their choice now (deterministic order:
    /// waiter FIFO, RNG for case ties).
    fn pump_selects(&mut self) {
        loop {
            // Find the first ready waiter, then remove and perform it.
            // Each pump pass re-scans: performing one select can change
            // readiness for the rest.
            let waiter = {
                let mut st = self.shared.state.lock().expect("shared state lock");
                let pos = st
                    .select_waiters
                    .iter()
                    .position(|w| !Self::ready_cases(&st, &w.cases).is_empty());
                match pos {
                    Some(p) => st.select_waiters.remove(p),
                    None => return,
                }
            };
            let Some(waiter) = waiter else { continue };
            let pick = {
                let st = self.shared.state.lock().expect("shared state lock");
                let mut rng = self.shared.rng.lock().expect("rng lock");
                Self::choose_case(&st, &waiter.cases, false, &mut rng)
            };
            match pick {
                SelectPick::Case(i) => self.perform_case(waiter.task, waiter.cases, i),
                // Parked selects never carry a default; a lost race
                // (M:N only) re-parks at the back.
                SelectPick::Default | SelectPick::Block => {
                    let mut st = self.shared.state.lock().expect("shared state lock");
                    st.select_waiters.push_back(waiter);
                }
            }
        }
    }

    /// `close(ch)` (spec §13.1): mark closed, fail parked senders
    /// (E0213 at their spans), drain buffered items to parked receivers,
    /// then zero values for the rest. Double-close panics (E0213).
    fn op_close(&mut self, id: TaskId, chan: ChannelId, span: Span) {
        struct Release {
            task: TaskId,
            resp: ToTask,
        }
        let mut releases: Vec<Release> = Vec::new();
        let mut result: ToTask = ToTask::Unit;
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                self.chan_missing(id, span);
                return;
            };
            if ch.closed {
                result = ToTask::OpPanic {
                    diag: Diag::new("E0213", "close of a closed channel".to_string(), span, None),
                };
            } else {
                ch.closed = true;
                for w in ch.send_waiters.drain(..) {
                    releases.push(Release {
                        task: w.task,
                        resp: ToTask::OpPanic {
                            diag: Diag::new(
                                "E0213",
                                "send on a closed channel".to_string(),
                                w.span,
                                None,
                            ),
                        },
                    });
                }
                // Drained-buffer zero value, computed once (the element
                // type is fixed per channel).
                let elem = ch.elem.clone();
                let structs = self.code.structs.clone();
                let zero = {
                    let heap = &mut st.heap;
                    zero_value(&elem, heap, &structs)
                };
                let ch = st.channels.get_mut(chan).expect("channel live");
                while let Some(w) = ch.recv_waiters.pop_front() {
                    if let Some(v) = ch.buf.pop_front() {
                        if let Some(sw) = ch.send_waiters.pop_front() {
                            ch.buf.push_back(sw.value);
                            releases.push(Release {
                                task: sw.task,
                                resp: ToTask::Unit,
                            });
                        }
                        releases.push(Release {
                            task: w.task,
                            resp: ToTask::Value(v),
                        });
                    } else {
                        match &zero {
                            Some(v) => releases.push(Release {
                                task: w.task,
                                resp: ToTask::Value(v.clone()),
                            }),
                            None => releases.push(Release {
                                task: w.task,
                                resp: ToTask::OpPanic {
                                    diag: Diag::new(
                                        "E0204",
                                        "internal error: zero value of a channel/fn element (unreachable: close is E0315)".to_string(),
                                        w.span,
                                        None,
                                    ),
                                },
                            }),
                        }
                    }
                }
            }
        }
        for r in releases {
            self.enqueue(r.task, r.resp);
        }
        self.enqueue(id, result);
    }

    fn task_done(&mut self, id: TaskId, res: TaskResult) {
        // High-water: the stack is dead; scan for the pattern reversal.
        let hw = {
            let slot = &self.slots[id.0];
            unsafe { super::measure::scan_high_water(slot.writable_start, slot.stack_base) }
        };
        self.slots[id.0].high_water = Some(hw);
        self.high_water_max = self.high_water_max.max(hw);
        // Reap the coroutine stack now (frees the mapping promptly).
        self.slots[id.0].co = None;
        self.slots[id.0].state = TaskState::Done;
        let scope = self.slots[id.0].scope;
        match res {
            TaskResult::Panic { diag, trace } => {
                self.fail_scope(
                    scope,
                    Failure {
                        diag: diag.clone(),
                        trace: trace.clone(),
                    },
                );
                if id == self.main_task {
                    self.main_result = Some(TaskResult::Panic { diag, trace });
                }
            }
            TaskResult::Return(v) => {
                if id == self.main_task {
                    self.main_result = Some(TaskResult::Return(v));
                }
            }
        }
        self.on_task_done(scope);
    }

    fn on_task_done(&mut self, scope: ScopeId) {
        {
            let mut t = self.shared.scopes.lock().expect("scopes lock");
            t.scopes[scope.0].tasks_live -= 1;
        }
        let mut cur = scope;
        loop {
            let next = {
                let mut t = self.shared.scopes.lock().expect("scopes lock");
                let s = &mut t.scopes[cur.0];
                if s.tasks_live == 0 && s.children_live == 0 {
                    let failure = s.failed.take();
                    let parent = s.parent;
                    if let Some(p) = parent {
                        t.scopes[p.0].children_live -= 1;
                    }
                    Some((parent, failure))
                } else {
                    None
                }
            };
            match next {
                Some((parent, failure)) => {
                    if let Some(f) = failure {
                        match parent {
                            Some(p) => self.fail_scope(p, f),
                            None => {
                                if self.root_failure.is_none() {
                                    self.root_failure = Some(f);
                                }
                            }
                        }
                    }
                    match parent {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                None => break,
            }
        }
    }

    /// Fail a scope (first-panic-wins) and release every task parked in
    /// its subtree with cancellation. The release order is task-id order
    /// (deterministic).
    fn fail_scope(&mut self, scope: ScopeId, failure: Failure) {
        let newly = self
            .shared
            .scopes
            .lock()
            .expect("scopes lock")
            .fail_if_clean(scope, failure);
        if !newly {
            return;
        }
        let in_sub: Vec<TaskId> = {
            let t = self.shared.scopes.lock().expect("scopes lock");
            self.slots
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    matches!(s.state, TaskState::Blocked { .. }) && t.in_subtree(s.scope, scope)
                })
                .map(|(i, _)| TaskId(i))
                .collect()
        };
        for tid in in_sub {
            let op = match self.slots[tid.0].state {
                TaskState::Blocked { op, .. } => op,
                _ => continue,
            };
            self.enqueue(tid, ToTask::Cancelled { op });
        }
    }
}

enum SelectPick {
    Case(usize),
    Default,
    Block,
}
