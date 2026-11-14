//! Task context for reload-aware code: version-pinned scopes,
//! publish-before-suspend, cooperative cancellation, callable
//! references, and layout-checked struct reads.
//!
//! A scope pins its version at entry: every `call` inside dispatches
//! to the entry version, so a suspended task resumes on old code and
//! drains normally. The closure form keeps `RefCell` borrows short
//! (a guard object would hold the borrow across user code).

use std::cell::RefCell;
use std::rc::Rc;

use spike_1_gc_scheduler::heap::GcRef;
use spike_1b_stack_switching::TaskRt;

use crate::registry::{FuncId, Registry, SpecId, VersionId};

/// A version-pinned scope, bound at `in_scope` entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scope {
    pub version: VersionId,
}

/// Cancellation signal: the scope's version was cancelled at the
/// drain timeout. Tasks exit at the boundary (early return); scope
/// guards still run, so counts stay exact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cancelled;

/// Layout check failure: data carries an older/newer layout tag than
/// the reader expects. Routed to restart, never reinterpreted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LayoutMismatch {
    pub have: i64,
    pub want: i64,
}

pub struct Hctx<'a> {
    rt: TaskRt<'a>,
    reg: Rc<RefCell<Registry>>,
    spec: SpecId,
    scopes: RefCell<Vec<Scope>>,
}

impl<'a> Hctx<'a> {
    pub(crate) fn new(rt: TaskRt<'a>, reg: Rc<RefCell<Registry>>, spec: SpecId) -> Self {
        Self {
            rt,
            reg,
            spec,
            scopes: RefCell::new(Vec::new()),
        }
    }

    fn current_version(&self) -> VersionId {
        self.reg.borrow().current()
    }

    fn is_cancelled(&self, v: VersionId) -> bool {
        self.reg
            .borrow()
            .versions
            .get(&v)
            .is_some_and(|e| e.cancelled)
    }

    fn enclosing_cancelled(&self) -> bool {
        let scopes = self.scopes.borrow();
        let reg = self.reg.borrow();
        scopes
            .iter()
            .any(|s| reg.versions.get(&s.version).is_some_and(|e| e.cancelled))
    }

    /// Enter a version-pinned scope. Binds the current version; runs
    /// `f` with the scope cookie; always exits (count/token release),
    /// reporting cancellation that arrived before, during, or after.
    pub fn in_scope<R>(&self, f: impl FnOnce(&Self, Scope) -> R) -> Result<R, Cancelled> {
        let v = self.current_version();
        if self.is_cancelled(v) {
            return Err(Cancelled);
        }
        self.enter(v);
        let scope = Scope { version: v };
        let r = f(self, scope);
        self.exit(v);
        if self.is_cancelled(v) {
            return Err(Cancelled);
        }
        Ok(r)
    }

    fn enter(&self, v: VersionId) {
        // Root the version token in this task's shadow: an active
        // frame pins its version exactly like any live heap reference.
        let token = self
            .reg
            .borrow()
            .versions
            .get(&v)
            .unwrap_or_else(|| panic!("enter scope on unknown version v{v}"))
            .token
            .expect("holder task must pump once after install before scopes run");
        self.rt.root(token);
        let mut reg = self.reg.borrow_mut();
        reg.versions.get_mut(&v).expect("checked").frames += 1;
        reg.spec_versions.entry(self.spec).or_default().insert(v);
        self.scopes.borrow_mut().push(Scope { version: v });
    }

    fn exit(&self, v: VersionId) {
        let token = self
            .reg
            .borrow()
            .versions
            .get(&v)
            .unwrap_or_else(|| panic!("exit scope on unknown version v{v}"))
            .token
            .expect("token exists while a frame runs");
        // Pop first so the recount below sees post-exit state.
        self.scopes.borrow_mut().pop();
        {
            let still = self.scopes.borrow().iter().any(|s| s.version == v);
            let mut reg = self.reg.borrow_mut();
            let e = reg.versions.get_mut(&v).expect("checked");
            debug_assert!(e.frames > 0, "scope exit without entry");
            e.frames -= 1;
            if !still && let Some(set) = reg.spec_versions.get_mut(&self.spec) {
                set.remove(&v);
            }
        }
        let removed = self.rt.remove_one_root(token);
        debug_assert!(removed, "scope exit must release its token root");
    }

    /// Cooperative cancellation point = scope boundary. Never
    /// suspends past a cancellation: checks before and after the
    /// yield, so a task cancelled mid-suspension still exits cleanly
    /// at the boundary.
    pub fn yield_point(&self) -> Result<(), Cancelled> {
        if self.enclosing_cancelled() {
            return Err(Cancelled);
        }
        self.rt.yield_now();
        if self.enclosing_cancelled() {
            return Err(Cancelled);
        }
        Ok(())
    }

    /// Dispatch through the pinning scope's version. Old frames keep
    /// calling old code after a reload; new scopes call new code.
    pub fn call(&self, scope: Scope, func: FuncId, args: &[i64]) -> i64 {
        debug_assert!(
            self.scopes.borrow().contains(&scope),
            "call outside its pinning scope",
        );
        let f = self
            .reg
            .borrow()
            .versions
            .get(&scope.version)
            .unwrap_or_else(|| panic!("call on unknown version v{}", scope.version))
            .funcs
            .get(&func)
            .unwrap_or_else(|| panic!("unknown func {func} on v{}", scope.version))
            .clone();
        f(self, args)
    }

    /// Create a callable reference to `func` at the scope's version.
    /// The heap pair (token, func) keeps the version alive through
    /// the marker for as long as the holder roots the callable —
    /// exactly the retention half of the lifetime rule.
    pub fn make_callable(&self, scope: Scope, func: FuncId) -> GcRef {
        let token = self
            .reg
            .borrow()
            .versions
            .get(&scope.version)
            .expect("callable on known version")
            .token
            .expect("holder task must pump once after install");
        let idx = self.rt.alloc_rooted_int(func as i64);
        let c = self.rt.with_frame(|rt| rt.alloc_pair(token, idx));
        self.rt.root(c);
        // The pair edge keeps the id cell alive; its direct root is
        // now redundant, so release it (keeps reclamation exact).
        self.rt.remove_one_root(idx);
        self.reg.borrow_mut().callables.insert(c, scope.version);
        c
    }

    /// Release a callable reference. The version becomes reclaimable
    /// once quiescence also holds (explicit accounting stands in for
    /// GC reachability in this spike).
    pub fn drop_callable(&self, c: GcRef) {
        // Release the holder's root (one release per acquire).
        self.rt.remove_one_root(c);
        self.reg.borrow_mut().callables.remove(&c);
    }

    /// Invoke through a callable: dispatches the version captured at
    /// creation, so old behavior survives a reload while held.
    pub fn call_callable(&self, c: GcRef, args: &[i64]) -> i64 {
        let (token, idx) = self.rt.heap_get_pair(c).expect("callable alive");
        let vid = self.rt.heap_get_int(token).expect("version token alive") as VersionId;
        let func = self.rt.heap_get_int(idx).expect("func id alive") as FuncId;
        let f = self
            .reg
            .borrow()
            .versions
            .get(&vid)
            .unwrap_or_else(|| panic!("callable of unknown version v{vid}"))
            .funcs
            .get(&func)
            .unwrap_or_else(|| panic!("unknown func {func} on v{vid}"))
            .clone();
        f(self, args)
    }

    /// Layout-checked struct read. Representation is a pair chain
    /// `(tag-cell, (f1-cell, (f2-cell, ...)))`; the tag comes first
    /// and on mismatch this returns `Err` BEFORE walking any field,
    /// so old-layout data is never read under a new layout.
    pub fn read_struct(
        &self,
        obj: GcRef,
        want_tag: i64,
        arity: usize,
    ) -> Result<Vec<i64>, LayoutMismatch> {
        let (tag_cell, mut list) = self.rt.heap_get_pair(obj).expect("struct alive");
        let have = self.rt.heap_get_int(tag_cell).expect("tag alive");
        if have != want_tag {
            self.log(format!(
                "layout mismatch: have tag {have}, want {want_tag}; routing to restart"
            ));
            return Err(LayoutMismatch {
                have,
                want: want_tag,
            });
        }
        let mut out = Vec::with_capacity(arity);
        for _ in 0..arity {
            let (f_cell, rest) = self.rt.heap_get_pair(list).expect("field alive");
            out.push(self.rt.heap_get_int(f_cell).expect("field int alive"));
            list = rest;
        }
        Ok(out)
    }

    /// Allocate a layout-tagged struct: `(tag, (f1, (f2, ...)))`
    /// ending in a terminator cell the reader never reaches (it
    /// walks exactly `arity` steps after a matching tag).
    pub fn alloc_struct(&self, tag: i64, fields: &[i64]) -> GcRef {
        let tag_cell = self.rt.alloc_rooted_int(tag);
        let mut tail = self.rt.alloc_rooted_int(0);
        for &f in fields.iter().rev() {
            let c = self.rt.alloc_rooted_int(f);
            tail = self.rt.alloc_pair(c, tail);
            self.rt.root(tail);
        }
        let s = self.rt.alloc_pair(tag_cell, tail);
        self.rt.root(s);
        s
    }

    pub fn alloc_int(&self, v: i64) -> GcRef {
        self.rt.alloc_int(v)
    }

    /// Holder-task sync: root every live version's token, allocate
    /// missing tokens, unroot retired ones. Runs once per holder
    /// poll, so registry roots are always published before any
    /// forced collection can observe them.
    pub fn sync_version_roots(&self, kept: &mut Vec<(VersionId, GcRef)>) {
        let live = self.reg.borrow().live_tokens();
        let live_ids: std::collections::HashSet<VersionId> = live.iter().map(|(v, _)| *v).collect();
        kept.retain(|(v, tok)| {
            if live_ids.contains(v) {
                return true;
            }
            self.rt.remove_one_root(*tok);
            false
        });
        for (v, tok) in live {
            if kept.iter().any(|(k, _)| *k == v) {
                continue;
            }
            let t = match tok {
                Some(t) => {
                    if !self.rt.shadow_contains(t) {
                        self.rt.root(t);
                    }
                    t
                }
                None => {
                    let t = self.rt.alloc_rooted_int(v as i64);
                    self.reg
                        .borrow_mut()
                        .versions
                        .get_mut(&v)
                        .expect("live")
                        .token = Some(t);
                    t
                }
            };
            kept.push((v, t));
        }
    }

    pub fn log(&self, msg: impl Into<String>) {
        self.rt.log(msg);
    }
}
