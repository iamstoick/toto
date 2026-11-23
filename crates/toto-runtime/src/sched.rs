//! M:N work-stealing scheduler (Phase 3, Stage 2): corosensei stackful
//! switching, 64 KiB fixed PROT_NONE-guard stacks, per-worker ready
//! deques with stealing, stop-the-world GC rendezvous, and a dedicated
//! blocking pool. One worker (`workers = 1`) behaves exactly like the
//! Stage 1 scheduler: single FIFO queue, fixed-seed RNG, deterministic.
//!
//! Ownership: all cross-task state lives in `Shared` behind mutexes,
//! owned via `Arc`. Task bodies own their executor state outright. No
//! `Rc`/`RefCell` anywhere in production. Coroutine bodies are `'static`
//! by construction (only `Arc`s and plain data cross the boundary).
//!
//! Task migration (`Send` audit): corosensei's `Coroutine` is `!Send`
//! only because of a deliberate `PhantomData<*mut ()>` marker (the
//! crate docs bless a manual `Send` impl when everything on the stack
//! at suspension is `Send`). `TaskCo` below is that manual impl. The
//! audit: at every suspension point the stack holds only the bootstrap
//! closure (owned `Arc`s + plain data), the `Executor` (`&Yielder`
//! pointing into the coroutine's OWN stack, `&ExecCode`/`&Shared`
//! into `Send + Sync` shared state), `Frame`s of `RtVal`s (all `Send`,
//! compile-asserted), and the in-flight `ToSched`/`ToTask`/`TaskResult`
//! messages (all `Send`, compile-asserted). The one `!Send` family in
//! the codebase — the interpreter's `Rc`-bearing `Value` — exists only
//! as transient conversion temporaries that are dropped BEFORE any
//! `park`/`yield_now` (blocking builtins convert to owned plain data
//! first; see `exec.rs`). A future `Rc` in any migrated type breaks the
//! compile asserts, not silently.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use corosensei::stack::{DefaultStack, Stack as _};
use corosensei::{Coroutine, CoroutineResult, Yielder};
use toto_compiler::ast::FnDecl;
use toto_compiler::ctxlog::CtxSlot;
use toto_compiler::diag::Diag;
use toto_compiler::hir::Ty;
use toto_compiler::interp::TraceFrame;
use toto_compiler::mir::{MProgram, MirBody};
use toto_compiler::span::Span;

use crate::RuntimeConfig;
use crate::channel::{Channel, ChannelId, ChannelTable, RecvWaiter, SendWaiter, TaskId};
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
    /// Native `go` body (Phase 4 JIT): spawn a task running compiled
    /// code. Mirrors `Spawn` but carries a native entry + context
    /// instead of MIR.
    SpawnNative {
        entry: crate::exec::NativeEntry,
        ctx: u64,
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
    /// Blocking file-system call. The executor has already converted
    /// arguments to owned plain data (no interpreter `Value`s cross the
    /// park — see the `Send` audit above); the task parks until the
    /// blocking pool posts the completion.
    Blocking {
        op: BlockingOp,
        span: Span,
    },
    /// Network wait (Wave 1, unix-only): the task parks until the
    /// poller thread observes readiness on its fd and completes the
    /// nonblocking syscall. Unlike `Blocking`, the waiter never
    /// occupies a pool thread — that is what makes this a poller.
    /// The full op crosses here (owned plain data only); the poller
    /// executes it under the net lock, so value conversion stays on
    /// the worker at park (marshal) and drain (convert) time, where
    /// the heap is available.
    #[cfg(unix)]
    Net {
        op: NetOp,
        span: Span,
    },
    /// Cancel a context handle (Wave 2, Stage 3): fail the scope the
    /// handle names through the standard scope-failure sweep
    /// (`cancel_scope` hook of ADR-009). Completes inline like the
    /// other non-parking requests; the caller is `Running`, never a
    /// release candidate. A forged/out-of-range handle is a loud
    /// `OpPanic`, never a silent misroute. Carries the caller's
    /// trace so the panic renders against the issuing call (an empty
    /// trace would attribute a stdlib-coordinate span to the entry
    /// file instead).
    CtxCancel {
        scope: ScopeId,
        handle: i64,
        span: Span,
        trace: Vec<TraceFrame>,
    },
}

/// A file-system call for the dedicated blocking pool. Owned plain
/// data only: pool threads never touch the GC heap, the channel
/// tables, or any scheduler state (they only push completions).
#[derive(Debug)]
pub enum BlockingOp {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        contents: Vec<u8>,
    },
    FileExists {
        path: String,
    },
    /// A foreign call (Phase 5, Stage 3). Arguments are owned plain
    /// data converted on the worker; the pool thread rebuilds values
    /// locally, calls, and returns a plain result. Callback slots are
    /// claimed on the worker (the pool only passes trampoline
    /// addresses); the worker maps slots to callables and drains after
    /// the completion arrives.
    Foreign {
        name: String,
        sig: toto_compiler::hir::HExternFn,
        args: Vec<ForeignArg>,
        slots: Vec<(usize, u64)>,
    },
    /// Blocking DNS resolution (Wave 1): `getaddrinfo` cannot be made
    /// nonblocking, so names resolve on the dedicated pool — never on
    /// a worker, never on the poller (which must never block). Unix
    /// only (same gate as the socket surface).
    #[cfg(unix)]
    DnsResolve {
        host: String,
    },
    /// Blocking TLS steps (Wave 2, Stage 1): handshakes and record
    /// I/O run on the dedicated pool — never inline on a worker
    /// (they block on the network), never on the poller (an SSL step
    /// is not a `poll()` readiness wait; see §17.5). Unix only
    /// (same gate as the socket surface).
    #[cfg(unix)]
    Tls {
        op: TlsOp,
    },
    /// Blocking database steps (Wave 3, Stage 1): checkout, connect,
    /// query, and fetch run on the dedicated pool — never inline on
    /// a worker (a checkout or a query blocks), never on the poller
    /// (a pool wait is not a readiness wait; see the Wave 3
    /// blocking-I/O policy). Unix only (same gate as the db
    /// surface: sqlite unix-wide, postgres/mysql linux-only).
    #[cfg(unix)]
    Db {
        op: DbOp,
    },
    /// Blocking LDAP steps (Wave 3, Stage 3): connect, bind, and
    /// search run on the dedicated blocking pool — never inline on
    /// a worker (an LDAP round-trip blocks), never on the poller.
    /// Linux-only (same gate as the ldap surface).
    #[cfg(target_os = "linux")]
    Ldap {
        op: LdapOp,
    },
}

/// One blocking LDAP step, as the pool thread sees it. Owned
/// plain data only: pool threads never touch the GC heap,
/// channel tables, or scheduler state beyond pushing
/// completions. Linux-only. No handle cache in v1: each step
/// connects, binds, operates, and unbinds (documented).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub enum LdapOp {
    /// Bind-only check (authentication without search).
    Bind {
        host: String,
        port: u16,
        bind_dn: String,
        password: String,
    },
    /// Subtree search returning a JSON array string.
    Search {
        host: String,
        port: u16,
        bind_dn: String,
        password: String,
        base: String,
        filter: String,
        attrs: String,
    },
}

/// One blocking database step, as the pool thread sees it. Owned
/// plain data only: pool threads never touch the GC heap, channel
/// tables, or scheduler state beyond pushing completions.
/// Unix-only. Cells are `dblib::DbCell` (kind-tagged plain data).
#[cfg(unix)]
#[derive(Debug, Clone)]
pub enum DbOp {
    /// Run a statement; the affected-row count comes back.
    Exec {
        dsn: String,
        sql: String,
        params: Vec<toto_compiler::dblib::DbCell>,
    },
    /// Run a query; all rows come back.
    Query {
        dsn: String,
        sql: String,
        params: Vec<toto_compiler::dblib::DbCell>,
    },
    /// Configure a pool (max connections, checkout timeout ms).
    PoolConfig {
        dsn: String,
        max: u32,
        timeout_ms: u64,
    },
    /// Retire a pool (test isolation, clean shutdown).
    ClosePool { dsn: String },
}

/// One blocking TLS step, as the pool thread sees it. Owned plain
/// data only: pool threads never touch the GC heap, channel tables,
/// or scheduler state beyond pushing completions. Unix-only.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub enum TlsOp {
    /// Client handshake on a connected TCP fd (chain verified
    /// against the CA PEM).
    Connect {
        fd: std::os::fd::RawFd,
        ca_pem: String,
    },
    /// Server handshake on an accepted TCP fd (cert chain + key).
    Accept {
        fd: std::os::fd::RawFd,
        cert_pem: String,
        key_pem: String,
    },
    /// One record write (may short-write; the Toto `tls_send_all`
    /// wrapper loops).
    Send {
        fd: std::os::fd::RawFd,
        bytes: Vec<u8>,
    },
    /// One record read (empty is orderly shutdown, not an error).
    Recv { fd: std::os::fd::RawFd, max: usize },
    /// Full teardown (`close_notify` + free + close).
    Close { fd: std::os::fd::RawFd },
}

/// Owned plain data crossing to pool threads: no `Rc`, no heap refs,
/// no scheduler handles — everything here is `Send` by construction.
/// Strings cross as byte buffers; NUL termination happens on the pool
/// thread inside the call frame (rooting for exactly the call).
#[derive(Debug, Clone)]
pub enum ForeignArg {
    Int(i64),
    Float(f64),
    /// String content (not NUL-terminated; the pool frame copies).
    Str(Vec<u8>),
    Ptr(u64),
    Null,
    /// A claimed callback slot: the pool passes its trampoline
    /// address as the C function pointer.
    FnPtr(u64),
}

/// A foreign call result crossing back: plain data the worker converts
/// to runtime values (heap-allocating strings there, where the heap
/// is available).
#[derive(Debug, Clone)]
pub enum ForeignRet {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Ptr(u64),
    Unit,
}

/// Pool result: bytes for reads, byte count for writes, flag for
/// exists, or the exact E0210 message the inline builtin would have
/// produced (parity by construction — same message strings).
#[derive(Debug)]
pub enum BlockingResult {
    File(Vec<u8>),
    Count(i64),
    Exists(bool),
    Fail(String),
    /// A foreign call result. Errors carry their code (rebuilt into a
    /// `Diag` on the worker with the call span) because only the
    /// worker has trace context.
    ForeignOk(ForeignRet),
    ForeignErr {
        code: &'static str,
        message: String,
    },
    /// Network results (Wave 1, unix-only): plain data the worker
    /// converts to runtime values (heap-allocating there, where the
    /// heap is available — the poller thread never touches the heap).
    /// Every variant either carries the payload or names the failure;
    /// the drain sets the task's `net_error` slot from it (see
    /// `drain_completions_locked`).
    #[cfg(unix)]
    NetInt(i64),
    /// Received bytes (empty = orderly shutdown when the error slot
    /// stays clean; empty + error = failure — see the slot rule).
    #[cfg(unix)]
    NetBytes(Vec<u8>),
    /// Loud failure: the drain sets `net_error` and resumes with
    /// `Unit`; the executor's `net_last_error` reads the slot.
    #[cfg(unix)]
    NetFail(String),
    /// DNS results (Wave 1, unix-only): same conversion discipline
    /// as the poller variants, but posted by the blocking POOL (see
    /// `BlockingOp::DnsResolve`) and accepted only by tasks parked
    /// on `"blocking"` with a matching `blocking_seq`. Separate
    /// variants so a stale pool completion can never satisfy a
    /// poller wait or vice versa (the two seq counters are
    /// independent).
    #[cfg(unix)]
    DnsAddrs(Vec<String>),
    /// DNS failure: like `NetFail` but for the blocking path.
    #[cfg(unix)]
    DnsFail(String),
    /// TLS results (Wave 2, Stage 1, unix-only): same conversion
    /// discipline as the poller variants, but posted by the blocking
    /// POOL (see `BlockingOp::Tls`) and accepted only by tasks parked
    /// on `"blocking"` with a matching `blocking_seq`. The drain sets
    /// the task's `tls_error` slot from these (never `net_error`, so
    /// interleaved net/TLS calls never clobber each other).
    #[cfg(unix)]
    TlsInt(i64),
    /// Received TLS bytes (empty = orderly shutdown when the error
    /// slot stays clean; empty + error = failure — see the slot rule).
    #[cfg(unix)]
    TlsBytes(Vec<u8>),
    /// Loud TLS failure: the drain sets `tls_error` and resumes with
    /// `Unit`; the executor's `tls_last_error` reads the slot.
    #[cfg(unix)]
    TlsFail(String),
    /// DB results (Wave 3, Stage 1, unix-only): same conversion
    /// discipline as the TLS variants, but for database pool jobs
    /// (see `BlockingOp::Db`) and accepted only by tasks parked on
    /// `"blocking"` with a matching `blocking_seq`. The drain sets
    /// the task's `db_error` slot from these (never `net_error` or
    /// `tls_error`, so interleaved db/net calls never clobber each
    /// other).
    #[cfg(unix)]
    DbInt(i64),
    /// All rows (structs per row, converted to heap values in the
    /// drain where the heap is available).
    #[cfg(unix)]
    DbRows(Vec<Vec<toto_compiler::dblib::DbCell>>),
    /// Loud DB failure: the drain sets `db_error` and resumes with
    /// `Unit`; the executor's `db_last_error` reads the slot.
    #[cfg(unix)]
    DbFail(String),
    /// LDAP results (Wave 3, Stage 3, Linux-only): same conversion
    /// discipline as the DB variants, but for directory pool jobs
    /// (see `BlockingOp::Ldap`) and accepted only by tasks parked
    /// on `"blocking"` with a matching `blocking_seq`. The drain
    /// sets the task's `ldap_error` slot from these (never
    /// `net_error`/`db_error`, so interleaved calls never clobber
    /// each other's `last_error`).
    #[cfg(target_os = "linux")]
    LdapInt(i64),
    /// Search results as a JSON array string (parsed by
    /// `gzv.std/json` — no new framing).
    #[cfg(target_os = "linux")]
    LdapStr(String),
    /// Loud LDAP failure: the drain sets `ldap_error` and resumes
    /// with `Unit`; the executor's `ldap_last_error` reads the
    /// slot.
    #[cfg(target_os = "linux")]
    LdapFail(String),
}

/// A finished pool job awaiting pickup. The scheduler turns it into
/// the task's resume response (mirroring inline builtin results).
/// `seq` must match the slot's `blocking_seq`; stale completions are
/// dropped (the task was released and re-parked meanwhile).
pub struct Completion {
    pub task: TaskId,
    pub result: BlockingResult,
    pub span: Span,
    pub seq: u64,
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

    /// Live scope count: handle validation bounds-checks against it
    /// (handles name scope indices; see the `CtxCancel` arm).
    pub fn scope_count(&self) -> usize {
        self.scopes.len()
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

/// Everything the collector needs, under one lock. Lock discipline:
/// worker code never holds `sched` while taking `state`/`scopes`/
/// `output`/`rng`; executor code never takes `sched` at all; the GC
/// rendezvous takes `gc`, releases it, and only then takes `state`.
/// No path nests these in the opposite order, so no ABBA deadlock.
pub struct State {
    pub heap: Heap,
    pub channels: ChannelTable,
    pub shadows: Vec<ShadowStack>,
    pub select_waiters: VecDeque<SelectWaiter>,
    /// Declared-state cells by slot (spec amendment, Phase 6):
    /// written by `StateSet`, read by `StateGet`, initialized by
    /// `main`'s prepended MIR. Shared across tasks (that is the
    /// point) and traced at every collection — a state-held object
    /// is live as long as its slot names it.
    pub state_vars: Vec<RtVal>,
}

/// A suspended coroutine that may resume on ANY worker thread. See the
/// module-level `Send` audit: `Coroutine` is `!Send` only through a
/// deliberate `PhantomData<*mut ()>` marker.
pub struct TaskCo(pub Coroutine<ToTask, ToSched, TaskResult, DefaultStack>);

// SAFETY: audited in the module docs. Every value on a suspended
// task's stack is `Send` (compile-asserted below for the message and
// value types; the bootstrap closure owns only `Arc`s and plain
// data; the `&Yielder` points into the coroutine's own stack, which
// migrates with it).
unsafe impl Send for TaskCo {}

fn _assert_send<T: Send>() {}
fn _assert_sync<T: Sync>() {}

/// Compile-time tripwires for the migration audit: if any of these
/// types gains a `!Send` member (notably an `Rc`), the M:N scheduler
/// stops compiling instead of silently becoming unsound.
#[allow(dead_code)]
fn _migration_asserts() {
    _assert_send::<RtVal>();
    _assert_sync::<RtVal>();
    _assert_send::<ToSched>();
    _assert_send::<ToTask>();
    _assert_send::<SelReq>();
    _assert_send::<TaskResult>();
    _assert_send::<crate::exec::Initial>();
    _assert_send::<Failure>();
    _assert_send::<BlockingOp>();
    _assert_send::<BlockingResult>();
    _assert_sync::<Shared>();
}

/// Code-version identity for heap-retained closures (Phase 6,
/// Stage 2a): which generation's module each live closure handle
/// was compiled into. Owned cross-generation by the dev driver and
/// cloned into every spawned run; entries are pruned when their
/// handle dies (see `Heap::is_live`). Arc-based shared state per
/// the Phase 3 ownership rule — no `Rc`/`RefCell` anywhere near
/// shared state (see the `_assert_send_sync` tripwires below).
pub type ClosureModules = Arc<Mutex<HashMap<GcRef, u64>>>;

/// A fresh, empty closure→module table for single-run paths
/// (interpreter, AOT products, JIT probe, non-dev `run_jit`):
/// nothing is ever retained through these.
pub fn fresh_closure_modules() -> ClosureModules {
    Arc::new(Mutex::new(HashMap::new()))
}

pub struct Shared {
    pub state: Mutex<State>,
    /// Generating code version that allocates into this run
    /// (Phase 6, Stage 2a): stamped into `closure_modules` by
    /// `task_alloc_closure` alongside each new closure handle.
    /// `0` means "no version" (single-run paths: interpreter,
    /// AOT products, JIT probe) — retention only consults it
    /// under the dev driver, which always stamps real numbers.
    pub codegen: u64,
    /// Cross-generation closure→module table (see above).
    pub closure_modules: ClosureModules,
    pub scopes: Mutex<ScopeTable>,
    pub output: Mutex<Vec<u8>>,
    pub threshold: u64,
    pub rng: Mutex<Rng>,
    /// TEST-ONLY: mirror of `RuntimeConfig::collect_at_every_safepoint_for_test`.
    pub force_collect_for_test: bool,
    /// Scheduler state (slots, queues, outcomes). Workers take this
    /// lock only for short scheduling decisions, never while running
    /// task code or holding any other lock.
    pub sched: Mutex<SchedState>,
    /// Paired with `sched`: idle workers sleep here; `notify_all`
    /// wakes them on enqueue, completion, GC request, or shutdown.
    pub sched_cv: Condvar,
    /// Stop-the-world rendezvous (see `rendezvous_collect`).
    pub gc: Mutex<GcState>,
    pub gc_cv: Condvar,
    /// Dedicated blocking pool: jobs in, completions out. Pool threads
    /// touch neither the heap nor scheduler state directly.
    pub pool: Mutex<PoolState>,
    pub pool_cv: Condvar,
    pub done: Mutex<VecDeque<Completion>>,
    /// M:N network poller (Wave 1, unix-only): registrations in,
    /// readiness completions out. The poller thread touches neither
    /// the heap nor scheduler state directly (same discipline as the
    /// pool). It blocks in `poll()`, woken via `net_wake`.
    #[cfg(unix)]
    pub net: Mutex<NetState>,
    /// Self-pipe wakeup ends: `net_wake[0]` read by the poller,
    /// `net_wake[1]` written by parker/closer/shutdown.
    #[cfg(unix)]
    pub net_wake: [std::os::fd::RawFd; 2],
    /// Fixed worker count for the run (1 = deterministic Stage 1 mode).
    pub workers: usize,
    /// Fixed blocking-pool thread count for the run.
    pub pool_threads: usize,
}

/// All slot/queue/outcome state, behind `Shared::sched`.
pub struct SchedState {
    pub slots: Vec<Slot>,
    pub queues: Vec<VecDeque<TaskId>>,
    pub main_task: TaskId,
    pub main_result: Option<TaskResult>,
    pub root_failure: Option<Failure>,
    pub park_seq: u64,
    pub tasks_spawned: u64,
    pub switches: u64,
    pub steals: u64,
    pub high_water_max: usize,
    /// Tasks not yet `Done` (spawn increments, `task_done` decrements).
    pub live: usize,
    /// Workers still inside their loop. The last one out shuts the
    /// blocking pool down so its threads exit and joins complete.
    pub workers_live: usize,
    /// Set when the run is over (all done, or deadlock claimed). Every
    /// worker loop iteration checks it first and exits when set.
    pub finished: bool,
    /// True once some worker claimed deadlock reporting (exactly one
    /// E0216 outcome per run).
    pub deadlock_claimed: bool,
    /// Completions whose settle cascade (`fail_scope` wakeups,
    /// `on_task_done` propagation) is still in flight. A completion
    /// that already decremented `live` but has not yet enqueued its
    /// wakeups is invisible to the deadlock check — the same hazard
    /// `PoolState::in_flight` covers for pool jobs — so the detector
    /// treats a nonzero count as future work and defers instead of
    /// claiming a bogus deadlock while a scope failure is still
    /// propagating.
    pub settling: usize,
}

/// Stop-the-world rendezvous state. A safepoint that needs collection
/// sets `requested` and parks; every worker (running or idle) joins at
/// its next opportunity; the last arrival collects with all shadows
/// published, then releases everyone. Single generation counter —
/// no ABA: a woken worker proceeds only when the generation moved.
pub struct GcState {
    pub requested: bool,
    pub arrived: usize,
    pub workers: usize,
    pub generation: u64,
}

/// Blocking-pool state. `shutdown` is set once the run finishes so
/// pool threads exit; workers never wait on `pool_cv`.
pub struct PoolState {
    pub jobs: VecDeque<PoolJob>,
    /// Jobs popped by pool threads but not yet completed. A job in
    /// flight is invisible in both queues; without this count the
    /// scheduler could mistake a busy pool for an idle one and declare
    /// a bogus deadlock while a read is still running.
    pub in_flight: usize,
    pub shutdown: bool,
}

/// One unit of file-system work plus its destination. `seq` matches
/// the slot's `blocking_seq` at park time (stale-completion guard).
pub struct PoolJob {
    pub task: TaskId,
    pub op: BlockingOp,
    pub span: Span,
    pub seq: u64,
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
    // Declared state (spec amendment, Phase 6): every cell is a
    // root. Slots are plain indices (never stale), so no
    // generation check applies — presence in the vector is life.
    for v in &state.state_vars {
        heap_refs_of(v, &mut out);
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

impl Shared {
    /// Stop-the-world collection rendezvous. Call with task roots
    /// published (executor safepoint) or with no running task (idle
    /// worker), holding NO locks.
    ///
    /// The first arrival sets `requested` and wakes sleepers (a 1 ms
    /// sleep timeout backs the notify up, so a missed wakeup only
    /// delays the rendezvous, never stalls it). Every worker joins at
    /// its next opportunity. The last arrival collects — with every
    /// participant parked and holding no locks, taking `state` here
    /// cannot deadlock — then releases everyone via the generation
    /// counter (no ABA: a woken worker proceeds only when the
    /// generation moved past the one it joined).
    pub fn rendezvous_collect(&self) {
        let my_gen = {
            let mut g = self.gc.lock().expect("gc lock");
            if !g.requested {
                g.requested = true;
                // Wake sleepers so they join promptly.
                self.sched_cv.notify_all();
            }
            let generation = g.generation;
            g.arrived += 1;
            if g.arrived == g.workers {
                // Last arrival: collect WITHOUT holding the gc lock.
                // Lock order is sched -> gc on the decide path, so
                // nesting gc -> sched here would cycle under M:N
                // (worker in decide holds sched and wants gc while a
                // rendezvousing worker holds gc and wants sched).
                // Dropping `g` first keeps every section below
                // single-lock; re-acquiring afterwards is safe because
                // only the last arrival collects (arrivals beyond
                // `workers` cannot match `== workers`), and waiters
                // key off `generation`, never the counter.
                drop(g);
                // Snapshot resume-response
                // pendings under `sched` first — a response sitting in
                // a slot can be the only reference to its value.
                // Every other worker is parked holding no locks and no
                // task is running, so the set is stable.
                let pending: Vec<GcRef> = {
                    let sched = self.sched.lock().expect("sched lock");
                    let mut out = Vec::new();
                    for slot in &sched.slots {
                        if slot.state == TaskState::Done {
                            continue;
                        }
                        match &slot.pending {
                            ToTask::Value(v) => heap_refs_of(v, &mut out),
                            ToTask::Selected { value: Some(v), .. } => heap_refs_of(v, &mut out),
                            _ => {}
                        }
                    }
                    out
                };
                let roots = {
                    let st = self.state.lock().expect("shared state lock");
                    let mut roots = gather_roots(&st);
                    roots.extend(pending.iter().copied());
                    roots
                };
                {
                    let mut st = self.state.lock().expect("shared state lock");
                    st.heap.collect(&roots);
                }
                {
                    let mut g = self.gc.lock().expect("gc lock");
                    g.arrived = 0;
                    g.requested = false;
                    g.generation += 1;
                }
                self.gc_cv.notify_all();
                return;
            }
            generation
        };
        let mut g = self.gc.lock().expect("gc lock");
        while g.generation == my_gen {
            g = self.gc_cv.wait(g).expect("gc cv wait");
        }
    }
}

/// Code bundle shared by all tasks of one run (all `'static`-safe).
pub struct ExecCode {
    pub prog: MProgram,
    pub structs: std::collections::HashMap<String, toto_compiler::hir::HStructDecl>,
    pub fns: Vec<FnDecl>,
    /// Per-function block graphs, cloned once per run (frames share the
    /// `Arc`; calls never clone block vecs).
    pub fn_blocks: Vec<std::sync::Arc<[toto_compiler::mir::MBlock]>>,
    /// Checked foreign signatures (Phase 5, Stage 3) for `CallForeign`.
    pub externs: std::collections::HashMap<String, toto_compiler::hir::HExternFn>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskState {
    Ready,
    /// Checked out by a worker (the coroutine lives in the worker's
    /// hands, not the slot). Deciders treat it as live-but-busy.
    Running {
        worker: usize,
    },
    Blocked {
        span: Span,
        op: &'static str,
    },
    Done,
}

pub struct Slot {
    name: String,
    co: Option<TaskCo>,
    state: TaskState,
    scope: ScopeId,
    pending: ToTask,
    stack_base: usize,
    writable_start: usize,
    high_water: Option<usize>,
    /// Generation counter for blocking-pool round trips: each park on
    /// `blocking` bumps it, and completions carry the seq they answer.
    /// A stale completion (task released and re-parked meanwhile) can
    /// never wake the wrong wait.
    blocking_seq: u64,
    /// Generation counter for poller round trips: same discipline as
    /// `blocking_seq`, for `Blocked { op: "net" }` parks.
    #[cfg(unix)]
    net_seq: u64,
    /// Last network error for this task (`None` = clean). Set on every
    /// net completion (or inline net failure); read (and cleared) by
    /// `toto_rt_net_last_error`. Plain data, never a heap ref.
    #[cfg(unix)]
    pub(crate) net_error: Option<String>,
    /// Last crypto error for this task (`None` = clean). Separate
    /// from `net_error` so interleaved net/crypto calls in one task
    /// never clobber each other's `last_error`; read (and cleared)
    /// by `toto_rt_crypto_last_error`. Set on every crypto call.
    #[cfg(unix)]
    pub(crate) crypto_error: Option<String>,
    /// Last gzip error for this task (`None` = clean). Same
    /// separate-slot discipline as crypto; read (and cleared) by
    /// `toto_rt_gzip_last_error`.
    #[cfg(unix)]
    pub(crate) gzip_error: Option<String>,
    /// Last TLS error for this task (`None` = clean). Separate from
    /// `net_error` so interleaved net/TLS calls in one task never
    /// clobber each other's `last_error`; read (and cleared) by
    /// `toto_rt_tls_last_error`. Set on every TLS pool completion.
    #[cfg(unix)]
    pub(crate) tls_error: Option<String>,
    /// Last DB error for this task (`None` = clean). Separate from
    /// `net_error`/`tls_error` so interleaved db/net calls in one
    /// task never clobber each other's `last_error`; read (and
    /// cleared) by `toto_rt_db_last_error`. Set on every DB pool
    /// completion.
    #[cfg(unix)]
    pub(crate) db_error: Option<String>,
    /// Last LDAP error for this task (`None` = clean). Separate
    /// slot, same discipline; read (and cleared) by
    /// `toto_rt_ldap_last_error`. Set on every LDAP pool
    /// completion.
    #[cfg(target_os = "linux")]
    pub(crate) ldap_error: Option<String>,
    /// Current context slot (Wave 2, Stage 3): ordered pairs plus
    /// an optional absolute monotonic-ms deadline, inherited at
    /// spawn (ADR-009). Plain data — never a heap reference, so no
    /// GC rooting is needed. The slot is authoritative for logging
    /// (which reads it without call-site plumbing) and for deadline
    /// checks (which fail fast at suspension points).
    pub(crate) ctx: CtxSlot,
    /// Last context error for this task (`None` = clean). Same
    /// separate-slot discipline as crypto/gzip: interleaved calls
    /// never clobber each other's `last_error`; read (and cleared)
    /// by `toto_rt_ctx_last_error`.
    #[cfg(unix)]
    pub(crate) ctx_error: Option<String>,
}

/// One parked network wait, as the poller sees it. Owned plain data
/// only: the poller thread never touches the GC heap, channel
/// tables, or scheduler state beyond the net table + completions.
///
/// Lifetime rule (no generations needed): a registration exists from
/// park until exactly one of (a) readiness completion, (b) `close`
/// unregister + `NetFail`, (c) fail_scope sweep + `Cancelled`. The
/// poller executes its nonblocking syscall with the net lock HELD
/// (microsecond-scale, never blocks), so a close+reuse cannot slip
/// between verification and syscall — `close` waits for the same
/// lock. Level-triggered `poll` + self-pipe wakeups mean no wakeup
/// is ever lost.
/// One parked network operation: everything the poller needs to
/// wait for readiness AND complete the nonblocking syscall, owned
/// plain data only (no heap refs). Unix-only: socket fds and
/// readiness kinds with no Windows equivalent yet (POSIX-compat
/// decision, Wave 0 deferral).
#[cfg(unix)]
#[derive(Debug, Clone)]
pub enum NetOp {
    /// `connect()` finished: wait for writability, then `SO_ERROR`.
    Connect { fd: std::os::fd::RawFd },
    /// `accept()` a listener: wait for readability.
    Accept { fd: std::os::fd::RawFd },
    /// `recv()` up to `max` bytes: wait for readability. Empty
    /// bytes on completion is orderly shutdown (not an error).
    Recv { fd: std::os::fd::RawFd, max: usize },
    /// `send()` bytes: wait for writability. Short writes return
    /// short (the Toto `net_send_all` wrapper loops).
    Send {
        fd: std::os::fd::RawFd,
        bytes: Vec<u8>,
    },
    /// `recvfrom()` up to `max` bytes: wait for readability. The
    /// peer is recorded for `net_udp_peer_*`.
    UdpRecv { fd: std::os::fd::RawFd, max: usize },
    /// `sendto()` bytes to a numeric IP: wait for writability.
    UdpSend {
        fd: std::os::fd::RawFd,
        bytes: Vec<u8>,
        ip: String,
        port: u16,
    },
}

#[cfg(unix)]
#[derive(Debug, Clone)]
struct NetReg {
    fd: std::os::fd::RawFd,
    task: TaskId,
    seq: u64,
    op: NetOp,
    span: Span,
}

#[cfg(unix)]
impl NetReg {
    fn wants_read(&self) -> bool {
        matches!(
            self.op,
            NetOp::Accept { .. } | NetOp::Recv { .. } | NetOp::UdpRecv { .. }
        )
    }
    fn wants_write(&self) -> bool {
        matches!(
            self.op,
            NetOp::Connect { .. } | NetOp::Send { .. } | NetOp::UdpSend { .. }
        )
    }
}

/// One poller wakeup outcome: either re-arm the wait (spurious
/// readiness) or a finished result to post as the completion.
#[cfg(unix)]
enum PollerDone {
    Rearm,
    Done(BlockingResult),
}

/// Execute one parked net op after readiness fired. The caller
/// holds the net lock across this call (see the lifetime rule on
/// `NetReg`); everything here is a nonblocking syscall plus plain
/// data handling — no heap, no scheduler state, no blocking.
/// Returns `Rearm` on spurious readiness (EAGAIN after a race:
/// level-triggered `poll` fired, but another consumer — only
/// possible via a close+reopen that the wakeup path already
/// handled — got there first; the caller re-registers).
#[cfg(unix)]
fn poller_do(reg: &NetReg) -> PollerDone {
    use toto_compiler::netsys as nn;
    match &reg.op {
        NetOp::Connect { fd } => match nn::so_error(*fd) {
            Ok(()) => PollerDone::Done(BlockingResult::NetInt(*fd as i64)),
            Err(e) => PollerDone::Done(BlockingResult::NetFail(e)),
        },
        NetOp::Accept { fd } => match nn::accept_nonblocking(*fd) {
            nn::AcceptOutcome::Accepted(c) => {
                nn::set_nonblocking(c).ok();
                PollerDone::Done(BlockingResult::NetInt(c as i64))
            }
            nn::AcceptOutcome::WouldBlock => PollerDone::Rearm,
            nn::AcceptOutcome::Failed(e) => PollerDone::Done(BlockingResult::NetFail(e)),
        },
        NetOp::Recv { fd, max } => match nn::recv_nonblocking(*fd, *max) {
            nn::RecvOutcome::Got(bytes) => PollerDone::Done(BlockingResult::NetBytes(bytes)),
            // Orderly shutdown reads as an empty payload; the
            // error slot stays clean (see the drain conversion).
            nn::RecvOutcome::Closed => PollerDone::Done(BlockingResult::NetBytes(Vec::new())),
            nn::RecvOutcome::WouldBlock => PollerDone::Rearm,
            nn::RecvOutcome::Failed(e) => PollerDone::Done(BlockingResult::NetFail(e)),
        },
        NetOp::Send { fd, bytes } => match nn::send_nonblocking(*fd, bytes) {
            nn::SendOutcome::Wrote(n) => PollerDone::Done(BlockingResult::NetInt(n as i64)),
            nn::SendOutcome::WouldBlock => PollerDone::Rearm,
            // A closed peer on send is an error, not a count.
            nn::SendOutcome::Closed => PollerDone::Done(BlockingResult::NetFail(
                "connection closed by peer".to_string(),
            )),
            nn::SendOutcome::Failed(e) => PollerDone::Done(BlockingResult::NetFail(e)),
        },
        NetOp::UdpRecv { fd, max } => match nn::udp_recv_from_nb(*fd, *max) {
            nn::UdpRecvOutcome::Got(bytes, host, port) => {
                // Peer recorded here (static map, no heap) so
                // `net_udp_peer_*` works without another syscall. The
                // completion carries only the bytes; the Toto wrapper
                // assembles the `UdpPacket` struct itself.
                nn::record_peer(*fd, host, port);
                PollerDone::Done(BlockingResult::NetBytes(bytes))
            }
            nn::UdpRecvOutcome::WouldBlock => PollerDone::Rearm,
            nn::UdpRecvOutcome::Failed(e) => PollerDone::Done(BlockingResult::NetFail(e)),
        },
        NetOp::UdpSend {
            fd,
            bytes,
            ip,
            port,
        } => {
            match nn::udp_send_to_nb(*fd, bytes, ip, *port) {
                nn::SendOutcome::Wrote(n) => PollerDone::Done(BlockingResult::NetInt(n as i64)),
                nn::SendOutcome::WouldBlock => PollerDone::Rearm,
                nn::SendOutcome::Closed | nn::SendOutcome::Failed(_) => {
                    // UDP has no peer-closed state; treat any
                    // failure uniformly as the OS message.
                    PollerDone::Done(BlockingResult::NetFail(nn::last_error()))
                }
            }
        }
    }
}

/// Poller-table state. Registrations are one-shot: a readiness
/// completion, a `close`, or a fail_scope sweep unregisters them —
/// exactly once, so every park ends in exactly one wakeup.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct NetState {
    regs: Vec<NetReg>,
    shutdown: bool,
}

pub struct Scheduler {
    shared: Arc<Shared>,
    code: Arc<ExecCode>,
    stack_bytes: usize,
}

pub struct RunStats {
    pub tasks_spawned: u64,
    pub switches: u64,
    /// Tasks taken from another worker's queue (0 in 1-worker mode).
    /// Stress tests assert this is nonzero under M:N: proof that work
    /// actually migrates instead of one worker doing everything.
    pub steals: u64,
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
    /// The six scheduler knobs travel as `RuntimeConfig` (one
    /// call-site struct, no ten-argument constructor); the
    /// code-version pair (`codegen` + `closure_modules`) and the
    /// incoming `heap` stay explicit — they differ per generation,
    /// unlike the config.
    pub fn new(
        code: Arc<ExecCode>,
        heap: Heap,
        codegen: u64,
        closure_modules: ClosureModules,
        config: &RuntimeConfig,
    ) -> Self {
        assert!(config.workers >= 1, "at least one worker");
        let threshold = config.gc_threshold_bytes;
        let seed = config.rng_seed;
        let stack_bytes = config.stack_bytes;
        let force_collect_for_test = config.collect_at_every_safepoint_for_test;
        let workers = config.workers;
        let pool_threads = config.blocking_pool_threads;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                heap,
                channels: ChannelTable::new(),
                shadows: Vec::new(),
                select_waiters: VecDeque::new(),
                state_vars: Vec::new(),
            }),
            codegen,
            closure_modules,
            scopes: Mutex::new(ScopeTable::new()),
            output: Mutex::new(Vec::new()),
            threshold,
            rng: Mutex::new(Rng::new(seed)),
            force_collect_for_test,
            sched: Mutex::new(SchedState {
                slots: Vec::new(),
                queues: (0..workers).map(|_| VecDeque::new()).collect(),
                main_task: TaskId(usize::MAX),
                main_result: None,
                root_failure: None,
                park_seq: 0,
                tasks_spawned: 0,
                switches: 0,
                steals: 0,
                high_water_max: 0,
                live: 0,
                workers_live: workers,
                finished: false,
                deadlock_claimed: false,
                settling: 0,
            }),
            sched_cv: Condvar::new(),
            gc: Mutex::new(GcState {
                requested: false,
                arrived: 0,
                workers,
                generation: 0,
            }),
            gc_cv: Condvar::new(),
            pool: Mutex::new(PoolState {
                jobs: VecDeque::new(),
                in_flight: 0,
                shutdown: false,
            }),
            pool_cv: Condvar::new(),
            done: Mutex::new(VecDeque::new()),
            #[cfg(unix)]
            net: Mutex::new(NetState::default()),
            #[cfg(unix)]
            net_wake: {
                let (r, w) = toto_compiler::netsys::self_pipe();
                [r, w]
            },
            workers,
            pool_threads,
        });
        Self {
            shared,
            code,
            stack_bytes,
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn code(&self) -> &Arc<ExecCode> {
        &self.code
    }

    pub fn stack_bytes(&self) -> usize {
        self.stack_bytes
    }

    /// Create a task; the new task lands on `affinity`'s queue (spawn
    /// locality — stealing rebalances). Callable from any worker: all
    /// slot/queue mutation happens under the `sched` lock, and no
    /// other lock is held here.
    fn spawn_task(
        &self,
        affinity: usize,
        name: String,
        scope: ScopeId,
        initial: Initial,
        span: Span,
    ) -> TaskId {
        // The coroutine body must be `'static`: only `Arc`s and plain
        // data cross. Lifetimes inside borrow from those `Arc`s.
        //
        // Stack allocation happens OUTSIDE the `sched` lock (mmap, no
        // shared state), so spawning never blocks scheduling.
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
        // `Initial` owns everything it needs: `Main` is an index and
        // `GoBody` owns its `Arc<MirBody>` plus capture values, so the
        // coroutine body is `'static` as corosensei requires. `id` is
        // filled in after the slot exists; the executor learns it from
        // the move closure's copy — see below.
        let (id_tx, id_rx) = std::sync::mpsc::channel::<TaskId>();
        let co =
            Coroutine::with_stack(stack, move |y: &Yielder<ToTask, ToSched>, _init: ToTask| {
                let id = id_rx.recv().expect("task id assigned before first resume");
                match initial {
                    // Native tasks (Phase 4 JIT) run compiled code
                    // with the same yielder protocol, no executor.
                    Initial::Native { entry, ctx } => entry(y, ctx, id, scope),
                    other => {
                        let mut exec = Executor::new(y, &code, &shared, scope, id);
                        exec.run_task(other, span)
                    }
                }
            });
        let mut sched = self.shared.sched.lock().expect("sched lock");
        let id = TaskId(sched.slots.len());
        // Publish the id to the coroutine body before any resume. The
        // channel send completes before the task is enqueued, so the
        // first `recv` cannot race a resume.
        id_tx.send(id).expect("task id handoff");
        self.shared
            .scopes
            .lock()
            .expect("scopes lock")
            .add_task(scope);
        {
            let mut st = self.shared.state.lock().expect("shared state lock");
            st.shadows.push(ShadowStack::new());
        }
        sched.slots.push(Slot {
            name,
            co: Some(TaskCo(co)),
            state: TaskState::Ready,
            scope,
            pending: ToTask::Unit,
            stack_base: base,
            writable_start,
            high_water: None,
            blocking_seq: 0,
            #[cfg(unix)]
            net_seq: 0,
            #[cfg(unix)]
            net_error: None,
            #[cfg(unix)]
            crypto_error: None,
            #[cfg(unix)]
            gzip_error: None,
            #[cfg(unix)]
            tls_error: None,
            #[cfg(unix)]
            db_error: None,
            #[cfg(target_os = "linux")]
            ldap_error: None,
            ctx: CtxSlot::new(),
            #[cfg(unix)]
            ctx_error: None,
        });
        sched.tasks_spawned += 1;
        sched.live += 1;
        let q = affinity.min(sched.queues.len() - 1);
        sched.queues[q].push_back(id);
        drop(sched);
        self.shared.sched_cv.notify_all();
        id
    }

    pub fn spawn_main(&self, main_idx: usize, span: Span) -> TaskId {
        let root = ScopeTable::root();
        let id = self.spawn_task(0, "main".to_string(), root, Initial::Main(main_idx), span);
        self.shared.sched.lock().expect("sched lock").main_task = id;
        id
    }

    /// Spawn `main` as native code (Phase 4 JIT): like `spawn_main`
    /// but the task runs `entry` with `ctx` instead of the MIR
    /// executor.
    pub fn spawn_native_main(
        &self,
        entry: crate::exec::NativeEntry,
        ctx: u64,
        span: Span,
    ) -> TaskId {
        let root = ScopeTable::root();
        let id = self.spawn_task(
            0,
            "main".to_string(),
            root,
            Initial::Native { entry, ctx },
            span,
        );
        self.shared.sched.lock().expect("sched lock").main_task = id;
        id
    }

    /// Live task count for drain quiescence (Phase 4 dev): every task
    /// whose slot is not `Done` — ready, running, and
    /// suspended/parked tasks all count (a parked task still holds
    /// live frames).
    pub fn live_tasks(&self) -> usize {
        let sched = self.shared.sched.lock().expect("sched lock");
        sched
            .slots
            .iter()
            .filter(|s| s.state != TaskState::Done)
            .count()
    }

    /// Drain cancellation (Phase 4 dev): fail the root scope,
    /// releasing every parked task in the run with cancellation
    /// (send/recv/select/blocking — the same sweep as a task panic).
    /// Running tasks fail fast at their next park op; tasks that
    /// never suspend keep running (cooperative limit — the drain
    /// timeout plus the dev restart fallback own that case).
    pub fn cancel_all(&self) {
        self.fail_scope(
            0,
            ScopeTable::root(),
            Failure {
                diag: Diag::new(
                    "E0214",
                    String::from(
                        "drain cancelled: reload superseded this run (tasks released by cancellation)",
                    ),
                    Span::new(0, 0),
                    None,
                ),
                trace: Vec::new(),
            },
        );
    }

    /// Conditionally wake another task: applies only while the task is
    /// still `Blocked`. If a scope failure released it meanwhile (its
    /// `Cancelled` stands) or another pairing already woke it, the
    /// response is dropped instead of overwriting. This is what makes
    /// release-vs-pair races benign: exactly one response ever lands.
    /// (A residual teardown race remains: a sender may complete while
    /// its released receiver drops the value. The scope is failing
    /// anyway; the sender fail-fasts at its next suspension.)
    fn enqueue_if_blocked(&self, worker: usize, id: TaskId, resp: ToTask) {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        let slot = &mut sched.slots[id.0];
        if !matches!(slot.state, TaskState::Blocked { .. }) {
            return;
        }
        slot.pending = resp;
        slot.state = TaskState::Ready;
        let q = worker.min(sched.queues.len() - 1);
        sched.queues[q].push_back(id);
        drop(sched);
        self.shared.sched_cv.notify_all();
    }

    /// Wake a task with a response onto a worker's queue (spawn locality
    /// and wake locality: the worker that produces the wake keeps the
    /// task). Short `sched`-lock section only; callers must hold NO
    /// other lock (see module lock discipline). A task that is already
    /// `Ready` (a second wakeup before resume) keeps its single queue
    /// entry with the newest response — never queued twice.
    ///
    /// Use ONLY for the running task's own response (it cannot have
    /// been released: releases hit only `Blocked` tasks). Wakes of any
    /// other task must go through `enqueue_if_blocked`.
    fn enqueue_on(&self, worker: usize, id: TaskId, resp: ToTask) {
        {
            let mut sched = self.shared.sched.lock().expect("sched lock");
            Self::enqueue_locked(&mut sched, worker, id, resp);
        }
        self.shared.sched_cv.notify_all();
    }

    /// `enqueue_on` with the `sched` lock already held: wake a task with
    /// a response onto a worker's queue without locking or notifying.
    /// The caller must `notify_all` on `sched_cv` after releasing the
    /// lock. Same Ready-entry invariant as `enqueue_on`.
    fn enqueue_locked(sched: &mut SchedState, worker: usize, id: TaskId, resp: ToTask) {
        let slot = &mut sched.slots[id.0];
        debug_assert!(slot.state != TaskState::Done, "enqueue a live task");
        slot.pending = resp;
        if slot.state == TaskState::Ready {
            return;
        }
        slot.state = TaskState::Ready;
        let q = worker.min(sched.queues.len() - 1);
        sched.queues[q].push_back(id);
    }

    /// Main entry: spawn pool + worker threads, join everything, and
    /// compose the outcome. `spawn_main` must have run first.
    pub fn run(&self) -> SchedOutcome {
        std::thread::scope(|scope| {
            for _ in 0..self.shared.pool_threads {
                scope.spawn(|| Self::pool_loop(&self.shared));
            }
            #[cfg(unix)]
            scope.spawn(|| Self::poller_loop(&self.shared));
            for worker in 0..self.shared.workers {
                scope.spawn(move || self.worker_loop(worker));
            }
        });
        // The poller closed the read end on shutdown; close the write
        // end here so one run never leaks an fd into the next (the
        // corpus runs 100+ programs in one test process).
        #[cfg(unix)]
        toto_compiler::netsys::close(self.shared.net_wake[1]).ok();
        // All workers exited and the last one shut the pool down, so
        // every thread joined. Compose the outcome.
        let (collections, allocated, reclaimed) = {
            let st = self.shared.state.lock().expect("shared state lock");
            let s = st.heap.stats();
            (s.collections, s.allocated_total, s.reclaimed_total)
        };
        let (main, root_failure, tasks_spawned, switches, steals, high_water_max) = {
            let mut sched = self.shared.sched.lock().expect("sched lock");
            (
                sched.main_result.take().expect("main finished"),
                sched.root_failure.take(),
                sched.tasks_spawned,
                sched.switches,
                sched.steals,
                sched.high_water_max,
            )
        };
        SchedOutcome {
            main,
            root_failure,
            stats: RunStats {
                tasks_spawned,
                switches,
                steals,
                high_water_max,
                collections,
                allocated,
                reclaimed,
            },
            output: std::mem::take(&mut *self.shared.output.lock().expect("output lock")),
        }
    }

    /// Check a queued task out for running: Ready -> Running, coroutine
    /// and pending response move to the worker. Returns `None` for a
    /// non-Ready entry (defensive: the queue/Ready invariant says this
    /// is unreachable; the caller requeues and moves on, which is
    /// fail-safe toward deadlock detection rather than a bad resume).
    fn take_running(
        sched: &mut SchedState,
        worker: usize,
        id: TaskId,
    ) -> Option<(TaskId, TaskCo, ToTask)> {
        let slot = &mut sched.slots[id.0];
        if slot.state != TaskState::Ready {
            return None;
        }
        slot.state = TaskState::Running { worker };
        let co = slot.co.take().expect("ready task owns its coroutine");
        let input = std::mem::replace(&mut slot.pending, ToTask::Unit);
        sched.switches += 1;
        Some((id, co, input))
    }

    /// One worker thread: take tasks from the local queue, steal when
    /// idle, sleep when there is nothing to do. Pool completions are
    /// drained inside the decide critical section (see
    /// `drain_completions_locked`).
    fn worker_loop(&self, worker: usize) {
        loop {
            enum Step {
                Run(TaskId, TaskCo, ToTask),
                JoinGc,
                Exit,
            }
            let (step, woke) = {
                let mut sched = self.shared.sched.lock().expect("sched lock");
                if sched.finished {
                    (Step::Exit, false)
                } else {
                    // Drain pool completions INSIDE the decide section
                    // (see `drain_completions_locked`): the drain and
                    // the queue/deadlock decision below are one atomic
                    // section, so no worker can observe an idle-looking
                    // pool while a drained completion is still waiting
                    // to be enqueued.
                    let woke = self.drain_completions_locked(worker, &mut sched);
                    if self.shared.gc.lock().expect("gc lock").requested {
                        // A collection is rendezvousing: join it instead
                        // of taking work (this worker runs nothing right
                        // now, so its only duty is arriving).
                        (Step::JoinGc, woke)
                    } else {
                        // Local queue, then steal in worker order after
                        // self (one worker: no other queues exist, so this
                        // is exactly the old FIFO).
                        let n = sched.queues.len();
                        let mut found = None;
                        let mut found_stolen = false;
                        for k in 0..n {
                            let q = (worker + k) % n;
                            // Pop at most one entry per queue per pass (the
                            // queue/Ready invariant says it is `Ready`; a
                            // non-`Ready` entry is requeued defensively and
                            // reconsidered next pass — fail-safe, see
                            // `take_running`).
                            if let Some(id) = sched.queues[q].pop_front() {
                                match Self::take_running(&mut sched, worker, id) {
                                    Some(t) => {
                                        found = Some(t);
                                        found_stolen = q != worker;
                                        break;
                                    }
                                    None => sched.queues[q].push_back(id),
                                }
                            }
                        }
                        match found {
                            Some((id, co, input)) => {
                                if found_stolen {
                                    sched.steals += 1;
                                }
                                (Step::Run(id, co, input), woke)
                            }
                            None => {
                                if sched.live == 0 {
                                    sched.finished = true;
                                    self.shared.sched_cv.notify_all();
                                    (Step::Exit, woke)
                                } else {
                                    let any_running = sched
                                        .slots
                                        .iter()
                                        .any(|s| matches!(s.state, TaskState::Running { .. }));
                                    // Pool work outstanding (a job a pool
                                    // thread will complete, or an undrained
                                    // completion) is future work. So is a
                                    // live poller registration: readiness
                                    // (or a peer error) is an external
                                    // wakeup the worker cannot see yet —
                                    // the poller will post it. Lock order
                                    // is sched -> net (op_net's order).
                                    //
                                    // CHECK ORDER is soundness-critical:
                                    // pool, then net registrations, then
                                    // the completion queue LAST. The
                                    // poller posts a completion BEFORE
                                    // removing its registration (two
                                    // separate lock sections), so a
                                    // worker that checked `done` before
                                    // `regs` could observe a
                                    // posted-but-not-yet-drained
                                    // completion as absent and claim a
                                    // false deadlock (Wave 1 race fix:
                                    // lost-wakeup false positive under
                                    // GC-pressure timing). Checking
                                    // registrations first closes the
                                    // hole: any completion whose
                                    // registration is already gone was
                                    // posted before the regs check, so
                                    // the later `done` check sees it.
                                    // The pool needs no order (its post
                                    // + `in_flight` decrement hold both
                                    // locks), and `settling` rides the
                                    // `sched` lock already held here.
                                    let pool_active = {
                                        let pool = self.shared.pool.lock().expect("pool lock");
                                        !pool.jobs.is_empty() || pool.in_flight > 0
                                    } || {
                                        #[cfg(unix)]
                                        {
                                            !self
                                                .shared
                                                .net
                                                .lock()
                                                .expect("net lock")
                                                .regs
                                                .is_empty()
                                        }
                                        #[cfg(not(unix))]
                                        {
                                            false
                                        }
                                    } || !self
                                        .shared
                                        .done
                                        .lock()
                                        .expect("done lock")
                                        .is_empty();
                                    if !any_running && !pool_active && sched.settling == 0 {
                                        // Nobody can run or wake, and no
                                        // completion is still settling its
                                        // wakeups: deadlock (spec §13.2).
                                        // A nonzero `settling` count (like
                                        // `pool.in_flight`) is future work:
                                        // defer to the next pass instead of
                                        // claiming while a scope failure is
                                        // still propagating.
                                        // Exactly one worker
                                        // claims the outcome; the rest see
                                        // `finished` and exit.
                                        if !sched.deadlock_claimed {
                                            sched.deadlock_claimed = true;
                                            let blocked: Vec<(TaskId, Span, &'static str, String)> =
                                                sched
                                                    .slots
                                                    .iter()
                                                    .enumerate()
                                                    .filter_map(|(i, s)| match s.state {
                                                        TaskState::Blocked { span, op } => Some((
                                                            TaskId(i),
                                                            span,
                                                            op,
                                                            s.name.clone(),
                                                        )),
                                                        _ => None,
                                                    })
                                                    .collect();
                                            debug_assert!(
                                                !blocked.is_empty(),
                                                "live tasks but none blocked"
                                            );
                                            let (_, first_span, _, _) = blocked[0];
                                            let mut msg = format!(
                                                "deadlock: no runnable tasks remain ({} blocked with no one left to wake them)",
                                                blocked.len()
                                            );
                                            for (TaskId(i), span, op, name) in &blocked {
                                                msg.push_str(&format!(
                                                "; task{i} `{name}` blocked in `{op}` (bytes {}..{})",
                                                span.start, span.end
                                            ));
                                            }
                                            let diag = Diag::new("E0216", msg, first_span, None);
                                            sched.main_result = Some(TaskResult::Panic {
                                                diag,
                                                trace: Vec::new(),
                                            });
                                            sched.finished = true;
                                            self.shared.sched_cv.notify_all();
                                        }
                                        (Step::Exit, woke)
                                    } else {
                                        // Sleep briefly: steady-state idle
                                        // costs nothing, and the timeout
                                        // guarantees GC rendezvous progress
                                        // even if a notify is missed. Wake
                                        // stealers first if the drain above
                                        // enqueued work onto our queue.
                                        if woke {
                                            self.shared.sched_cv.notify_all();
                                        }
                                        let (guard, _) = self
                                            .shared
                                            .sched_cv
                                            .wait_timeout(
                                                sched,
                                                std::time::Duration::from_millis(1),
                                            )
                                            .expect("sched cv wait");
                                        drop(guard);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            };
            // The drain may have enqueued another worker's task onto our
            // queue: let stealers (and our next pass) see it promptly.
            if woke {
                self.shared.sched_cv.notify_all();
            }
            match step {
                Step::Exit => {
                    // Last worker out shuts the pool down so its
                    // threads (blocked on `pool_cv`) exit and the
                    // scope can join everything.
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    sched.workers_live -= 1;
                    if sched.workers_live == 0 {
                        let mut pool = self.shared.pool.lock().expect("pool lock");
                        pool.shutdown = true;
                        drop(pool);
                        self.shared.pool_cv.notify_all();
                        // Same for the poller (unix-only): set the
                        // flag and poke the pipe so its `poll`
                        // wakes, sees shutdown, and returns.
                        #[cfg(unix)]
                        {
                            self.shared.net.lock().expect("net lock").shutdown = true;
                            toto_compiler::netsys::poke_pipe(self.shared.net_wake[1]);
                        }
                    }
                    return;
                }
                Step::JoinGc => {
                    self.shared.rendezvous_collect();
                }
                Step::Run(id, co, input) => {
                    let mut co = co.0;
                    let outcome = co.resume(input);
                    match outcome {
                        CoroutineResult::Yield(req) => {
                            {
                                let mut sched = self.shared.sched.lock().expect("sched lock");
                                sched.slots[id.0].co = Some(TaskCo(co));
                            }
                            self.handle_req(worker, id, req);
                        }
                        CoroutineResult::Return(res) => {
                            self.task_done(worker, id, res);
                        }
                    }
                }
            }
        }
    }

    /// Apply pool completions: each becomes the parked task's resume
    /// response — unless the task moved on (released, then re-parked on
    /// something else), in which case the stale completion is dropped.
    /// Drain pool completions with the `sched` lock already held (the
    /// caller — the decide section of `worker_loop` — must
    /// `notify_all` on `sched_cv` after releasing the lock if anything
    /// was enqueued). Runs INSIDE the decide critical section on
    /// purpose: a completion popped from `done` is still invisible to
    /// the deadlock check until its task is enqueued, so draining and
    /// deciding must be one atomic section — otherwise a worker can
    /// claim deadlock while another worker holds a drained completion
    /// that has not been enqueued yet (lost wakeup → bogus E0216).
    /// Lock order: `sched` → `done`, `sched` → `state` (see the module
    /// discipline). The `File` conversion allocates WITHOUT a
    /// safepoint: our worker is running (not parked at the rendezvous),
    /// so no collection can run concurrently; the response lands in the
    /// slot before release and is published by the next rendezvous.
    fn drain_completions_locked(&self, worker: usize, sched: &mut SchedState) -> bool {
        let completions: Vec<Completion> = {
            let mut done = self.shared.done.lock().expect("done lock");
            done.drain(..).collect()
        };
        let mut enqueued = false;
        for c in completions {
            // Network completions (unix-only) take a separate path
            // with their own stale guard (`"net"` + `net_seq`, or
            // `"blocking"` + `blocking_seq` for pool-posted DNS) and
            // set the task's `net_error` slot. The split keeps the
            // two seq counters from ever cross-accepting.
            #[cfg(unix)]
            {
                use BlockingResult as B;
                let net_kind: Option<bool> = match &c.result {
                    B::NetInt(_)
                    | B::NetBytes(_)
                    | B::NetFail(_)
                    | B::DnsAddrs(_)
                    | B::DnsFail(_)
                    | B::TlsInt(_)
                    | B::TlsBytes(_)
                    | B::TlsFail(_)
                    | B::DbInt(_)
                    | B::DbRows(_)
                    | B::DbFail(_) => Some(matches!(
                        c.result,
                        B::NetInt(_) | B::NetBytes(_) | B::NetFail(_)
                    )),
                    // LDAP rides the same pool pre-pass (never
                    // the poller — always `Some(false)`); the
                    // arm vanishes where the variants do.
                    #[cfg(target_os = "linux")]
                    B::LdapInt(_) | B::LdapStr(_) | B::LdapFail(_) => Some(false),
                    _ => None,
                };
                if let Some(is_poller) = net_kind {
                    // TLS completions ride the pool (`TlsOp`), never
                    // the poller — but they take the same pre-pass so
                    // a stale pool completion can never satisfy a
                    // poller wait or vice versa.
                    let is_tls = matches!(c.result, B::TlsInt(_) | B::TlsBytes(_) | B::TlsFail(_));
                    let is_db = matches!(c.result, B::DbInt(_) | B::DbRows(_) | B::DbFail(_));
                    #[cfg(target_os = "linux")]
                    let is_ldap =
                        matches!(c.result, B::LdapInt(_) | B::LdapStr(_) | B::LdapFail(_));
                    #[cfg(not(target_os = "linux"))]
                    let is_ldap = false;
                    let Completion {
                        task,
                        result,
                        span: _,
                        seq,
                    } = c;
                    let (resp, err): (ToTask, Option<String>) = match result {
                        B::NetInt(n) => (ToTask::Value(RtVal::Int(n)), None),
                        B::TlsInt(n) => (ToTask::Value(RtVal::Int(n)), None),
                        B::DbInt(n) => (ToTask::Value(RtVal::Int(n)), None),
                        B::DbRows(rows) => {
                            // Rows cross back as the framed string
                            // (same grammar pure Toto decodes on
                            // the interpreter path — one wrapper,
                            // one decoder, both engines). Payloads
                            // are lossy-converted at read time, so
                            // the framing is always valid UTF-8.
                            let framed =
                                String::from_utf8(toto_compiler::dblib::encode_rows(&rows))
                                    .unwrap_or_default();
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                st.heap.alloc_str(&framed)
                            };
                            (ToTask::Value(RtVal::Heap(r)), None)
                        }
                        B::DbFail(m) => (ToTask::Value(RtVal::Unit), Some(m)),
                        B::NetBytes(bytes) => {
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                let elems: Vec<RtVal> =
                                    bytes.into_iter().map(|b| RtVal::Int(b as i64)).collect();
                                st.heap.alloc_array(elems)
                            };
                            (ToTask::Value(RtVal::Heap(r)), None)
                        }
                        B::TlsBytes(bytes) => {
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                let elems: Vec<RtVal> =
                                    bytes.into_iter().map(|b| RtVal::Int(b as i64)).collect();
                                st.heap.alloc_array(elems)
                            };
                            (ToTask::Value(RtVal::Heap(r)), None)
                        }
                        B::NetFail(m) => (ToTask::Value(RtVal::Unit), Some(m)),
                        B::TlsFail(m) => (ToTask::Value(RtVal::Unit), Some(m)),
                        #[cfg(target_os = "linux")]
                        B::LdapInt(n) => (ToTask::Value(RtVal::Int(n)), None),
                        #[cfg(target_os = "linux")]
                        B::LdapStr(s) => {
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                st.heap.alloc_str(&s)
                            };
                            (ToTask::Value(RtVal::Heap(r)), None)
                        }
                        #[cfg(target_os = "linux")]
                        B::LdapFail(m) => (ToTask::Value(RtVal::Unit), Some(m)),
                        B::DnsAddrs(addrs) => {
                            // Comma-joined contract (see `gzv.std/net`):
                            // IPv6 has colons, never commas, so the
                            // wrapper splits losslessly; the foreign
                            // signature promises `string`, never an
                            // array (the Array<int> carve-out is the
                            // only foreign array shape).
                            let joined = addrs.join(",");
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                st.heap.alloc_str(&joined)
                            };
                            (ToTask::Value(RtVal::Heap(r)), None)
                        }
                        B::DnsFail(m) => {
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                st.heap.alloc_str("")
                            };
                            (ToTask::Value(RtVal::Heap(r)), Some(m))
                        }
                        _ => unreachable!("net-kind match is exhaustive"),
                    };
                    let slot = &mut sched.slots[task.0];
                    let live = if is_poller {
                        matches!(slot.state, TaskState::Blocked { op: "net", .. })
                            && slot.net_seq == seq
                    } else {
                        matches!(slot.state, TaskState::Blocked { op: "blocking", .. })
                            && slot.blocking_seq == seq
                    };
                    if live {
                        if is_tls {
                            slot.tls_error = err;
                        } else if is_db {
                            slot.db_error = err;
                        } else if is_ldap {
                            // `is_ldap` is always false where the
                            // variants do not exist; the dead branch
                            // still compiles (it just drops).
                            #[cfg(target_os = "linux")]
                            {
                                slot.ldap_error = err;
                            }
                            #[cfg(not(target_os = "linux"))]
                            {
                                let _ = err;
                            }
                        } else {
                            slot.net_error = err;
                        }
                        Self::enqueue_locked(sched, worker, task, resp);
                        enqueued = true;
                    } else {
                        // Stale completion for a task that moved on; drop it.
                    }
                    continue;
                }
            }
            let resp = match c.result {
                BlockingResult::File(bytes) => {
                    let r = {
                        let mut st = self.shared.state.lock().expect("shared state lock");
                        st.heap.alloc_str(&String::from_utf8_lossy(&bytes))
                    };
                    ToTask::Value(RtVal::Heap(r))
                }
                BlockingResult::Count(n) => ToTask::Value(RtVal::Int(n)),
                BlockingResult::Exists(b) => ToTask::Value(RtVal::Bool(b)),
                BlockingResult::Fail(msg) => ToTask::OpPanic {
                    diag: Diag::new("E0210", msg, c.span, None),
                },
                BlockingResult::ForeignOk(ret) => {
                    let v = match ret {
                        ForeignRet::Int(n) => RtVal::Int(n),
                        ForeignRet::Float(f) => RtVal::Float(f),
                        ForeignRet::Ptr(p) => RtVal::Ptr(p),
                        ForeignRet::Unit => RtVal::Unit,
                        ForeignRet::Str(bytes) => {
                            let s = String::from_utf8_lossy(&bytes).into_owned();
                            let r = {
                                let mut st = self.shared.state.lock().expect("shared state lock");
                                st.heap.alloc_str(&s)
                            };
                            RtVal::Heap(r)
                        }
                    };
                    ToTask::Value(v)
                }
                BlockingResult::ForeignErr { code, message } => ToTask::OpPanic {
                    diag: Diag::new(code, message, c.span, None),
                },
                // Network results drain in the pre-pass above; reaching
                // here would mean a stale classification — impossible
                // (the pre-pass matches the same set), so fail loud.
                #[cfg(unix)]
                _ => unreachable!("net completion escaped the net drain path"),
            };
            // Stale-completion guard: only a task still parked on
            // `blocking` with the matching seq takes the response.
            // Anything else (released, re-parked, done) drops it.
            // (Net completions never reach here; the pre-pass above
            // takes them with their own `"net"` guard.)
            let slot = &mut sched.slots[c.task.0];
            let live = matches!(slot.state, TaskState::Blocked { op: "blocking", .. })
                && slot.blocking_seq == c.seq;
            if live {
                Self::enqueue_locked(sched, worker, c.task, resp);
                enqueued = true;
            }
        }
        enqueued
    }

    /// Run one foreign call on a pool thread: rebuild values locally
    /// (owned by this frame — rooting for exactly the call), call
    /// with the worker-claimed slots, and return a plain result. The
    /// pool never releases slots (single-owner rule: the worker
    /// releases at drain or on call failure); failed calls report
    /// `(code, message)` for the worker to rebuild into a `Diag`.
    fn run_foreign(
        name: &str,
        sig: &toto_compiler::hir::HExternFn,
        args: &[ForeignArg],
        slots: &[(usize, u64)],
    ) -> BlockingResult {
        use toto_compiler::interp::Value;
        let mut vs = Vec::with_capacity(args.len());
        for a in args {
            vs.push(match a {
                ForeignArg::Int(n) => Value::Int(*n),
                ForeignArg::Float(f) => Value::Float(*f),
                ForeignArg::Str(b) => match String::from_utf8(b.clone()) {
                    Ok(s) => Value::Str(std::rc::Rc::new(s)),
                    Err(_) => {
                        return BlockingResult::ForeignErr {
                            code: "E0210",
                            message: "foreign string argument is not valid UTF-8".to_string(),
                        };
                    }
                },
                ForeignArg::Ptr(p) => Value::Ptr(*p),
                ForeignArg::Null => Value::None,
                // A function-typed argument arrives as its trampoline
                // slot; the pool passes the address without claiming.
                // The value shape is irrelevant (only `fn_arg`'s slot
                // lookup matters), so a unit placeholder suffices.
                ForeignArg::FnPtr(_) => Value::Unit,
            });
        }
        // The worker pre-claimed every callback slot and mapped it to
        // its callable before submitting, so the completion carries no
        // slot info back: the map is intact on wake, and drain resolves
        // it. Slot release is worker-owned (drain, call failure, or
        // task teardown).
        let span = sig.span;
        match toto_compiler::ffi::call_preclaimed(name, sig, &vs, slots, span) {
            Ok(ret) => match ret {
                Value::Int(n) => BlockingResult::ForeignOk(ForeignRet::Int(n)),
                Value::Float(f) => BlockingResult::ForeignOk(ForeignRet::Float(f)),
                Value::Str(s) => BlockingResult::ForeignOk(ForeignRet::Str(s.as_bytes().to_vec())),
                Value::Ptr(p) => BlockingResult::ForeignOk(ForeignRet::Ptr(p)),
                Value::Unit => BlockingResult::ForeignOk(ForeignRet::Unit),
                other => BlockingResult::ForeignErr {
                    code: "E0204",
                    message: format!(
                        "internal error: foreign function returned {}",
                        other.type_name()
                    ),
                },
            },
            Err((diag, _)) => BlockingResult::ForeignErr {
                code: diag.code,
                message: diag.message,
            },
        }
    }

    /// Run one TLS step on a pool thread (Wave 2, Stage 1): the fd is
    /// forced blocking on entry (harmless when already blocking), the
    /// handshake/record call runs to completion, and the result posts
    /// as a `Tls*` completion for the drain pre-pass. No heap, no
    /// scheduler state — owned plain data only.
    #[cfg(unix)]
    fn run_tls(op: &TlsOp) -> BlockingResult {
        use toto_compiler::tlslib as tt;
        match op {
            TlsOp::Connect { fd, ca_pem } => match tt::client_connect(*fd, ca_pem) {
                Ok(()) => BlockingResult::TlsInt(*fd as i64),
                Err(e) => BlockingResult::TlsFail(e),
            },
            TlsOp::Accept {
                fd,
                cert_pem,
                key_pem,
            } => match tt::server_accept(*fd, cert_pem, key_pem) {
                Ok(()) => BlockingResult::TlsInt(*fd as i64),
                Err(e) => BlockingResult::TlsFail(e),
            },
            TlsOp::Send { fd, bytes } => match tt::send(*fd, bytes) {
                Ok(n) => BlockingResult::TlsInt(n as i64),
                Err(e) => BlockingResult::TlsFail(e),
            },
            TlsOp::Recv { fd, max } => match tt::recv(*fd, *max) {
                Ok(bytes) => BlockingResult::TlsBytes(bytes),
                Err(e) => BlockingResult::TlsFail(e),
            },
            TlsOp::Close { fd } => {
                toto_compiler::netsys::forget_peer(*fd);
                match tt::shutdown_close(*fd) {
                    Ok(()) => BlockingResult::TlsInt(0),
                    Err(e) => BlockingResult::TlsFail(e),
                }
            }
        }
    }

    /// Run one database step on a pool thread (Wave 3, Stage 1):
    /// the checkout (wait with timeout), connect, query, and fetch
    /// all run to completion, and the result posts as a `Db*`
    /// completion for the drain pre-pass. No heap, no scheduler
    /// state — owned plain data only.
    #[cfg(unix)]
    fn run_db(op: &DbOp) -> BlockingResult {
        use toto_compiler::dblib as db;
        match op {
            DbOp::Exec { dsn, sql, params } => match db::db_exec(dsn, sql, params) {
                Ok(n) => BlockingResult::DbInt(n),
                Err(e) => BlockingResult::DbFail(e),
            },
            DbOp::Query { dsn, sql, params } => match db::db_query(dsn, sql, params) {
                Ok(rows) => BlockingResult::DbRows(rows),
                Err(e) => BlockingResult::DbFail(e),
            },
            DbOp::PoolConfig {
                dsn,
                max,
                timeout_ms,
            } => match db::db_pool_config(dsn, *max, *timeout_ms) {
                Ok(()) => BlockingResult::DbInt(0),
                Err(e) => BlockingResult::DbFail(e),
            },
            DbOp::ClosePool { dsn } => {
                db::db_close_pool(dsn);
                BlockingResult::DbInt(0)
            }
        }
    }

    /// Run one LDAP step on a pool thread (Wave 3, Stage 3):
    /// connect, bind, and operate run to completion inside this
    /// call (no handle cache — each step is self-contained), and
    /// the result posts as an `Ldap*` completion. No heap, no
    /// scheduler state — owned plain data only.
    #[cfg(target_os = "linux")]
    fn run_ldap(op: &LdapOp) -> BlockingResult {
        use toto_compiler::ldaplib as ldap;
        match op {
            LdapOp::Bind {
                host,
                port,
                bind_dn,
                password,
            } => match ldap::bind(host, *port, bind_dn, password) {
                Ok(()) => BlockingResult::LdapInt(0),
                Err(e) => BlockingResult::LdapFail(e),
            },
            LdapOp::Search {
                host,
                port,
                bind_dn,
                password,
                base,
                filter,
                attrs,
            } => match ldap::search(host, *port, bind_dn, password, base, filter, attrs) {
                Ok(json) => BlockingResult::LdapStr(json),
                Err(e) => BlockingResult::LdapFail(e),
            },
        }
    }

    /// Dedicated blocking-pool thread: run file-system jobs, post
    /// completions, never touch the heap or scheduler state directly.
    /// Exits once the last worker sets `shutdown`.
    fn pool_loop(shared: &Arc<Shared>) {
        loop {
            let job = {
                let mut pool = shared.pool.lock().expect("pool lock");
                while pool.jobs.is_empty() && !pool.shutdown {
                    pool = shared.pool_cv.wait(pool).expect("pool cv wait");
                }
                if pool.jobs.is_empty() {
                    debug_assert!(pool.shutdown, "pool wakeup with no job and no shutdown");
                    return;
                }
                pool.in_flight += 1;
                pool.jobs.pop_front().expect("pool job present")
            };
            let result = match job.op {
                BlockingOp::ReadFile { path } => match std::fs::read(&path) {
                    Ok(bytes) => BlockingResult::File(bytes),
                    Err(e) => BlockingResult::Fail(format!("couldn't read file `{path}`: {e}")),
                },
                BlockingOp::WriteFile { path, contents } => {
                    let n = contents.len() as i64;
                    match std::fs::write(&path, &contents) {
                        Ok(()) => BlockingResult::Count(n),
                        Err(e) => {
                            BlockingResult::Fail(format!("couldn't write file `{path}`: {e}"))
                        }
                    }
                }
                BlockingOp::FileExists { path } => {
                    BlockingResult::Exists(std::path::Path::new(&path).exists())
                }
                BlockingOp::Foreign {
                    name,
                    sig,
                    args,
                    slots,
                } => Self::run_foreign(&name, &sig, &args, &slots),
                #[cfg(unix)]
                BlockingOp::DnsResolve { host } => {
                    match toto_compiler::netsys::resolve_blocking(&host) {
                        Ok(addrs) => BlockingResult::DnsAddrs(addrs),
                        Err(e) => BlockingResult::DnsFail(e),
                    }
                }
                #[cfg(unix)]
                BlockingOp::Tls { op } => {
                    // Closing unregisters first (same order as
                    // `net_close` on workers): a parked wait on a
                    // recycled fd number fails loudly instead of
                    // operating on the new socket. TLS fds never park
                    // on the poller, so this is a no-op sweep today —
                    // kept for the fd-reuse invariant, not the common
                    // case.
                    if let TlsOp::Close { fd } = &op {
                        Self::net_unregister_fd(shared, *fd, "socket closed");
                    }
                    Self::run_tls(&op)
                }
                #[cfg(unix)]
                BlockingOp::Db { op } => Self::run_db(&op),
                #[cfg(target_os = "linux")]
                BlockingOp::Ldap { op } => Self::run_ldap(&op),
            };
            {
                let mut pool = shared.pool.lock().expect("pool lock");
                let mut done = shared.done.lock().expect("done lock");
                done.push_back(Completion {
                    task: job.task,
                    result,
                    span: job.span,
                    seq: job.seq,
                });
                pool.in_flight -= 1;
            }
            shared.sched_cv.notify_all();
        }
    }

    /// Unregister every wait on one fd and wake each waiter with
    /// `NetFail("socket closed")`. Called by `close` (inline, on the
    /// worker) and by the fail_scope sweep path. Draining drops the
    /// completions of tasks that already moved on (seq guard), so
    /// over-waking is impossible — every removed registration ends
    /// in exactly one wakeup.
    #[cfg(unix)]
    pub(crate) fn net_unregister_fd(shared: &Shared, fd: std::os::fd::RawFd, why: &str) {
        let mut net = shared.net.lock().expect("net lock");
        let mut i = 0;
        let mut done = shared.done.lock().expect("done lock");
        while i < net.regs.len() {
            if net.regs[i].fd == fd {
                let reg = net.regs.remove(i);
                done.push_back(Completion {
                    task: reg.task,
                    result: BlockingResult::NetFail(why.to_string()),
                    span: Span::new(0, 0),
                    seq: reg.seq,
                });
            } else {
                i += 1;
            }
        }
        drop(net);
        drop(done);
        shared.sched_cv.notify_all();
    }

    /// The M:N network poller (Wave 1, unix-only): one thread, no heap,
    /// no scheduler state beyond the net table + completions. It
    /// blocks in `poll()` on registered fds plus the self-pipe;
    /// readiness runs the nonblocking syscall INLINE with the net lock
    /// held (microsecond-scale, never blocks — so close+reuse cannot
    /// slip between verification and syscall), then the registration
    /// is removed and exactly one completion is posted. EAGAIN after
    /// readiness (consumed-by-race, vanishingly rare single-threaded)
    /// re-arms silently; anything else completes the wait.
    ///
    /// GC coordination: the poller never joins the STW rendezvous
    /// (only workers count) and never touches the heap — a stop
    /// happening mid-`poll` is invisible to it, and parked tasks'
    /// roots stay published through their slots.
    #[cfg(unix)]
    fn poller_loop(shared: &Arc<Shared>) {
        use toto_compiler::netsys as nn;
        let pipe_rd = shared.net_wake[0];
        loop {
            // Snapshot the poll set under one lock section. `ids`
            // carries the registration identity per pollfd so a
            // ready fd resolves to exactly its own wait — never a
            // different wait on a reused fd number (close+reuse
            // hands the number to a new socket while an old
            // completion is still in flight).
            let (fds, ids, shutdown) = {
                let net = shared.net.lock().expect("net lock");
                let mut fds: Vec<(std::os::fd::RawFd, bool, bool)> = net
                    .regs
                    .iter()
                    .map(|r| (r.fd, r.wants_read(), r.wants_write()))
                    .collect();
                let mut ids: Vec<Option<(TaskId, u64)>> =
                    net.regs.iter().map(|r| Some((r.task, r.seq))).collect();
                fds.push((pipe_rd, true, false));
                ids.push(None);
                (fds, ids, net.shutdown)
            };
            if shutdown {
                nn::close(pipe_rd).ok();
                return;
            }
            // Indefinite wait: every table change pokes the pipe, so
            // wakeups cannot be missed; the 60 s timeout only bounds
            // a hypothetical missed wakeup (fail-safe, not a timer).
            let ready = match nn::poll_wait(&fds, -1) {
                Ok(r) => r,
                Err(_) => {
                    // `poll` itself failed (fd table torn down under
                    // us is impossible — close paths hold this lock;
                    // an `EINTR`-class blip just retries).
                    continue;
                }
            };
            // Drain the pipe first: every table change (register,
            // unregister, shutdown) pokes it, so by the time we look
            // at the table no wakeup is missed. `poll` uses an
            // indefinite wait — the pipe IS the wakeup path.
            nn::drain_pipe(pipe_rd);
            {
                let net = shared.net.lock().expect("net lock");
                if net.shutdown {
                    drop(net);
                    // The write end is closed by the run owner after
                    // join; close our read end here. Any registered
                    // fds belong to dead runs — unreachable: shutdown
                    // happens after all tasks are done.
                    nn::close(pipe_rd).ok();
                    return;
                }
            }
            let mut woke = false;
            for &i in &ready {
                if fds[i].0 == pipe_rd {
                    continue;
                }
                // Peek, execute, then post-before-remove: close paths
                // wait for the net lock, so no close and reuse can slip
                // between verification and the nonblocking syscall.
                // The registration is removed only after its completion
                // is posted, so the deadlock check never observes a
                // parked wait with neither a registration nor its
                // completion (Wave 1 race fix). Each lock stands alone
                // (never nested): net for peek/execute, done for post,
                // net again for exact remove — so the poller can never
                // ABBA against the net -> done order of close/sweep.
                let (reg, result) = {
                    let net = shared.net.lock().expect("net lock");
                    let want = match ids[i] {
                        Some(w) => w,
                        // Pipe entry (already skipped above) or a
                        // snapshot inconsistency: safe drop.
                        None => continue,
                    };
                    let reg = match net
                        .regs
                        .iter()
                        .find(|r| r.task == want.0 && r.seq == want.1)
                        .cloned()
                    {
                        Some(reg) => reg,
                        // Close/sweep raced us: those paths post their
                        // own wakeups, so a miss here is a safe drop.
                        None => continue,
                    };
                    let result = poller_do(&reg);
                    (reg, result)
                };
                match result {
                    PollerDone::Rearm => {
                        // Spurious readiness (EAGAIN after a race): the
                        // registration was never removed (peek, never
                        // take), so there is nothing to restore.
                        // Close/sweep paths unregister + post on their
                        // own; the seq guard drops anything stray.
                    }
                    PollerDone::Done(result) => {
                        {
                            let mut done = shared.done.lock().expect("done lock");
                            done.push_back(Completion {
                                task: reg.task,
                                result,
                                span: reg.span,
                                seq: reg.seq,
                            });
                        }
                        woke = true;
                        // Remove exactly this wait *after* posting: the fd
                        // number may already be reused by a newer wait, so
                        // identity (task + seq) is the only safe key.
                        {
                            let mut net = shared.net.lock().expect("net lock");
                            if let Some(pos) = net
                                .regs
                                .iter()
                                .position(|r| r.task == reg.task && r.seq == reg.seq)
                            {
                                net.regs.remove(pos);
                            }
                        }
                    }
                }
            }
            if woke {
                shared.sched_cv.notify_all();
            }
        }
    }

    fn handle_req(&self, worker: usize, id: TaskId, req: ToSched) {
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
                let n = self.shared.sched.lock().expect("sched lock").tasks_spawned;
                let child_task = self.spawn_task(
                    worker,
                    format!("go-{n}"),
                    child,
                    Initial::GoBody {
                        body,
                        blocks,
                        captures,
                    },
                    span,
                );
                // ADR-009 spawn inheritance: the child starts with
                // the spawner's context slot. A pending detach
                // (`go_detached`) starts empty instead and consumes
                // the one-shot flag.
                {
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    let inherited = {
                        let parent = &mut sched.slots[id.0];
                        if parent.ctx.detach_once {
                            parent.ctx.detach_once = false;
                            CtxSlot::new()
                        } else {
                            let mut c = parent.ctx.clone();
                            c.detach_once = false;
                            c
                        }
                    };
                    sched.slots[child_task.0].ctx = inherited;
                }
                self.enqueue_on(worker, id, ToTask::Unit);
            }
            ToSched::SpawnNative {
                entry,
                ctx,
                span,
                scope,
            } => {
                let child = self
                    .shared
                    .scopes
                    .lock()
                    .expect("scopes lock")
                    .new_scope(scope);
                let n = self.shared.sched.lock().expect("sched lock").tasks_spawned;
                let child_task = self.spawn_task(
                    worker,
                    format!("go-{n}"),
                    child,
                    Initial::Native { entry, ctx },
                    span,
                );
                // Same inheritance as `Spawn` (Phase 4 JIT path).
                {
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    let inherited = {
                        let parent = &mut sched.slots[id.0];
                        if parent.ctx.detach_once {
                            parent.ctx.detach_once = false;
                            CtxSlot::new()
                        } else {
                            let mut c = parent.ctx.clone();
                            c.detach_once = false;
                            c
                        }
                    };
                    sched.slots[child_task.0].ctx = inherited;
                }
                self.enqueue_on(worker, id, ToTask::Unit);
            }
            ToSched::ChanNew { cap, elem, span } => {
                if cap.is_some_and(|n| n < 0) {
                    self.enqueue_on(
                        worker,
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
                self.enqueue_on(worker, id, ToTask::Value(RtVal::Chan(cid)));
            }
            ToSched::Send { chan, value, span } => {
                self.op_send(worker, id, chan, value, span);
                self.pump_selects(worker);
            }
            ToSched::Recv { chan, span } => {
                self.op_recv(worker, id, chan, span);
                self.pump_selects(worker);
            }
            ToSched::Select {
                cases,
                has_default,
                span,
            } => {
                self.op_select(worker, id, cases, has_default, span);
                self.pump_selects(worker);
            }
            ToSched::Close { chan, span } => {
                self.op_close(worker, id, chan, span);
                self.pump_selects(worker);
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
                self.enqueue_on(worker, id, ToTask::Value(RtVal::Bool(closed)));
            }
            ToSched::Blocking { op, span } => {
                self.op_blocking(worker, id, op, span);
            }
            #[cfg(unix)]
            ToSched::Net { op, span } => {
                self.op_net(worker, id, op, span);
            }
            ToSched::CtxCancel {
                scope,
                handle,
                span,
                trace,
            } => {
                // The ADR-009 `cancel_scope` hook: fail exactly the
                // named scope through the standard first-panic-wins
                // sweep. Parked tasks in its subtree release with
                // `Cancelled`; running tasks fail fast at their next
                // suspension point — the same path as a scope panic.
                let known = self
                    .shared
                    .scopes
                    .lock()
                    .expect("scopes lock")
                    .scope_count()
                    > scope.0;
                if !known {
                    self.enqueue_on(
                        worker,
                        id,
                        ToTask::OpPanic {
                            diag: Diag::new(
                                "E0210",
                                format!("invalid cancel handle {handle} (no such scope)"),
                                span,
                                None,
                            ),
                        },
                    );
                    return;
                }
                self.fail_scope(
                    worker,
                    scope,
                    Failure {
                        diag: Diag::new(
                            "E0214",
                            format!(
                                "context cancel: handle {handle} failed scope {} (tasks released by cancellation)",
                                scope.0
                            ),
                            span,
                            None,
                        ),
                        trace,
                    },
                );
                self.enqueue_on(worker, id, ToTask::Unit);
            }
        }
    }

    fn chan_missing(&self, worker: usize, id: TaskId, span: Span) {
        self.enqueue_on(
            worker,
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

    /// Is `task` still parked (not released by a scope failure)? Waiter
    /// lists are cleaned lazily: a released task keeps its stale entry
    /// until pairing pops it, so every pairing site must skip
    /// non-blocked waiters — otherwise a `Cancelled` response would be
    /// overwritten by a stale `Value`.
    /// Pairing precondition, checked with both locks held: the waiter
    /// is still parked, so waking it cannot overwrite a `Cancelled`
    /// response left by a scope-failure release. Waiter lists are
    /// cleaned lazily (stale entries stay until pairing pops them),
    /// which is safe only with this check at every pairing site.
    fn still_parked(sched: &SchedState, task: TaskId) -> bool {
        matches!(sched.slots[task.0].state, TaskState::Blocked { .. })
    }

    /// Pop the earliest waiter that is still parked, discarding stale
    /// entries left by scope-failure releases. Caller holds `sched`
    /// (and `state` for the channel). `keep` names the running task's
    /// own freshly registered entry (register-first pairing, see
    /// `op_send`): it is `Running`, never live, and must stay
    /// registered — entries are removed selectively so `keep` survives
    /// the scan in place.
    fn pop_live_recv(
        sched: &SchedState,
        ch: &mut Channel,
        keep: Option<TaskId>,
    ) -> Option<RecvWaiter> {
        let mut i = 0;
        while i < ch.recv_waiters.len() {
            if Some(ch.recv_waiters[i].task) == keep {
                i += 1;
                continue;
            }
            if Self::still_parked(sched, ch.recv_waiters[i].task) {
                return ch.recv_waiters.remove(i);
            }
            ch.recv_waiters.remove(i);
        }
        None
    }

    /// Pop the earliest sender that is still parked. See `pop_live_recv`
    /// for the `keep` discipline.
    fn pop_live_send(
        sched: &SchedState,
        ch: &mut Channel,
        keep: Option<TaskId>,
    ) -> Option<SendWaiter> {
        let mut i = 0;
        while i < ch.send_waiters.len() {
            if Some(ch.send_waiters[i].task) == keep {
                i += 1;
                continue;
            }
            if Self::still_parked(sched, ch.send_waiters[i].task) {
                return ch.send_waiters.remove(i);
            }
            ch.send_waiters.remove(i);
        }
        None
    }

    /// Send rendezvous (spec §13.1): pair with a parked receiver, buffer
    /// when bounded with space, else park the sender. Send on a closed
    /// channel panics (E0213).
    fn op_send(&self, worker: usize, id: TaskId, chan: ChannelId, value: RtVal, span: Span) {
        // Pairing decision under `scopes` then `sched` then `state`
        // (`scopes`-first matches `fail_scope`; the sanctioned order
        // for the rest): waiter liveness and list removal are atomic,
        // so a scope-failure release racing this send cannot lose a
        // `Cancelled` response. Wakes happen after all locks drop.
        //
        // PARK-FUSED REGISTER-FIRST (M:N race fixes): the decision
        // hold registers the task, scans, AND parks (on the Park path)
        // atomically. Two splits are fatal under M:N and both are
        // closed here: (1) check-then-register lets two complementary
        // tasks both observe an empty list and both park; with
        // registration inside the hold, holders serialize and whoever
        // arrives second always observes the first. (2) register-then-
        // park lets a pairing land between registration and parking and
        // its wake is dropped (`enqueue_if_blocked` only wakes
        // `Blocked` tasks); with parking inside the hold, a visible
        // entry always belongs to a `Blocked` task, so a popped peer
        // is always woken. The own entry is skipped by the scan
        // (`keep`); pairing or buffering removes it again, parking
        // leaves it registered.
        //
        // The hold also fail-fasts a scope that died between the
        // executor's pre-yield check and this op: parking in a dead
        // scope would strand the task (the release sweep already ran).
        enum Decision {
            Cancelled,
            Closed,
            Pair { task: TaskId, value: RtVal },
            Buffered,
            Park,
        }
        let decision = {
            let scopes = self.shared.scopes.lock().expect("scopes lock");
            let mut sched = self.shared.sched.lock().expect("sched lock");
            if scopes.is_failed_subtree(sched.slots[id.0].scope) {
                Decision::Cancelled
            } else {
                let mut st = self.shared.state.lock().expect("shared state lock");
                let Some(ch) = st.channels.get_mut(chan) else {
                    drop(st);
                    drop(sched);
                    drop(scopes);
                    self.chan_missing(worker, id, span);
                    return;
                };
                if ch.closed {
                    Decision::Closed
                } else {
                    ch.send_waiters.push_back(SendWaiter {
                        task: id,
                        value,
                        span,
                    });
                    if let Some(w) = Self::pop_live_recv(&sched, ch, Some(id)) {
                        let mine = ch.send_waiters.pop_back().expect("own send entry present");
                        debug_assert_eq!(mine.task, id);
                        Decision::Pair {
                            task: w.task,
                            value: mine.value,
                        }
                    } else if ch.buf.len() < ch.cap {
                        let mine = ch.send_waiters.pop_back().expect("own send entry present");
                        debug_assert_eq!(mine.task, id);
                        ch.buf.push_back(mine.value);
                        Decision::Buffered
                    } else {
                        sched.slots[id.0].state = TaskState::Blocked { span, op: "send" };
                        Decision::Park
                    }
                }
            }
        };
        match decision {
            Decision::Closed => self.enqueue_on(
                worker,
                id,
                ToTask::OpPanic {
                    diag: Diag::new("E0213", "send on a closed channel".to_string(), span, None),
                },
            ),
            Decision::Pair { task, value } => {
                self.enqueue_if_blocked(worker, task, ToTask::Value(value));
                self.enqueue_on(worker, id, ToTask::Unit);
            }
            Decision::Buffered => self.enqueue_on(worker, id, ToTask::Unit),
            // Already parked inside the decision hold; nothing to do.
            Decision::Park => {}
            Decision::Cancelled => self.enqueue_on(worker, id, ToTask::Cancelled { op: "send" }),
        }
    }

    /// Receive (spec §13.1): buffered items first, then a parked sender,
    /// else park — unless closed-and-drained, which yields the zero value.
    /// Same park-fused register-first discipline as `op_send`: a
    /// visible entry always belongs to a `Blocked` task, and a scope
    /// that died before this op fail-fasts instead of parking.
    fn op_recv(&self, worker: usize, id: TaskId, chan: ChannelId, span: Span) {
        enum Out {
            Value(RtVal),
            Park,
            NoZero,
            Cancelled,
        }
        let mut out = Out::Park;
        let mut woken: Option<TaskId> = None;
        {
            let scopes = self.shared.scopes.lock().expect("scopes lock");
            let mut sched = self.shared.sched.lock().expect("sched lock");
            if scopes.is_failed_subtree(sched.slots[id.0].scope) {
                out = Out::Cancelled;
            } else {
                let mut st = self.shared.state.lock().expect("shared state lock");
                let Some(ch) = st.channels.get_mut(chan) else {
                    drop(st);
                    drop(sched);
                    drop(scopes);
                    self.chan_missing(worker, id, span);
                    return;
                };
                ch.recv_waiters.push_back(RecvWaiter { task: id, span });
                // Un-register the own entry on every non-parking path
                // (it is the back: pushed above, same hold, and the
                // scan below skips it via `keep`).
                let unregister = |ch: &mut Channel| {
                    let mine = ch.recv_waiters.pop_back().expect("own recv entry present");
                    debug_assert_eq!(mine.task, id);
                };
                if let Some(v) = ch.buf.pop_front() {
                    // Freeing one buffer slot admits the earliest sender.
                    if let Some(w) = Self::pop_live_send(&sched, ch, Some(id)) {
                        ch.buf.push_back(w.value);
                        woken = Some(w.task);
                    }
                    unregister(ch);
                    out = Out::Value(v);
                } else if let Some(w) = Self::pop_live_send(&sched, ch, Some(id)) {
                    // Unbuffered rendezvous (or bounded-drained): take it.
                    unregister(ch);
                    woken = Some(w.task);
                    out = Out::Value(w.value);
                } else if ch.closed {
                    // Closed-and-drained: the zero value of `T`.
                    unregister(ch);
                    let elem = ch.elem.clone();
                    let structs = self.code.structs.clone();
                    let z = zero_value(&elem, &mut st.heap, &structs);
                    match z {
                        Some(v) => out = Out::Value(v),
                        None => out = Out::NoZero,
                    }
                } else {
                    // Park inside the hold (see `op_send`).
                    sched.slots[id.0].state = TaskState::Blocked { span, op: "recv" };
                }
            }
        }
        if let Some(wid) = woken {
            self.enqueue_if_blocked(worker, wid, ToTask::Unit);
        }
        match out {
            Out::Value(v) => self.enqueue_on(worker, id, ToTask::Value(v)),
            // Already parked inside the decision hold; nothing to do.
            Out::Park => {}
            Out::Cancelled => {
                self.enqueue_on(worker, id, ToTask::Cancelled { op: "recv" })
            }
            Out::NoZero => self.enqueue_on(
                worker,
                id,
                ToTask::OpPanic {
                    diag: Diag::new(
                        "E0204",
                        "internal error: zero value of a channel/fn element (unreachable: close is E0315 there)".to_string(),
                        span,
                        None,
                    ),
                },
            ),
        }
    }

    /// `select` (spec §13.3): the first ready case runs; random among
    /// simultaneously ready (seeded RNG); `default` never blocks. A send
    /// guard on a closed channel counts as ready and panics when chosen
    /// (matching Go: the send can "proceed" only by panicking).
    fn op_select(
        &self,
        worker: usize,
        id: TaskId,
        cases: Vec<SelReq>,
        has_default: bool,
        span: Span,
    ) {
        let pick = {
            let st = self.shared.state.lock().expect("shared state lock");
            let mut rng = self.shared.rng.lock().expect("rng lock");
            Self::choose_case(&st, &cases, has_default, &mut rng)
        };
        match pick {
            SelectPick::Case(i) => self.perform_case(worker, id, cases, i, true),
            SelectPick::Default => {
                self.enqueue_on(
                    worker,
                    id,
                    ToTask::Selected {
                        index: cases.len(),
                        value: None,
                    },
                );
            }
            SelectPick::Block => {
                // Register + park in ONE hold (see `op_send`): a channel
                // op racing this registration either pairs before it
                // (then the trailing `pump_selects` below finds nothing
                // to do) or lands after it (then the trailing pump
                // performs this select). No split between registration
                // and parking means no lost wakeup in either order.
                // A scope that died before this op fail-fasts instead
                // of parking (see `op_send`).
                let cancelled = {
                    let scopes = self.shared.scopes.lock().expect("scopes lock");
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    if scopes.is_failed_subtree(sched.slots[id.0].scope) {
                        true
                    } else {
                        let mut st = self.shared.state.lock().expect("shared state lock");
                        let seq = sched.park_seq;
                        sched.park_seq += 1;
                        st.select_waiters.push_back(SelectWaiter {
                            task: id,
                            cases,
                            span,
                            seq,
                        });
                        sched.slots[id.0].state = TaskState::Blocked { span, op: "select" };
                        false
                    }
                };
                if cancelled {
                    self.enqueue_on(worker, id, ToTask::Cancelled { op: "select" });
                }
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
    fn perform_case(
        &self,
        worker: usize,
        id: TaskId,
        mut cases: Vec<SelReq>,
        i: usize,
        direct: bool,
    ) {
        // `direct` (from `op_select`) performs the running task's own
        // request: it cannot have been released (releases hit only
        // `Blocked` tasks), so delivery is unconditional. From `pump`
        // the waiter may have been released or woken between scan and
        // performance: delivery is conditional (see `enqueue_if_blocked`).
        match cases.remove(i) {
            SelReq::Recv { chan, span } => {
                self.op_recv_inner(worker, id, chan, span, i, direct);
            }
            SelReq::Send { chan, value, span } => {
                self.op_send_inner(worker, id, chan, value, span, i, direct);
            }
        }
    }

    /// Receive for a selected case: like `op_recv` but responds
    /// `Selected` instead of `Value`. Readiness held, so this never
    /// parks (a lost race re-blocks instead of panicking — the task
    /// simply waits for the next state change).
    fn op_recv_inner(
        &self,
        worker: usize,
        id: TaskId,
        chan: ChannelId,
        span: Span,
        index: usize,
        direct: bool,
    ) {
        enum Out {
            Selected(RtVal),
            Repark,
        }
        let mut out = Out::Repark;
        let mut woken: Option<TaskId> = None;
        {
            let sched = self.shared.sched.lock().expect("sched lock");
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                drop(sched);
                self.chan_missing(worker, id, span);
                return;
            };
            if let Some(v) = ch.buf.pop_front() {
                if let Some(w) = Self::pop_live_send(&sched, ch, None) {
                    ch.buf.push_back(w.value);
                    woken = Some(w.task);
                }
                out = Out::Selected(v);
            } else if let Some(w) = Self::pop_live_send(&sched, ch, None) {
                woken = Some(w.task);
                out = Out::Selected(w.value);
            } else if ch.closed {
                let elem = ch.elem.clone();
                let structs = self.code.structs.clone();
                match zero_value(&elem, &mut st.heap, &structs) {
                    Some(v) => out = Out::Selected(v),
                    None => {
                        drop(st);
                        self.enqueue_on(worker,
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
            self.enqueue_if_blocked(worker, wid, ToTask::Unit);
        }
        match out {
            Out::Selected(v) => {
                let resp = ToTask::Selected {
                    index,
                    value: Some(v),
                };
                // Direct performs the running task's own select:
                // unconditional. Pumped performs are conditional.
                if direct {
                    self.enqueue_on(worker, id, resp);
                } else {
                    self.enqueue_if_blocked(worker, id, resp);
                }
            }
            Out::Repark => {
                // Lost a race between readiness and performance (only
                // under M:N; single-threaded this is unreachable). Park
                // on this channel and wait for the next change.
                // Register + park in ONE hold: a pairing racing this
                // re-park either lands first (the task is no longer
                // parked, so the response stands and nothing registers)
                // or scans after it (and observes the new entry). A
                // split would let a response land between registration
                // and parking, and `park` would overwrite `Ready` with
                // `Blocked` — stranding the task with its queue entry
                // spinning in the defensive-requeue loop.
                //
                // `direct` (own select) runs on the `Running` task,
                // which cannot have been released (releases hit only
                // `Blocked` tasks): park unconditionally — returning
                // early here would strand a `Running` task with no queue
                // entry — but fail-fast a scope that died meanwhile.
                // Pumped performs check `still_parked` first: a released
                // waiter keeps its `Cancelled` response.
                enum Repark {
                    Park,
                    Drop,
                    Cancelled,
                    Missing,
                }
                let what = {
                    let scopes = self.shared.scopes.lock().expect("scopes lock");
                    let mut sched = self.shared.sched.lock().expect("sched lock");
                    if !direct && !Self::still_parked(&sched, id) {
                        Repark::Drop
                    } else if direct && scopes.is_failed_subtree(sched.slots[id.0].scope) {
                        Repark::Cancelled
                    } else {
                        let mut st = self.shared.state.lock().expect("shared state lock");
                        match st.channels.get_mut(chan) {
                            Some(ch) => {
                                ch.recv_waiters.push_back(RecvWaiter { task: id, span });
                                sched.slots[id.0].state = TaskState::Blocked { span, op: "recv" };
                                Repark::Park
                            }
                            // Unreachable: channels are never removed.
                            // Error rather than strand the task.
                            None => Repark::Missing,
                        }
                    }
                };
                match what {
                    Repark::Park | Repark::Drop => {}
                    Repark::Cancelled => {
                        self.enqueue_on(worker, id, ToTask::Cancelled { op: "recv" })
                    }
                    Repark::Missing => self.chan_missing(worker, id, span),
                }
            }
        }
    }

    /// Send for a selected case: like `op_send` but responds `Selected`.
    /// A closed channel panics (E0213) when chosen.
    #[allow(clippy::too_many_arguments)]
    fn op_send_inner(
        &self,
        worker: usize,
        id: TaskId,
        chan: ChannelId,
        value: RtVal,
        span: Span,
        index: usize,
        direct: bool,
    ) {
        let mut wake_recv: Option<(TaskId, RtVal)> = None;
        let mut parked = false;
        let mut panicked = false;
        // The send value rides outside the decision hold until its fate
        // is known, so the parking path below can register it in the
        // same hold that parks (register-first, no stale entry).
        let mut send_value = Some(value);
        {
            let sched = self.shared.sched.lock().expect("sched lock");
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                drop(sched);
                self.chan_missing(worker, id, span);
                return;
            };
            if ch.closed {
                panicked = true;
            } else if let Some(w) = Self::pop_live_recv(&sched, ch, None) {
                let v = send_value.take().expect("send value present");
                wake_recv = Some((w.task, v));
            } else if ch.buf.len() < ch.cap {
                let v = send_value.take().expect("send value present");
                ch.buf.push_back(v);
            } else {
                parked = true;
            }
        }
        // All select responses route through one conditional point:
        // direct performs the running task's own select (unconditional
        // — it cannot have been released); pumped performs are
        // conditional on the waiter still being parked.
        let deliver_self = |resp| {
            if direct {
                self.enqueue_on(worker, id, resp);
            } else {
                self.enqueue_if_blocked(worker, id, resp);
            }
        };
        if panicked {
            deliver_self(ToTask::OpPanic {
                diag: Diag::new("E0213", "send on a closed channel".to_string(), span, None),
            });
        } else if let Some((rid, v)) = wake_recv {
            self.enqueue_if_blocked(worker, rid, ToTask::Value(v));
            deliver_self(ToTask::Selected { index, value: None });
        } else if parked {
            // Lost a race (M:N only); wait on this channel. Register +
            // park in ONE hold (see Repark above): `direct` parks
            // unconditionally (the `Running` task cannot have been
            // released) but fail-fasts a scope that died meanwhile;
            // pumped checks `still_parked` so a released waiter keeps
            // its `Cancelled` response. A missing channel (unreachable:
            // channels are never removed) errors rather than strands.
            enum Parked {
                Park,
                Drop,
                Cancelled,
                Missing,
            }
            let what = {
                let scopes = self.shared.scopes.lock().expect("scopes lock");
                let mut sched = self.shared.sched.lock().expect("sched lock");
                if !direct && !Self::still_parked(&sched, id) {
                    Parked::Drop
                } else if direct && scopes.is_failed_subtree(sched.slots[id.0].scope) {
                    Parked::Cancelled
                } else {
                    let mut st = self.shared.state.lock().expect("shared state lock");
                    match st.channels.get_mut(chan) {
                        Some(ch) => {
                            let value = send_value.take().expect("send value present");
                            ch.send_waiters.push_back(SendWaiter {
                                task: id,
                                value,
                                span,
                            });
                            sched.slots[id.0].state = TaskState::Blocked { span, op: "send" };
                            Parked::Park
                        }
                        None => Parked::Missing,
                    }
                }
            };
            match what {
                Parked::Park | Parked::Drop => {}
                Parked::Cancelled => self.enqueue_on(worker, id, ToTask::Cancelled { op: "send" }),
                Parked::Missing => self.chan_missing(worker, id, span),
            }
        } else {
            deliver_self(ToTask::Selected { index, value: None });
        }
    }

    /// Re-evaluate every parked select after a channel state change.
    /// Ready selects perform their choice now (deterministic order:
    /// waiter FIFO, RNG for case ties).
    fn pump_selects(&self, worker: usize) {
        loop {
            // Find the first ready waiter, then remove and perform it.
            // Each pump pass re-scans: performing one select can change
            // readiness for the rest. Released waiters are stale
            // (their `Cancelled` stands): drop them instead of
            // performing or re-parking, so no select entry — or the
            // send value it holds — leaks.
            let waiter = {
                let sched = self.shared.sched.lock().expect("sched lock");
                let mut st = self.shared.state.lock().expect("shared state lock");
                let pos = st.select_waiters.iter().position(|w| {
                    Self::still_parked(&sched, w.task)
                        && !Self::ready_cases(&st, &w.cases).is_empty()
                });
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
                SelectPick::Case(i) => {
                    self.perform_case(worker, waiter.task, waiter.cases, i, false)
                }
                // Parked selects never carry a default; a lost race
                // (M:N only) re-parks at the back — unless released
                // meanwhile, in which case the waiter is dropped.
                SelectPick::Default | SelectPick::Block => {
                    let released = {
                        let sched = self.shared.sched.lock().expect("sched lock");
                        !Self::still_parked(&sched, waiter.task)
                    };
                    if !released {
                        let mut st = self.shared.state.lock().expect("shared state lock");
                        st.select_waiters.push_back(waiter);
                    }
                }
            }
        }
    }

    /// `close(ch)` (spec §13.1): mark closed, fail parked senders
    /// (E0213 at their spans), drain buffered items to parked receivers,
    /// then zero values for the rest. Double-close panics (E0213).
    fn op_close(&self, worker: usize, id: TaskId, chan: ChannelId, span: Span) {
        struct Release {
            task: TaskId,
            resp: ToTask,
        }
        let mut releases: Vec<Release> = Vec::new();
        let mut result: ToTask = ToTask::Unit;
        {
            let sched = self.shared.sched.lock().expect("sched lock");
            let mut st = self.shared.state.lock().expect("shared state lock");
            let Some(ch) = st.channels.get_mut(chan) else {
                drop(st);
                drop(sched);
                self.chan_missing(worker, id, span);
                return;
            };
            if ch.closed {
                result = ToTask::OpPanic {
                    diag: Diag::new("E0213", "close of a closed channel".to_string(), span, None),
                };
            } else {
                ch.closed = true;
                // Stale sender entries (released by a scope failure)
                // keep their `Cancelled`: skip them here.
                for w in ch.send_waiters.drain(..) {
                    if !Self::still_parked(&sched, w.task) {
                        continue;
                    }
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
                while let Some(w) = Self::pop_live_recv(&sched, ch, None) {
                    if let Some(v) = ch.buf.pop_front() {
                        if let Some(sw) = Self::pop_live_send(&sched, ch, None) {
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
            // Conditional: a waiter released between drain and delivery
            // keeps its `Cancelled` (the close response stands dropped).
            self.enqueue_if_blocked(worker, r.task, r.resp);
        }
        self.enqueue_on(worker, id, result);
    }

    fn task_done(&self, worker: usize, id: TaskId, res: TaskResult) {
        // High-water: the stack is dead; scan for the pattern reversal.
        // The coroutine object is reaped (mapping freed promptly); the
        // slot stays for census. All under the `sched` lock; scope
        // work after it drops.
        let scope = {
            let mut sched = self.shared.sched.lock().expect("sched lock");
            let hw = {
                let slot = &sched.slots[id.0];
                unsafe { super::measure::scan_high_water(slot.writable_start, slot.stack_base) }
            };
            {
                let slot = &mut sched.slots[id.0];
                slot.high_water = Some(hw);
                slot.co = None;
                slot.state = TaskState::Done;
                sched.live -= 1;
                // Open a settle: until the cascade below enqueues its
                // wakeups, this completion is invisible to the deadlock
                // check (closed by `settle_done` on every path).
                sched.settling += 1;
            }
            sched.high_water_max = sched.high_water_max.max(hw);
            let scope = sched.slots[id.0].scope;
            let is_main = id == sched.main_task;
            match res {
                TaskResult::Panic { diag, trace } => {
                    if is_main {
                        sched.main_result = Some(TaskResult::Panic {
                            diag: diag.clone(),
                            trace: trace.clone(),
                        });
                    }
                    Some(Failure { diag, trace })
                }
                TaskResult::Return(v) => {
                    if is_main {
                        sched.main_result = Some(TaskResult::Return(v));
                    }
                    None
                }
            }
            .map(|f| (scope, f))
        };
        if let Some((scope, failure)) = scope {
            self.fail_scope(worker, scope, failure);
        } else {
            // Still run the settle cascade for clean completions.
            let scope = self.shared.sched.lock().expect("sched lock").slots[id.0].scope;
            self.on_task_done(worker, scope);
            self.settle_done();
            return;
        }
        let scope = self.shared.sched.lock().expect("sched lock").slots[id.0].scope;
        self.on_task_done(worker, scope);
        self.settle_done();
        // A finished task may unblock the world: wake sleepers so
        // termination/deadlock is re-evaluated promptly.
        self.shared.sched_cv.notify_all();
    }

    /// Close a settle opened in `task_done`: the cascade's wakeups are
    /// all enqueued, so the deadlock check may decide again. Short
    /// `sched`-lock section only (module lock discipline: no nesting).
    fn settle_done(&self) {
        let mut sched = self.shared.sched.lock().expect("sched lock");
        debug_assert!(sched.settling > 0, "settle_done without a settle");
        sched.settling -= 1;
    }

    fn on_task_done(&self, worker: usize, scope: ScopeId) {
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
                            Some(p) => self.fail_scope(worker, p, f),
                            None => {
                                let mut sched = self.shared.sched.lock().expect("sched lock");
                                if sched.root_failure.is_none() {
                                    sched.root_failure = Some(f);
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
    /// (deterministic). Collect-then-release: no enqueue while holding
    /// the `sched` lock.
    ///
    /// Release also sweeps the released tasks' waiter entries out of
    /// the select table and every channel list. Without the sweep the
    /// entries would linger: their send values would stay rooted
    /// forever, and a later pairing could consume a channel value on
    /// behalf of a dead wait.
    fn fail_scope(&self, worker: usize, scope: ScopeId, failure: Failure) {
        let newly = self
            .shared
            .scopes
            .lock()
            .expect("scopes lock")
            .fail_if_clean(scope, failure);
        if !newly {
            return;
        }
        let in_sub: Vec<(TaskId, &'static str)> = {
            let t = self.shared.scopes.lock().expect("scopes lock");
            let sched = self.shared.sched.lock().expect("sched lock");
            sched
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| match s.state {
                    TaskState::Blocked { op, .. } if t.in_subtree(s.scope, scope) => {
                        Some((TaskId(i), op))
                    }
                    _ => None,
                })
                .collect()
        };
        if !in_sub.is_empty() {
            let released: std::collections::HashSet<usize> =
                in_sub.iter().map(|(tid, _)| tid.0).collect();
            let mut st = self.shared.state.lock().expect("shared state lock");
            st.select_waiters.retain(|w| !released.contains(&w.task.0));
            st.channels.sweep_released(&released);
        }
        // Unix: unregister any parked net waits in the failed subtree.
        // Two phases, never nested (net→done matches the pool order;
        // sched is taken alone in between): snapshot candidates, check
        // scopes without holding net, then remove + post. Each
        // unregistered wait gets a `NetFail` completion — the sweep
        // never drops a wakeup.
        #[cfg(unix)]
        {
            let snap: Vec<(TaskId, u64)> = {
                let net = self.shared.net.lock().expect("net lock");
                net.regs.iter().map(|r| (r.task, r.seq)).collect()
            };
            let mut dead = Vec::new();
            {
                let t = self.shared.scopes.lock().expect("scopes lock");
                let sched = self.shared.sched.lock().expect("sched lock");
                for (task, seq) in &snap {
                    if t.in_subtree(sched.slots[task.0].scope, scope) {
                        dead.push((*task, *seq));
                    }
                }
            }
            if !dead.is_empty() {
                let mut net = self.shared.net.lock().expect("net lock");
                let mut done = self.shared.done.lock().expect("done lock");
                net.regs.retain(|r| {
                    if dead.contains(&(r.task, r.seq)) {
                        done.push_back(Completion {
                            task: r.task,
                            result: BlockingResult::NetFail("scope cancelled".to_string()),
                            span: Span::new(0, 0),
                            seq: r.seq,
                        });
                        false
                    } else {
                        true
                    }
                });
            }
        }
        for (tid, op) in in_sub {
            // Conditional: a pairing that already woke the task (its
            // `Value` stands) is not overwritten. Either way the task
            // runs next in a failed scope and fail-fasts at its next
            // suspension.
            self.enqueue_if_blocked(worker, tid, ToTask::Cancelled { op });
        }
    }

    /// Park a task on a network wait (Wave 1, unix-only): mark
    /// `Blocked` on `"net"` with a fresh `net_seq`, register the fd
    /// with the poller (assigning its close+reuse generation), and
    /// wake the poller via the self-pipe. Same atomicity discipline
    /// as `op_blocking` (one `sched` section, net table nested
    /// inside): a task observed as `Blocked` on `"net"` always has
    /// its registration submitted.
    #[cfg(unix)]
    fn op_net(&self, worker: usize, id: TaskId, op: NetOp, span: Span) {
        // No fail-fast check here: like `park`, the executor checks the
        // scope BEFORE yielding, so a task that reaches this request is
        // live. (A scope that fails while the wait is registered
        // releases the task with `Cancelled`; the sweep in
        // `fail_scope` unregisters it and the stale completion is
        // dropped by its `seq` — see `drain_completions_locked`.)
        let _ = worker;
        let fd = match op {
            NetOp::Connect { fd }
            | NetOp::Accept { fd }
            | NetOp::Recv { fd, .. }
            | NetOp::Send { fd, .. }
            | NetOp::UdpRecv { fd, .. }
            | NetOp::UdpSend { fd, .. } => fd,
        };
        {
            let mut sched = self.shared.sched.lock().expect("sched lock");
            let slot = &mut sched.slots[id.0];
            slot.net_seq += 1;
            let seq = slot.net_seq;
            slot.state = TaskState::Blocked { span, op: "net" };
            let mut net = self.shared.net.lock().expect("net lock");
            net.regs.push(NetReg {
                fd,
                task: id,
                seq,
                op,
                span,
            });
        }
        toto_compiler::netsys::poke_pipe(self.shared.net_wake[1]);
    }

    fn op_blocking(&self, worker: usize, id: TaskId, op: BlockingOp, span: Span) {
        // No fail-fast check here: like `park`, the executor checks the
        // scope BEFORE yielding, so a task that reaches this request is
        // live. (A scope that fails while the job is in flight releases
        // the task with `Cancelled`; the stale completion is dropped by
        // its `seq` — see `drain_completions`.)
        let _ = worker;
        // Park and submit atomically (one `sched` section, pool nested
        // inside per the lock order): a task observed as `Blocked` on
        // `blocking` always has its job submitted, so the deadlock
        // check can never see a parked task with an idle-looking pool.
        {
            let mut sched = self.shared.sched.lock().expect("sched lock");
            let slot = &mut sched.slots[id.0];
            slot.blocking_seq += 1;
            let seq = slot.blocking_seq;
            slot.state = TaskState::Blocked {
                span,
                op: "blocking",
            };
            let mut pool = self.shared.pool.lock().expect("pool lock");
            pool.jobs.push_back(PoolJob {
                task: id,
                op,
                span,
                seq,
            });
        }
        self.shared.pool_cv.notify_one();
    }
}

enum SelectPick {
    Case(usize),
    Default,
    Block,
}
