//! Version registry: explicit version identity, pinned scopes,
//! reloadability classification, and the retention data for the
//! code-version lifetime rule.
//!
//! "Code" here is simulated: each version maps function IDs to Rust
//! closures. The mechanism under test is identity, dispatch, drain,
//! classification, and lifetime — not compilation.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use spike_1_gc_scheduler::heap::GcRef;

use crate::hctx::Hctx;

pub type VersionId = u64;
pub type FuncId = u32;
pub type SpecId = usize;

/// A simulated function body. Receives task context (for nested
/// calls, allocation, logging) and plain int args.
pub type Func = Rc<dyn Fn(&Hctx, &[i64]) -> i64>;

/// Test-only change descriptor. Classification input is explicit —
/// never inferred from source diffs in this spike.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    /// Function bodies changed only: drain + new version.
    Body,
    /// A function signature changed: restart the affected scope.
    Signature,
    /// A struct gained a field: migrate or restart (v1: restart).
    StructAdd,
    /// A struct lost a field: migrate or restart (v1: restart).
    StructRemove,
    /// A channel's element type changed: restart.
    ChannelType,
    /// A global initializer changed: restart.
    GlobalInit,
}

/// D7 reloadability matrix, enforced as code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disposition {
    /// Old frames drain, new scopes use the new version.
    DrainReload,
    /// Cancel affected scopes, respawn them on the new version.
    ScopeRestart,
    /// Tear everything down, restart fresh on the new version.
    FullRestart,
}

pub fn classify(change: ChangeKind) -> Disposition {
    match change {
        ChangeKind::Body => Disposition::DrainReload,
        ChangeKind::Signature => Disposition::ScopeRestart,
        ChangeKind::StructAdd
        | ChangeKind::StructRemove
        | ChangeKind::ChannelType
        | ChangeKind::GlobalInit => Disposition::FullRestart,
    }
}

pub struct VersionEntry {
    pub id: VersionId,
    pub funcs: HashMap<FuncId, Func>,
    /// Heap version object, allocated and rooted by the holder task.
    /// `None` until the holder first syncs after install.
    pub token: Option<GcRef>,
    /// Active scope count (frames executing this version, including
    /// suspended tasks — their guards live on their native stacks).
    pub frames: usize,
    /// Drain timeout fired: tasks must leave at the next boundary.
    pub cancelled: bool,
    pub retired: bool,
    /// Global initializers carried by this version.
    pub initializers: HashMap<String, i64>,
}

pub struct Registry {
    pub versions: HashMap<VersionId, VersionEntry>,
    pub current: VersionId,
    pub next_id: VersionId,
    /// Outstanding callable references (simulated reachability
    /// accounting: the real system would read GC reachability; the
    /// spike tracks acquire/release explicitly).
    pub callables: HashMap<GcRef, VersionId>,
    /// Versions a scope set belongs to, per restartable task entry.
    /// A task holding an old frame is cancelled precisely.
    pub spec_versions: HashMap<SpecId, HashSet<VersionId>>,
    pub globals: HashMap<String, i64>,
    /// Spike-2 channel id -> declared element-type tag.
    pub channel_tags: HashMap<u32, &'static str>,
    pub next_channel: u32,
    /// Reload-protocol events (install/retire/cancel/restart).
    pub log: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
            current: 0,
            next_id: 1,
            callables: HashMap::new(),
            spec_versions: HashMap::new(),
            globals: HashMap::new(),
            channel_tags: HashMap::new(),
            next_channel: 0,
            log: Vec::new(),
        }
    }

    pub fn install(
        &mut self,
        funcs: HashMap<FuncId, Func>,
        initializers: HashMap<String, i64>,
    ) -> VersionId {
        let id = self.next_id;
        self.next_id += 1;
        self.versions.insert(
            id,
            VersionEntry {
                id,
                funcs,
                token: None,
                frames: 0,
                cancelled: false,
                retired: false,
                initializers,
            },
        );
        self.current = id;
        self.log.push(format!("installed v{id}"));
        id
    }

    pub fn current(&self) -> VersionId {
        self.current
    }

    /// Quiescence: no frame executes this version. Callable
    /// references are checked separately (see `callables_of`).
    pub fn quiescent(&self, v: VersionId) -> bool {
        self.versions.get(&v).is_some_and(|e| e.frames == 0)
    }

    pub fn callables_of(&self, v: VersionId) -> usize {
        self.callables.values().filter(|&&w| w == v).count()
    }

    pub fn is_retired(&self, v: VersionId) -> bool {
        self.versions.get(&v).is_some_and(|e| e.retired)
    }

    /// Retire exactly when the v2.2 lifetime rule holds: no active
    /// frame AND no reachable callable. Never retires the current
    /// version. Returns whether retirement happened.
    pub fn retire_if_quiescent(&mut self, v: VersionId) -> bool {
        if v == self.current {
            return false;
        }
        let ready = match self.versions.get(&v) {
            Some(e) if !e.retired => e.frames == 0 && self.callables_of(v) == 0,
            _ => return false,
        };
        if ready {
            self.versions.get_mut(&v).expect("checked").retired = true;
            self.log.push(format!("retired v{v}"));
        }
        ready
    }

    /// Live (unretired) versions and their tokens, for the holder
    /// task's root sync.
    pub fn live_tokens(&self) -> Vec<(VersionId, Option<GcRef>)> {
        let mut out: Vec<_> = self
            .versions
            .values()
            .filter(|e| !e.retired)
            .map(|e| (e.id, e.token))
            .collect();
        out.sort_unstable_by_key(|(v, _)| *v);
        out
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
