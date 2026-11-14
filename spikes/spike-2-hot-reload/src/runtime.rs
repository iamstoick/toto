//! Drain-and-restart reload protocol on the Spike 1/1b runtime.
//!
//! A reload installs a new version and retires the old one exactly
//! when the v2.2 lifetime rule holds (no frames + no callables).
//! Drain waits for quiescence; the timeout cancels precisely the
//! tasks still holding old frames (tracked per restartable entry)
//! and respawns them on the new version; layout-typed changes fall
//! back to full restart. Reloads are test-only (`reload_for_test`,
//! `begin_reload`, `restart_for_test`); no watcher, no compiler.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use spike_1_gc_scheduler::heap::{GcRef, GcReport, HeapStats};
use spike_1b_stack_switching::{
    ChannelId as RtChannelId, DEFAULT_STACK_BYTES, JoinHandle, Scheduler, TaskRt,
};

use crate::hctx::Hctx;
use crate::registry::{ChangeKind, Disposition, Func, Registry, SpecId, VersionId, classify};
use std::collections::HashMap;

/// Restartable task entry: a named scope body re-runnable on the
/// current version after cancellation.
pub type EntryFn = Rc<dyn for<'a> Fn(Hctx<'a>)>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpecState {
    Active,
    Done,
    Cancelled,
}

struct ScopeSpec {
    name: String,
    stack_bytes: usize,
    entry: EntryFn,
    handle: Option<JoinHandle>,
}

/// Outcome of pumping a drain to its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PumpResult {
    Drained,
    TimedOut,
}

/// Outcome recorded in a reload report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Old version drained and retired, no cancellation needed.
    Drained,
    /// Timeout fired; stuck scopes cancelled at boundaries and
    /// respawned on the new version.
    CancelledAndRespawned(usize),
    /// Matrix routed to scope restart; scopes respawned.
    ScopeRestarted(usize),
    /// Full restart executed (matrix or fallback).
    Restarted,
    /// Cancel grace expired with frames still stuck: fell back to
    /// full restart. (Unreachable with cooperative test tasks;
    /// the path exists so a stuck drain can never hang the loop.)
    FellBack,
}

#[derive(Debug)]
pub struct ReloadReport {
    pub change: ChangeKind,
    pub disposition: Disposition,
    pub outcome: Outcome,
    pub old: VersionId,
    pub new: VersionId,
}

/// State between `begin_reload` and drain completion, for tests that
/// interleave scheduler steps, forced GCs, and assertions.
#[derive(Debug)]
pub struct PendingReload {
    pub old: VersionId,
    pub new: VersionId,
    pub disposition: Disposition,
}

pub struct HotRuntime {
    sched: Scheduler,
    reg: Rc<RefCell<Registry>>,
    specs: Vec<ScopeSpec>,
    states: Rc<RefCell<Vec<SpecState>>>,
    draining: Vec<VersionId>,
}

impl HotRuntime {
    pub fn new() -> Self {
        let mut rt = Self {
            sched: Scheduler::new(),
            reg: Rc::new(RefCell::new(Registry::new())),
            specs: Vec::new(),
            states: Rc::new(std::cell::RefCell::new(Vec::new())),
            draining: Vec::new(),
        };
        rt.spawn_holder();
        rt
    }

    /// Version-root holder: keeps every live version's heap token
    /// rooted in its own shadow stack, so ALL forced collections
    /// (whoever triggers them) see registry roots. Syncs every poll.
    fn spawn_holder(&mut self) {
        let reg = Rc::clone(&self.reg);
        self.sched
            .spawn("version-holder", DEFAULT_STACK_BYTES, move |rt| {
                let h = Hctx::new(rt, reg, SpecId::MAX);
                let mut kept: Vec<(VersionId, GcRef)> = Vec::new();
                loop {
                    h.sync_version_roots(&mut kept);
                    // The holder serves all versions and never enters a
                    // scope, so it is never cancelled; the check documents
                    // the invariant (unreachable in practice).
                    if h.yield_point().is_err() {
                        return;
                    }
                }
            });
    }

    /// Install a version directly (test setup / post-restart).
    pub fn install_version(
        &mut self,
        funcs: HashMap<u32, Func>,
        initializers: HashMap<String, i64>,
    ) -> VersionId {
        self.reg.borrow_mut().install(funcs, initializers)
    }

    /// Spawn a restartable scope entry on the current version.
    pub fn spawn_scope(&mut self, name: &str, stack_bytes: usize, entry: EntryFn) -> SpecId {
        let spec = self.specs.len();
        self.states.borrow_mut().push(SpecState::Active);
        let handle = self.spawn_adapter(spec, name, stack_bytes, &entry);
        self.specs.push(ScopeSpec {
            name: name.to_owned(),
            stack_bytes,
            entry,
            handle: Some(handle),
        });
        spec
    }

    fn spawn_adapter(
        &mut self,
        spec: SpecId,
        name: &str,
        stack_bytes: usize,
        entry: &EntryFn,
    ) -> JoinHandle {
        let reg = Rc::clone(&self.reg);
        let states = Rc::clone(&self.states);
        let entry = Rc::clone(entry);
        self.sched.spawn(name, stack_bytes, move |rt: TaskRt<'_>| {
            (entry)(Hctx::new(rt, reg, spec));
            let mut states = states.borrow_mut();
            if states[spec] == SpecState::Active {
                states[spec] = SpecState::Done;
            }
        })
    }

    // -- reload protocol -------------------------------------------------

    /// TEST-ONLY reload trigger. Installs the new version, then runs
    /// the full protocol synchronously: drain to quiescence, or
    /// timeout -> cancel at boundaries -> respawn, or restart.
    /// Tests pass SHORT timeouts (e.g. 100 ms); the provisional 5 s
    /// dev-loop default is a Phase 4 UX value and is never waited
    /// out here.
    pub fn reload_for_test(
        &mut self,
        change: ChangeKind,
        funcs: HashMap<u32, Func>,
        initializers: HashMap<String, i64>,
        timeout: Duration,
    ) -> ReloadReport {
        let pending = self.begin_reload(change, funcs, initializers);
        let outcome = match pending.disposition {
            Disposition::DrainReload => match self.pump_until_quiescent(pending.old, timeout) {
                PumpResult::Drained => {
                    self.retire_if_quiescent(pending.old);
                    Outcome::Drained
                }
                PumpResult::TimedOut => {
                    self.cancel_version(pending.old);
                    // Grace: cancelled tasks exit at their next
                    // boundary; then the version must be quiescent.
                    match self.pump_until_quiescent(pending.old, timeout) {
                        PumpResult::Drained => {
                            self.retire_if_quiescent(pending.old);
                            Outcome::CancelledAndRespawned(self.respawn_cancelled())
                        }
                        PumpResult::TimedOut => {
                            self.restart_for_test(
                                HashMap::new(),
                                HashMap::new(),
                                "cancel grace expired",
                            );
                            Outcome::FellBack
                        }
                    }
                }
            },
            Disposition::ScopeRestart => {
                self.cancel_version(pending.old);
                match self.pump_until_quiescent(pending.old, timeout) {
                    PumpResult::Drained => {
                        self.retire_if_quiescent(pending.old);
                        Outcome::ScopeRestarted(self.respawn_cancelled())
                    }
                    PumpResult::TimedOut => {
                        self.restart_for_test(
                            HashMap::new(),
                            HashMap::new(),
                            "scope-restart grace expired",
                        );
                        Outcome::FellBack
                    }
                }
            }
            Disposition::FullRestart => Outcome::Restarted,
        };
        ReloadReport {
            change,
            disposition: pending.disposition,
            outcome,
            old: pending.old,
            new: pending.new,
        }
    }

    /// Begin a reload without draining: installs the new version and
    /// arms the disposition. FullRestart executes immediately (fresh
    /// runtime on the new version); the other dispositions need
    /// pumping (`pump_once` / `pump_until_quiescent`).
    pub fn begin_reload(
        &mut self,
        change: ChangeKind,
        funcs: HashMap<u32, Func>,
        initializers: HashMap<String, i64>,
    ) -> PendingReload {
        let disposition = classify(change);
        let old = self.reg.borrow().current();
        match disposition {
            Disposition::DrainReload => {
                let new = self.reg.borrow_mut().install(funcs, initializers);
                self.reg
                    .borrow_mut()
                    .log
                    .push(format!("draining v{old} -> v{new}"));
                self.draining.push(old);
                PendingReload {
                    old,
                    new,
                    disposition,
                }
            }
            Disposition::ScopeRestart => {
                let new = self.reg.borrow_mut().install(funcs, initializers);
                self.reg
                    .borrow_mut()
                    .log
                    .push(format!("scope restart v{old} -> v{new}"));
                self.cancel_version(old);
                PendingReload {
                    old,
                    new,
                    disposition,
                }
            }
            Disposition::FullRestart => {
                self.reg
                    .borrow_mut()
                    .log
                    .push(format!("matrix routes {change:?} to full restart"));
                self.restart_for_test(funcs, initializers, "matrix disposition");
                let new = self.reg.borrow().current();
                PendingReload {
                    old,
                    new,
                    disposition,
                }
            }
        }
    }

    /// Simulated process restart: drop every task, reset globals,
    /// rebuild on the new version's initializers. Documented as the
    /// fallback, not the normal path.
    pub fn restart_for_test(
        &mut self,
        funcs: HashMap<u32, Func>,
        initializers: HashMap<String, i64>,
        reason: &str,
    ) {
        self.reg
            .borrow_mut()
            .log
            .push(format!("full restart: {reason}"));
        self.sched = Scheduler::new();
        *self.reg.borrow_mut() = Registry::new();
        self.specs.clear();
        self.states.borrow_mut().clear();
        self.draining.clear();
        self.spawn_holder();
        let v = self.reg.borrow_mut().install(funcs, initializers);
        let inits = self
            .reg
            .borrow()
            .versions
            .get(&v)
            .expect("installed")
            .initializers
            .clone();
        self.reg.borrow_mut().globals = inits;
    }

    /// Pump the drain: drive tasks until `old` is quiescent or the
    /// deadline passes. Cooperative tasks only: a task that never
    /// suspends cannot be drained on one thread (documented).
    pub fn pump_until_quiescent(&mut self, old: VersionId, timeout: Duration) -> PumpResult {
        let t0 = Instant::now();
        loop {
            if self.retire_if_quiescent(old) {
                return PumpResult::Drained;
            }
            if t0.elapsed() >= timeout {
                return PumpResult::TimedOut;
            }
            if !self.sched.drive_once() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// One scheduler step plus retire maintenance. Returns whether a
    /// task ran (false: ready queue empty).
    pub fn pump_once(&mut self) -> bool {
        let progressed = self.sched.drive_once();
        let draining = self.draining.clone();
        for v in draining {
            self.retire_if_quiescent(v);
        }
        progressed
    }

    /// Mark every task still holding an old-version frame for
    /// cancellation at its next scope boundary. Precise: only specs
    /// whose pinned set contains the version.
    pub fn cancel_version(&mut self, v: VersionId) {
        let affected: Vec<SpecId> = {
            let reg = self.reg.borrow();
            reg.spec_versions
                .iter()
                .filter_map(|(&s, vs)| vs.contains(&v).then_some(s))
                .collect()
        };
        {
            let mut reg = self.reg.borrow_mut();
            if let Some(e) = reg.versions.get_mut(&v) {
                e.cancelled = true;
            }
            reg.log.push(format!("cancelled v{v}"));
        }
        let mut states = self.states.borrow_mut();
        for s in affected {
            if states.get(s).copied() == Some(SpecState::Active) {
                states[s] = SpecState::Cancelled;
            }
        }
    }

    /// Respawn cancelled entries on the current (new) version with
    /// clean state. Returns the respawn count.
    pub fn respawn_cancelled(&mut self) -> usize {
        let mut n = 0;
        for spec in 0..self.specs.len() {
            if self.states.borrow().get(spec).copied() != Some(SpecState::Cancelled) {
                continue;
            }
            // Clean slate: no stale version pins for the new task.
            self.reg.borrow_mut().spec_versions.remove(&spec);
            let (name, stack_bytes, entry) = {
                let s = &self.specs[spec];
                (s.name.clone(), s.stack_bytes, Rc::clone(&s.entry))
            };
            let handle = self.spawn_adapter(spec, &name, stack_bytes, &entry);
            self.specs[spec].handle = Some(handle);
            self.states.borrow_mut()[spec] = SpecState::Active;
            let current = self.reg.borrow().current();
            self.reg
                .borrow_mut()
                .log
                .push(format!("respawned {name} on v{current}"));
            n += 1;
        }
        n
    }

    /// Retire `v` if the lifetime rule holds. Returns the
    /// postcondition (true when retired, including already-retired),
    /// so drain loops can treat "retired at any point" as done.
    pub fn retire_if_quiescent(&mut self, v: VersionId) -> bool {
        self.reg.borrow_mut().retire_if_quiescent(v);
        if self.is_retired(v) {
            self.draining.retain(|&d| d != v);
            return true;
        }
        false
    }

    // -- queries for tests ------------------------------------------------

    pub fn quiescent(&self, v: VersionId) -> bool {
        self.reg.borrow().quiescent(v)
    }

    pub fn is_retired(&self, v: VersionId) -> bool {
        self.reg.borrow().is_retired(v)
    }

    pub fn version_frames(&self, v: VersionId) -> usize {
        self.reg
            .borrow()
            .versions
            .get(&v)
            .map(|e| e.frames)
            .unwrap_or(0)
    }

    pub fn callables_of(&self, v: VersionId) -> usize {
        self.reg.borrow().callables_of(v)
    }

    pub fn token_of(&self, v: VersionId) -> Option<GcRef> {
        self.reg.borrow().versions.get(&v).and_then(|e| e.token)
    }

    pub fn current_version(&self) -> VersionId {
        self.reg.borrow().current()
    }

    pub fn task_count(&self) -> usize {
        self.sched.task_count()
    }

    pub fn heap_stats(&self) -> HeapStats {
        self.sched.heap_stats()
    }

    pub fn event_log(&self) -> Vec<String> {
        self.sched.event_log()
    }

    pub fn reload_log(&self) -> Vec<String> {
        self.reg.borrow().log.clone()
    }

    pub fn gc_for_test(&mut self) -> GcReport {
        // Every root (frames, inboxes, channel slots, holder-kept
        // version tokens) lives in the 1b world, so one collection is
        // exact. Spike-2 task code never triggers GC directly; all
        // forced collections go through here.
        self.sched.gc_collect_for_test()
    }

    pub fn join_scope(&mut self, spec: SpecId) {
        let handle = self.specs[spec].handle.expect("scope spawned");
        self.sched.join(handle);
    }

    pub fn run_all(&mut self) {
        self.sched.run_all();
    }

    pub fn drive_once(&mut self) -> bool {
        self.sched.drive_once()
    }

    /// Test-only global access (simulated process globals).
    pub fn set_global(&mut self, name: &str, val: i64) {
        self.reg.borrow_mut().globals.insert(name.to_owned(), val);
    }

    pub fn get_global(&self, name: &str) -> i64 {
        self.reg.borrow().globals.get(name).copied().unwrap_or(0)
    }

    pub fn channel(&mut self, type_tag: &'static str) -> (u32, RtChannelId) {
        let id = self.reg.borrow().next_channel;
        self.reg.borrow_mut().next_channel += 1;
        self.reg.borrow_mut().channel_tags.insert(id, type_tag);
        (id, self.sched.channel())
    }

    pub fn is_done_scope(&self, spec: SpecId) -> bool {
        let h = self.specs[spec].handle.expect("scope spawned");
        self.sched.is_done(h)
    }
}

impl Default for HotRuntime {
    fn default() -> Self {
        Self::new()
    }
}
