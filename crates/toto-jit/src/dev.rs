//! `toto dev` machinery (Phase 4, Stage 2): file watching, the
//! reloadability-matrix classifier, and drain-and-restart generations.
//!
//! Model: one `Jit` module per generation, whole-module retirement
//! (a generation's module lives exactly as long as its run — no
//! cross-generation references exist, so the Spike 2 lifetime rule
//! holds structurally). Rebuilds are strictly sequential
//! (drain-then-restart, never overlapped), so old and new code can
//! never mix. No state is preserved across reloads (gated Phase 6);
//! Toto programs hold no persistent external handles (file builtins
//! are one-shot), so respawn starts clean by construction and the
//! reconnect-hooks row is vacuous (documented, not built).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use toto_compiler::ast::Program;
use toto_compiler::hir::CheckedProgram;
use toto_compiler::mir::{MBlock, MInstr, MProgram};
use toto_compiler::{parse_and_check, render_panic};
use toto_runtime::heap::{GcRef, Heap};
use toto_runtime::sched::{ClosureModules, Shared};
use toto_runtime::value::RtVal;
use toto_runtime::{RtOutcome, RuntimeConfig};

use crate::compile::{Jit, OptLevel};
use crate::preserve::{PreservingSeed, plan_carry};
use crate::task::{JitRun, JitSession, spawn_jit_run};

/// What changed between two checked revisions (reloadability matrix).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    /// No semantic change (identical MIR + struct layouts).
    NoChange,
    /// Only function bodies changed: drain (graceful) + respawn.
    BodyOnly,
    /// Signatures, struct layouts, or channel types changed: cancel
    /// promptly + respawn (restart path).
    Restart,
}

/// Classify old vs new checked programs for the reload matrix.
/// Comparison is structural over MIR (spans included: a shifted span
/// conservatively counts as changed — always safe, occasionally
/// rebuild-happy). Heuristics are documented per rule below; any
/// misclassification stays correct (both paths isolate generations
/// fully — they differ only in gracefulness).
pub fn classify_change(old: &CheckedProgram, new: &CheckedProgram) -> ChangeKind {
    let old_mir = toto_compiler::mir::lower(old);
    let new_mir = toto_compiler::mir::lower(new);
    // Struct layouts first (never silently reinterpreted: any
    // declaration change restarts; migration is gated Phase 6).
    if old.structs != new.structs {
        return ChangeKind::Restart;
    }
    // Top-level functions by name (added/removed/renamed functions
    // change the API surface: conservative restart).
    let mut new_by_name: HashMap<&str, &toto_compiler::mir::MFn> = HashMap::new();
    for f in &new_mir.fns {
        new_by_name.insert(f.name.as_str(), f);
    }
    if old_mir.fns.len() != new_mir.fns.len() {
        return ChangeKind::Restart;
    }
    let mut body_changed = false;
    for old_f in &old_mir.fns {
        let new_f = match new_by_name.get(old_f.name.as_str()) {
            Some(f) => *f,
            None => return ChangeKind::Restart,
        };
        if old_f.params != new_f.params || old_f.ret != new_f.ret {
            return ChangeKind::Restart;
        }
        if old_f.locals != new_f.locals
            || old_f.blocks != new_f.blocks
            || old_f.entry != new_f.entry
        {
            body_changed = true;
        }
    }
    // Channel element types (multiset per program — documented
    // heuristic: a moved type with an unchanged multiset
    // misclassifies as body-only, which stays correct, only less
    // prompt).
    if chan_types(&old_mir) != chan_types(&new_mir) {
        return ChangeKind::Restart;
    }
    // Global initializers: vacuous (Toto has no globals).
    if body_changed {
        ChangeKind::BodyOnly
    } else {
        ChangeKind::NoChange
    }
}

/// Sorted channel element types across a program (multiset).
fn chan_types(prog: &MProgram) -> Vec<toto_compiler::hir::Ty> {
    fn walk_body(out: &mut Vec<toto_compiler::hir::Ty>, blocks: &[MBlock]) {
        for b in blocks {
            for i in &b.instrs {
                match i {
                    MInstr::ChanNew { elem, .. } => out.push(elem.clone()),
                    MInstr::MakeClosure { body, .. } | MInstr::Spawn { body, .. } => {
                        walk_body(out, &body.blocks)
                    }
                    _ => {}
                }
            }
        }
    }
    let mut out = Vec::new();
    for f in &prog.fns {
        walk_body(&mut out, &f.blocks);
    }
    out.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    out
}

/// Drain policy: quiescence timeout (ratified default 5 s,
/// configurable) + cancel grace before the restart fallback.
pub struct DrainPolicy {
    pub drain_timeout: Duration,
    pub cancel_grace: Duration,
}

impl Default for DrainPolicy {
    fn default() -> Self {
        Self {
            drain_timeout: Duration::from_secs(5),
            cancel_grace: Duration::from_secs(5),
        }
    }
}

/// Full-restart fallback (the dev process restarts itself when a run
/// outlives cancel+grace — e.g. a never-suspending task wedged on one
/// worker): injectable for tests.
pub trait RestartPolicy {
    fn restart(&self, reason: &str);
}

/// Production fallback: re-exec the current binary with identical
/// arguments. Exits 0 on a clean handoff, 1 if the re-exec itself
/// fails to spawn.
pub struct ReexecRestart;

impl RestartPolicy for ReexecRestart {
    fn restart(&self, reason: &str) {
        eprintln!("toto dev: {reason}; restarting the dev process");
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("toto"));
        let args: Vec<String> = std::env::args().skip(1).collect();
        match std::process::Command::new(exe).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                eprintln!("toto dev: restart failed to spawn: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Test-only restart policy: records the restart reason instead of
/// re-executing the process. `ReexecRestart` re-spawns the current
/// binary with identical arguments — under `cargo test` that
/// re-runs the whole test binary (which re-triggers the abandonment,
/// an unbounded re-exec chain holding the harness pipe open), so no
/// test may use it on a path that can abandon a run.
#[cfg(test)]
#[derive(Clone, Default, Debug)]
pub(super) struct RecordRestart {
    fired: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
impl RestartPolicy for RecordRestart {
    fn restart(&self, reason: &str) {
        self.fired
            .lock()
            .expect("restart record lock")
            .push(reason.to_string());
    }
}

#[cfg(test)]
impl RecordRestart {
    pub(super) fn fired(&self) -> Vec<String> {
        self.fired.lock().expect("restart record lock").clone()
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// File watcher: content polling with change gating (std only — no
/// platform watcher dependency). Reads the file each tick and yields
/// new source bytes only when the content actually changed
/// (untouched saves are skipped without a rebuild).
pub struct PollWatcher {
    path: PathBuf,
    interval: Duration,
    last_hash: u64,
}

impl PollWatcher {
    pub fn new(path: PathBuf, interval: Duration) -> std::io::Result<Self> {
        let bytes = std::fs::read(&path)?;
        Ok(Self {
            path,
            interval,
            last_hash: hash_bytes(&bytes),
        })
    }

    /// Block until the content changes or `stop` fires; returns the
    /// new bytes (`None` = stop fired first). Transient IO errors
    /// (editor mid-save) retry on the next tick.
    pub fn wait_change(&mut self, stop: &AtomicBool) -> Option<Vec<u8>> {
        loop {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            match self.poll_once() {
                Ok(Some(bytes)) => return Some(bytes),
                Ok(None) => {}
                Err(_) => {}
            }
            std::thread::sleep(self.interval);
        }
    }

    /// Non-blocking single check: fresh bytes when the content
    /// changed since the last observation (test seam).
    pub fn poll_once(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let bytes = std::fs::read(&self.path)?;
        let h = hash_bytes(&bytes);
        if h == self.last_hash {
            return Ok(None);
        }
        self.last_hash = h;
        Ok(Some(bytes))
    }
}

/// A live generation run. The production handle is [`JitRun`]; tests
/// inject stubs to decide the drain/cancel/restart path
/// deterministically (no threads, no timing).
pub trait LiveRun {
    /// True once the run's thread finished (join would return).
    fn is_finished(&self) -> bool;
    /// Request cooperative cancellation (scope-failure release).
    fn cancel(&self);
    /// Wait for the thread and take the outcome.
    fn join(self: Box<Self>) -> RtOutcome;
}

impl LiveRun for JitRun {
    fn is_finished(&self) -> bool {
        JitRun::is_finished(self)
    }

    fn cancel(&self) {
        JitRun::cancel(self);
    }

    fn join(self: Box<Self>) -> RtOutcome {
        (*self).join()
    }
}

/// One compiled generation: the checked inputs, the source they
/// render against, and the JIT state, held together so a generation's
/// module provably outlives its run. Spawned runs borrow nothing from
/// the generation after `spawn` returns.
pub struct Generation {
    /// Monotonic generation number (1-based; strictly sequential).
    pub number: u64,
    /// Source text the run renders diagnostics against.
    pub src: String,
    /// Filename shown in rendered diagnostics.
    pub filename: String,
    /// Checked program (also the next rebuild's classify baseline).
    pub program: Program,
    /// Checked form (also the next rebuild's classify baseline).
    pub checked: CheckedProgram,
    /// Held, never read: owns the generation's JIT module so the
    /// compiled code provably outlives the run.
    #[allow(dead_code)]
    jit: Jit,
    session: JitSession,
    /// True when compiled without the state-init prepend (preserving
    /// reload): `spawn` requires `Some(seed)` exactly then.
    without_init: bool,
}

impl Generation {
    /// Lower and compile one generation. Fails only on internal
    /// codegen refusal, which cannot happen for checked programs
    /// (kept `Result` for totality; the driver surfaces it and keeps
    /// the old run).
    pub fn compile(
        number: u64,
        src: String,
        filename: String,
        program: Program,
        checked: CheckedProgram,
        opt: OptLevel,
    ) -> Result<Self, String> {
        Self::compile_inner(number, src, filename, program, checked, opt, false)
    }

    /// Lower and compile for a preserving reload (Phase 6, Stage 3):
    /// `main` is compiled WITHOUT the state-init prepend — the
    /// driver seeds every slot from carried values instead. Paired
    /// with `spawn` carrying `Some(seed)`; the pairing is checked
    /// loudly in `spawn` (a mismatch is a driver bug, never silent).
    pub fn compile_preserving(
        number: u64,
        src: String,
        filename: String,
        program: Program,
        checked: CheckedProgram,
        opt: OptLevel,
    ) -> Result<Self, String> {
        Self::compile_inner(number, src, filename, program, checked, opt, true)
    }

    fn compile_inner(
        number: u64,
        src: String,
        filename: String,
        program: Program,
        checked: CheckedProgram,
        opt: OptLevel,
        without_init: bool,
    ) -> Result<Self, String> {
        let mir = if without_init {
            toto_compiler::mir::lower_without_init(&checked)
        } else {
            toto_compiler::mir::lower(&checked)
        };
        let mut jit = Jit::new_task_with_opt(opt).map_err(|e| e.to_string())?;
        let session = jit.compile_task(&mir).map_err(|e| e.to_string())?;
        Ok(Self {
            number,
            src,
            filename,
            program,
            checked,
            jit,
            session,
            without_init,
        })
    }

    /// Spawn the generation's run in the background. The returned
    /// handle owns nothing of the generation; the caller keeps the
    /// generation alive until the run is joined (the driver does).
    /// `Err` carries the immediate outcome when no run starts (same
    /// shape as `spawn_jit_run`; kept unboxed for call-site ergonomics).
    /// A `Some(seed)` spawn on a normally-compiled generation (or the
    /// reverse) is a driver bug: it fails loudly here, never by
    /// silently clobbering or skipping initialization.
    #[allow(clippy::result_large_err)]
    pub fn spawn(
        &self,
        config: RuntimeConfig,
        heap: Heap,
        modules: ClosureModules,
        seed: Option<PreservingSeed>,
    ) -> Result<(Box<dyn LiveRun>, Arc<Shared>), RtOutcome> {
        if seed.is_some() != self.without_init {
            return Err(RtOutcome {
                result: Err((
                    toto_compiler::diag::Diag::new(
                        "E_INTERNAL",
                        "preserving/compile mismatch: seed without init-strip or vice versa"
                            .to_string(),
                        toto_compiler::span::Span::new(0, 0),
                        None,
                    ),
                    Vec::new(),
                )),
                stdout: Vec::new(),
                stats: crate::task::zero_stats(),
            });
        }
        match spawn_jit_run(
            &self.program,
            &self.checked,
            config,
            &self.session,
            heap,
            self.number,
            modules,
            seed,
        ) {
            Ok(run) => {
                let shared = Arc::clone(run.shared());
                Ok((Box::new(run), shared))
            }
            Err(outcome) => Err(outcome),
        }
    }

    /// Render a finished run for output streaming (stdout verbatim,
    /// panics fully rendered with traces, like `toto run`).
    pub fn render(&self, outcome: &RtOutcome) -> EndedRun {
        render_ended(self.number, outcome, &self.src, &self.filename)
    }
}

/// A finished generation's streamed output: stdout verbatim, stderr
/// carrying the fully rendered panic (empty on success), and the
/// exit-code rendering (`toto run` parity: the numeric code, or
/// `"panic"`).
#[derive(Debug)]
pub struct EndedRun {
    /// Generation that produced this output.
    pub generation: u64,
    /// Exit code rendering (`"0"`, `"1"`, …, or `"panic"`).
    pub code: String,
    /// Captured stdout, verbatim.
    pub stdout: Vec<u8>,
    /// Rendered panic (with trace), or empty on success.
    pub stderr: String,
}

/// Render a finished run's outcome against its own source.
pub fn render_ended(generation: u64, outcome: &RtOutcome, src: &str, filename: &str) -> EndedRun {
    match &outcome.result {
        Ok(code) => EndedRun {
            generation,
            code: code.to_string(),
            stdout: outcome.stdout.clone(),
            stderr: String::new(),
        },
        Err((diag, trace)) => EndedRun {
            generation,
            code: "panic".to_string(),
            stdout: outcome.stdout.clone(),
            stderr: render_panic(diag, trace, src, filename),
        },
    }
}

/// How a retired generation settled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettleOutcome {
    /// The run finished on its own inside the drain timeout.
    Drained,
    /// The drain timed out; the run was cancelled and then joined.
    Cancelled,
    /// Cancel plus grace both failed; the restart policy fired and
    /// the run was abandoned (production restarts the process, so
    /// this only returns with a recording test double).
    Restarted,
}

/// Settle poll tick: short enough that drain deadlines are honored
/// promptly, long enough not to spin (one atomic load per tick).
const SETTLE_TICK: Duration = Duration::from_millis(1);

/// What `DevDriver::rebuild` decided.
#[derive(Debug)]
pub enum RebuildOutcome {
    /// The new source does not check; the old generation (if any)
    /// keeps running. Carries the diagnostics for display.
    StillBroken(Vec<toto_compiler::diag::Diag>),
    /// The new source checks and classifies `NoChange`: same
    /// semantics, so the running generation is kept (its classify
    /// baseline is updated to the new revision).
    NoChange,
    /// The old generation was retired (or there was none) and the new
    /// one is now running. `ended` is the retired run's streamed
    /// output (`None` on a fresh spawn); `settle` says how it
    /// retired (`None` on a fresh spawn).
    Rebuilt {
        ended: Option<EndedRun>,
        settle: Option<SettleOutcome>,
    },
    /// The driver's JIT compile step refused checked code (internal
    /// error, unreachable). The old generation keeps running.
    CompileRefused(String),
    /// The old run survived cancel plus grace; the restart policy
    /// fired, the old run was abandoned, and nothing was spawned.
    Restarted,
}

/// A running generation: compiled inputs plus its live handle.
struct ActiveGeneration {
    compiled: Generation,
    run: Box<dyn LiveRun>,
}

/// Why a carried function reference failed to rebind to the new
/// generation. Every variant routes to loud atomic restart —
/// never to a guessed index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelinkFail {
    /// `Func` index with no function at that position (defensive:
    /// unreachable from real checked code).
    IndexOutOfRange { idx: usize },
    /// The old function no longer exists under its name.
    NameGone { name: String },
    /// Same name, different parameter or return types. The new
    /// generation's code was checked against the new signature,
    /// so rebinding would silently change what the value calls.
    SignatureChanged { name: String },
}

/// Rebind an old-generation `Func` index into the new generation
/// (Phase 6, Stage 2a): look the old index up by NAME in the old
/// MIR table, find the same name in the new MIR table, and require
/// identical parameter and return TYPES (parameter names are
/// binding-site and ignored). Returns the new index on success.
///
/// Positions are MIR order on both sides — the same lowering the
/// runtime uses to build its dispatch tables, so the returned
/// index resolves through the new generation's `func_names` exactly
/// like a natively created `Func` value.
pub fn relink_func(
    old_checked: &CheckedProgram,
    new_checked: &CheckedProgram,
    old_idx: usize,
) -> Result<usize, RelinkFail> {
    let old_mir = toto_compiler::mir::lower(old_checked);
    let (name, params, ret) = match old_mir.fns.get(old_idx) {
        Some(f) => (
            f.name.clone(),
            f.params.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>(),
            f.ret.clone(),
        ),
        None => return Err(RelinkFail::IndexOutOfRange { idx: old_idx }),
    };
    let new_mir = toto_compiler::mir::lower(new_checked);
    match new_mir.fns.iter().position(|f| f.name == name) {
        None => Err(RelinkFail::NameGone { name }),
        Some(new_idx) => {
            let nf = &new_mir.fns[new_idx];
            let new_params: Vec<_> = nf.params.iter().map(|(_, t)| t.clone()).collect();
            if new_params == params && nf.ret == ret {
                Ok(new_idx)
            } else {
                Err(RelinkFail::SignatureChanged { name })
            }
        }
    }
}

/// What the last retire check decided, per retired generation
/// number (observability for tests and the Stage 4 soak).
/// `carry_abort` names the loud reason when a preserving reload
/// fell back to atomic restart (None on every other path).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RetireReport {
    pub retained: Vec<u64>,
    pub dropped: Vec<u64>,
    pub carry_abort: Option<String>,
}

/// The `toto dev` state machine: generations, drain-vs-cancel
/// rebuild, respawn. Owns no files and prints nothing; the CLI owns
/// the watcher, the loop, and all output.
///
/// Phase 6, Stage 2a: the driver additionally owns the
/// cross-generation closure→module table and the retired-module
/// set. Retention is by REAL GC reachability (`Heap::is_live`
/// over the shared heap): a retired module lives exactly while a
/// live closure names it. Nothing here changes the drain-first
/// order — quiescence is still established by settle before any
/// retire decision.
pub struct DevDriver<R: RestartPolicy = ReexecRestart> {
    policy: DrainPolicy,
    restart: R,
    config: RuntimeConfig,
    opt: OptLevel,
    next_gen: u64,
    current: Option<ActiveGeneration>,
    current_shared: Option<Arc<Shared>>,
    module_table: ClosureModules,
    retired: Vec<Generation>,
    carried: Vec<RtVal>,
    last_retire: RetireReport,
}

impl<R: RestartPolicy> DevDriver<R> {
    /// New driver: drain policy, restart policy, the scheduler config
    /// every spawned generation runs under, and the Cranelift
    /// optimization level every generation compiles at (one flag for
    /// the whole watch session: mixed-level generations would break
    /// the strictly-sequential no-mixing contract's auditability).
    pub fn new(policy: DrainPolicy, restart: R, config: RuntimeConfig, opt: OptLevel) -> Self {
        Self {
            policy,
            restart,
            config,
            opt,
            next_gen: 1,
            current: None,
            current_shared: None,
            module_table: toto_runtime::sched::fresh_closure_modules(),
            retired: Vec::new(),
            carried: Vec::new(),
            last_retire: RetireReport::default(),
        }
    }

    /// Number of the running generation, if any.
    pub fn current_gen(&self) -> Option<u64> {
        self.current.as_ref().map(|a| a.compiled.number)
    }

    /// Ingest new source: parse, check, classify against the running
    /// generation, and (unless unchanged or broken) retire the old
    /// run and respawn. The old run is always joined before the new
    /// one spawns: generations are strictly sequential, so old and
    /// new code can never mix. The first call starts generation 1.
    pub fn rebuild(&mut self, src: &str, filename: &str) -> RebuildOutcome {
        self.rebuild_with(src, filename, &[])
    }

    /// `rebuild` plus an explicit held-value set (Phase 6, Stage 2a):
    /// values the caller keeps alive across the reload. `Func`
    /// values are relinked by name against the new generation
    /// (failure → loud atomic restart, never a guessed index);
    /// closure handles must name a table entry or the same atomic
    /// restart fires (fail-safe on unknown handles). On success the
    /// relinked values are readable via `take_carried`; the old
    /// module is retained while any carried closure is live.
    /// Production callers pass `&[]` until Stage 3 wires declared
    /// state through here; the 2a proofs drive it directly.
    pub fn rebuild_with(&mut self, src: &str, filename: &str, carry: &[RtVal]) -> RebuildOutcome {
        self.carried = Vec::new();
        let (program, checked) = match parse_and_check(src) {
            Ok(parsed) => parsed,
            Err(diags) => return RebuildOutcome::StillBroken(diags),
        };
        let old = self.current.take();
        match old {
            None => self.spawn_fresh(src, filename, program, checked),
            Some(mut active) => {
                match classify_change(&active.compiled.checked, &checked) {
                    ChangeKind::NoChange => {
                        // Same semantics: adopt the new revision as
                        // the next classify baseline, keep running.
                        active.compiled.checked = checked;
                        active.compiled.program = program;
                        active.compiled.src = src.to_string();
                        active.compiled.filename = filename.to_string();
                        self.current = Some(active);
                        RebuildOutcome::NoChange
                    }
                    _ => {
                        // Compile the replacement BEFORE touching the
                        // running generation: a refusal leaves the old
                        // run undisturbed.
                        let number = self.next_gen;
                        let next = match Generation::compile(
                            number,
                            src.to_string(),
                            filename.to_string(),
                            program,
                            checked,
                            self.opt,
                        ) {
                            Err(e) => {
                                self.current = Some(active);
                                return RebuildOutcome::CompileRefused(e);
                            }
                            Ok(next) => next,
                        };
                        let (settle, ended) =
                            settle_run(active.run, &active.compiled, &self.policy, &self.restart);
                        match settle {
                            SettleOutcome::Restarted => {
                                // Abandon everything: the process
                                // restarts (production) or the test
                                // double recorded it. Nothing is
                                // preserved — the next rebuild starts
                                // from a fresh heap with a clean
                                // table, so no stale handle can ever
                                // resolve into the new world.
                                self.retired.clear();
                                self.module_table.lock().expect("module table lock").clear();
                                self.current_shared = None;
                                self.last_retire = RetireReport::default();
                                RebuildOutcome::Restarted
                            }
                            drained_or_cancelled => {
                                // Move the heap out of the drained
                                // run (safe: settle joined every
                                // task, and this clone is the sole
                                // remaining owner of the old Shared).
                                // Declared-state slots and the
                                // channel table travel with it: the
                                // preserving path below seeds the
                                // next generation from them.
                                let (mut heap, old_state_vars, old_channels) =
                                    match self.current_shared.take() {
                                        Some(shared) => {
                                            let mut st =
                                                shared.state.lock().expect("shared state lock");
                                            let h = std::mem::replace(&mut st.heap, Heap::new());
                                            let v = st.state_vars.clone();
                                            let c = std::mem::replace(
                                                &mut st.channels,
                                                toto_runtime::channel::ChannelTable::new(),
                                            );
                                            (h, v, c)
                                        }
                                        None => (
                                            Heap::new(),
                                            Vec::new(),
                                            toto_runtime::channel::ChannelTable::new(),
                                        ),
                                    };
                                // Declared-state preserving path
                                // (Phase 6, Stage 3): with no
                                // explicit 2a carry and state on
                                // either side, plan the carry from
                                // the settled run's slots. Success
                                // recompiles without init and seeds
                                // the next generation; ANY failure
                                // falls through to the loud atomic
                                // restart below (fresh heap,
                                // initializers run once).
                                let old_checked = &active.compiled.checked;
                                if carry.is_empty()
                                    && (!old_checked.state.is_empty()
                                        || !next.checked.state.is_empty())
                                {
                                    match plan_carry(
                                        old_checked,
                                        &old_state_vars,
                                        &mut heap,
                                        &old_channels,
                                        &next.checked,
                                    ) {
                                        Ok(carried_values) => {
                                            // `active.run` moved into
                                            // settle above; only the
                                            // compiled remainder
                                            // travels from here.
                                            return self.spawn_preserving(
                                                active.compiled,
                                                next,
                                                ended,
                                                drained_or_cancelled,
                                                heap,
                                                old_channels,
                                                number,
                                                carried_values,
                                            );
                                        }
                                        Err(abort) => {
                                            return self.atomic_restart(
                                                next,
                                                ended,
                                                drained_or_cancelled,
                                                number,
                                                Some(abort.reason),
                                            );
                                        }
                                    }
                                }
                                // Validate + relink carried values
                                // against old→new. ANY failure routes
                                // to loud atomic restart: nothing is
                                // preserved, the old module is NOT
                                // retained, the new generation gets a
                                // fresh heap.
                                let carried = match Self::relink_carry(
                                    &active.compiled.checked,
                                    &next.checked,
                                    &self.module_table,
                                    &heap,
                                    carry,
                                ) {
                                    Ok(values) => values,
                                    Err(_) => {
                                        return self.atomic_restart(
                                            next,
                                            ended,
                                            drained_or_cancelled,
                                            number,
                                            None,
                                        );
                                    }
                                };
                                // The drained generation's number
                                // (read before the move below): a
                                // carried `Func` names it without
                                // pinning heap.
                                let drained_no = active.compiled.number;
                                if !carried.is_empty() {
                                    // Retention: the drained module
                                    // stays callable while a carried
                                    // closure names it. Quiescence
                                    // holds by construction (settle
                                    // joined every task above).
                                    self.retired.push(active.compiled);
                                }
                                if !self.retired.is_empty() {
                                    let mut held = Vec::new();
                                    for v in &carried {
                                        toto_runtime::value::heap_refs_of(v, &mut held);
                                    }
                                    // A carried `Func` names the
                                    // just-drained generation
                                    // without pinning heap: retain
                                    // it for exactly this span.
                                    let force_retain = carried
                                        .iter()
                                        .any(|v| matches!(v, toto_runtime::value::RtVal::Func(_)))
                                        .then_some(drained_no);
                                    self.last_retire =
                                        self.prune_retired(&mut heap, &held, force_retain);
                                }
                                // Cheap table hygiene every rebuild
                                // (no collect): drop entries whose
                                // slots were swept since.
                                self.module_table
                                    .lock()
                                    .expect("module table lock")
                                    .retain(|r, _| heap.is_live(*r));
                                let outcome = self.spawn_next(
                                    next,
                                    ended,
                                    Some(drained_or_cancelled),
                                    heap,
                                    number,
                                    None,
                                );
                                self.carried = carried;
                                outcome
                            }
                        }
                    }
                }
            }
        }
    }

    /// Compile and spawn without retiring anything (first generation,
    /// or recovery after `StillBroken` with nothing running).
    fn spawn_fresh(
        &mut self,
        src: &str,
        filename: &str,
        program: Program,
        checked: CheckedProgram,
    ) -> RebuildOutcome {
        let number = self.next_gen;
        match Generation::compile(
            number,
            src.to_string(),
            filename.to_string(),
            program,
            checked,
            self.opt,
        ) {
            Err(e) => RebuildOutcome::CompileRefused(format!("generation {number}: {e}")),
            Ok(built) => {
                let outcome = self.spawn_next(built, None, None, Heap::new(), number, None);
                // Fresh heaps never retain: make that explicit so a
                // stale `last_retire` can never be misread later.
                self.last_retire = RetireReport::default();
                outcome
            }
        }
    }

    /// Loud atomic restart (Phase 6, Stage 3): drop every retired
    /// module, clear the retention table, and respawn the new
    /// generation on a fresh heap (initializers run once, from
    /// scratch). `carry_abort` names the preserving failure when
    /// this fallback follows one (None on the pre-Stage-3 paths).
    fn atomic_restart(
        &mut self,
        next: Generation,
        ended: Option<EndedRun>,
        settle: SettleOutcome,
        number: u64,
        carry_abort: Option<String>,
    ) -> RebuildOutcome {
        let dropped = self.retired.iter().map(|c| c.number).collect::<Vec<_>>();
        self.retired.clear();
        self.module_table.lock().expect("module table lock").clear();
        self.last_retire = RetireReport {
            retained: Vec::new(),
            dropped,
            carry_abort,
        };
        let outcome = self.spawn_next(next, ended, Some(settle), Heap::new(), number, None);
        self.carried = Vec::new();
        outcome
    }

    /// Preserving respawn (Phase 6, Stage 3): recompile the settled
    /// replacement without the state-init prepend, retain the
    /// drained module while carried values name it, and seed the
    /// next generation's slots + channel table. Retention is by
    /// real GC reachability over the moved heap (`prune_retired`
    /// with the carried roots); a carried `Func` anywhere (top
    /// level or nested — deep scan, unlike the 2a top-level rule)
    /// force-retains the drained generation for exactly this span.
    #[allow(clippy::too_many_arguments)]
    fn spawn_preserving(
        &mut self,
        compiled: Generation,
        next: Generation,
        ended: Option<EndedRun>,
        settle: SettleOutcome,
        mut heap: Heap,
        channels: toto_runtime::channel::ChannelTable,
        number: u64,
        carried: Vec<RtVal>,
    ) -> RebuildOutcome {
        // The with-init `next` stays intact for the fallback below:
        // the preserving compile works from clones (checked
        // programs are cheap to clone — Arcs underneath).
        let pres = match Generation::compile_preserving(
            number,
            next.src.clone(),
            next.filename.clone(),
            next.program.clone(),
            next.checked.clone(),
            self.opt,
        ) {
            Err(e) => {
                return self.atomic_restart(
                    next,
                    ended,
                    settle,
                    number,
                    Some(format!("preserving recompile failed: {e}")),
                );
            }
            Ok(built) => built,
        };
        let drained_no = compiled.number;
        self.retired.push(compiled);
        let mut held = Vec::new();
        for v in &carried {
            toto_runtime::value::heap_refs_of(v, &mut held);
        }
        // Deep `Func` scan (nested included): indices are valid by
        // the planner's stability rule, and the drained module must
        // outlive every reference to them.
        let mut seen = HashSet::new();
        let force_retain = carried
            .iter()
            .any(|v| crate::preserve::contains_func(v, &heap, &mut seen))
            .then_some(drained_no);
        self.last_retire = self.prune_retired(&mut heap, &held, force_retain);
        // Cheap table hygiene every rebuild (no collect): drop
        // entries whose slots were swept since.
        self.module_table
            .lock()
            .expect("module table lock")
            .retain(|r, _| heap.is_live(*r));
        let seed = PreservingSeed {
            state_vars: carried.clone(),
            channels,
        };
        let outcome = self.spawn_next(pres, ended, Some(settle), heap, number, Some(seed));
        self.carried = carried;
        outcome
    }

    /// Spawn `next` as the current generation on `heap`, recording
    /// the run's shared state for the next rebuild's heap move.
    /// The spawn-failure arm is byte-identical to the pre-Phase-6
    /// path (missing main is unreachable in checked code).
    fn spawn_next(
        &mut self,
        next: Generation,
        ended: Option<EndedRun>,
        settle: Option<SettleOutcome>,
        heap: Heap,
        number: u64,
        seed: Option<PreservingSeed>,
    ) -> RebuildOutcome {
        // `RuntimeConfig` stopped being `Copy` in Wave 8 (the
        // coverage sink is an `Arc`): cloning here shares the sink
        // rather than moving it, which is exactly the semantics a
        // coverage run needs across generations.
        match next.spawn(self.config.clone(), heap, self.module_table.clone(), seed) {
            Ok((run, shared)) => {
                self.current_shared = Some(shared);
                self.current = Some(ActiveGeneration {
                    compiled: next,
                    run,
                });
                self.next_gen = number + 1;
                RebuildOutcome::Rebuilt { ended, settle }
            }
            Err(outcome) => {
                let ended = Some(next.render(&outcome));
                self.next_gen = number + 1;
                RebuildOutcome::Rebuilt { ended, settle }
            }
        }
    }

    /// Validate + relink held values old→new (Phase 6, Stage 2a).
    /// `Func` indices rebind by name with identical signatures
    /// (`relink_func`); closure handles must name a table entry.
    /// Anything else fails LOUD (`Err(())`) and the caller routes
    /// to atomic restart — never a guessed index, never a silent
    /// drop of one value while keeping the rest.
    fn relink_carry(
        old_checked: &CheckedProgram,
        new_checked: &CheckedProgram,
        table: &ClosureModules,
        heap: &Heap,
        carry: &[RtVal],
    ) -> Result<Vec<RtVal>, ()> {
        let table = table.lock().expect("module table lock");
        let mut out = Vec::with_capacity(carry.len());
        for v in carry {
            match v {
                RtVal::Func(i) => match relink_func(old_checked, new_checked, *i) {
                    Ok(new_i) => out.push(RtVal::Func(new_i)),
                    Err(_) => return Err(()),
                },
                RtVal::Heap(r) => {
                    // Table hit AND slot still owned by this
                    // generation: a swept slot (or a stale copy
                    // aliasing a reused slot) fails the whole
                    // carry rather than preserving half a state.
                    // The table is keyed by full `GcRef`
                    // (index + generation), so a reused slot
                    // never aliases: it misses the table.
                    if table.contains_key(r) && heap.get(*r).is_some() {
                        out.push(v.clone());
                    } else {
                        return Err(());
                    }
                }
                plain => out.push(plain.clone()),
            }
        }
        Ok(out)
    }

    /// Retire check (Phase 6, Stage 2a): force a collection with
    /// the held roots, then drop every retired module no live
    /// closure names and prune dead table entries. Decisions are
    /// exact at decision time (the forced collect precedes every
    /// `is_live` read); between checks dead closures may retain
    /// their module for at most one collection (the stated bound).
    /// Quiescence is the caller's contract: call only after the
    /// old run settled (no frames execute old code).
    ///
    /// `force_retain` names the just-drained generation when a
    /// carried `Func` validated against it: a plain-function
    /// reference pins no heap object (`heap_refs_of(Func)` is
    /// empty), so without this the collect below would drop a
    /// module the caller still names. The reference itself is the
    /// root; release (dropping the carried value) reclaims at the
    /// next check.
    fn prune_retired(
        &mut self,
        heap: &mut Heap,
        held: &[GcRef],
        force_retain: Option<u64>,
    ) -> RetireReport {
        let report = heap.collect(held);
        let _ = report;
        let table = self.module_table.lock().expect("module table lock");
        let mut retained = Vec::new();
        let mut dropped = Vec::new();
        self.retired.retain(|old| {
            let live = force_retain == Some(old.number)
                || table
                    .iter()
                    .any(|(r, g)| *g == old.number && heap.is_live(*r));
            if live {
                retained.push(old.number);
                true
            } else {
                dropped.push(old.number);
                false
            }
        });
        drop(table);
        self.module_table
            .lock()
            .expect("module table lock")
            .retain(|r, _| heap.is_live(*r));
        RetireReport {
            retained,
            dropped,
            carry_abort: None,
        }
    }

    /// Drain the last successful carry (Stage 3 consumes these into
    /// state slots; the 2a proofs assert them directly). Always
    /// empty after a non-carry rebuild — never stale.
    pub fn take_carried(&mut self) -> Vec<RtVal> {
        std::mem::take(&mut self.carried)
    }

    /// Last retire decision (observability for tests and soak).
    pub fn last_retire(&self) -> &RetireReport {
        &self.last_retire
    }
}

/// Retire one live run: wait for natural finish inside the drain
/// timeout; on expiry cancel and join; if the run still has not
/// finished after the cancel grace, fire the restart policy and
/// abandon it. Returns how the run settled plus its streamed output
/// (`None` only when the run was abandoned to restart).
pub fn settle_run(
    run: Box<dyn LiveRun>,
    built: &Generation,
    policy: &DrainPolicy,
    restart: &dyn RestartPolicy,
) -> (SettleOutcome, Option<EndedRun>) {
    let deadline = Instant::now() + policy.drain_timeout;
    while !run.is_finished() && Instant::now() < deadline {
        std::thread::sleep(SETTLE_TICK);
    }
    if run.is_finished() {
        let outcome = run.join();
        return (SettleOutcome::Drained, Some(built.render(&outcome)));
    }
    run.cancel();
    let grace = Instant::now() + policy.cancel_grace;
    while !run.is_finished() && Instant::now() < grace {
        std::thread::sleep(SETTLE_TICK);
    }
    if run.is_finished() {
        let outcome = run.join();
        return (SettleOutcome::Cancelled, Some(built.render(&outcome)));
    }
    restart.restart(&format!(
        "generation {} outlived cancel plus grace; restarting",
        built.number
    ));
    (SettleOutcome::Restarted, None)
}

/// Stage 2a retention/relink/callability proofs (authorization
/// §2.12): retention with REAL GC reachability in generated code
/// (no acquire/release stand-ins — the roots ARE the carried
/// values, `prune_retired` collects with exactly those roots),
/// relink including failure → loud atomic restart, and the
/// closure/function-pointer reference case: a retained reference
/// keeps its module callable across a reload, then release +
/// reclamation at the next check.
#[cfg(test)]
mod retain_proofs {
    use super::*;
    use std::time::{Duration, Instant};
    use toto_compiler::ast::{FnDecl, ItemKind};
    use toto_compiler::span::Span;
    use toto_runtime::channel::TaskId;
    use toto_runtime::heap::Payload;
    use toto_runtime::sched::ScopeId;

    use crate::runtime::encode;
    use crate::task::{JitTaskCtx, sorted_externs};

    pub(super) fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            gc_threshold_bytes: u64::MAX,
            rng_seed: 0x9E37_79B9_7F4A_7C15,
            stack_bytes: 64 * 1024,
            collect_at_every_safepoint_for_test: false,
            workers: 1,
            blocking_pool_threads: 2,
            // No coverage sink on dev-driver test paths (Wave 8).
            coverage: None,
        }
    }

    pub(super) fn test_driver() -> DevDriver<ReexecRestart> {
        DevDriver::new(
            DrainPolicy {
                drain_timeout: Duration::from_millis(300),
                cancel_grace: Duration::from_millis(300),
            },
            ReexecRestart,
            test_config(),
            OptLevel::Off,
        )
    }

    pub(super) fn wait_finished<R: RestartPolicy>(drv: &DevDriver<R>, what: &str) {
        let t0 = Instant::now();
        loop {
            let done = drv
                .current
                .as_ref()
                .map(|a| a.run.is_finished())
                .unwrap_or(false);
            if done {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "run never finished: {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn cur_number<R: RestartPolicy>(drv: &DevDriver<R>) -> u64 {
        drv.current.as_ref().expect("current").compiled.number
    }

    fn func_idx<R: RestartPolicy>(drv: &DevDriver<R>, name: &str) -> usize {
        drv.current
            .as_ref()
            .expect("current")
            .compiled
            .session
            .func_names
            .iter()
            .position(|n| n == name)
            .expect("function present")
    }

    pub(super) fn rebuilt<R: RestartPolicy>(
        drv: &mut DevDriver<R>,
        src: &str,
        carry: &[RtVal],
        what: &str,
    ) -> (Option<EndedRun>, Option<SettleOutcome>) {
        match drv.rebuild_with(src, "t.gzv", carry) {
            RebuildOutcome::Rebuilt { ended, settle } => (ended, settle),
            RebuildOutcome::StillBroken(diags) => {
                panic!("{what}: unexpectedly StillBroken ({} diags)", diags.len())
            }
            RebuildOutcome::NoChange => panic!("{what}: unexpectedly NoChange"),
            RebuildOutcome::CompileRefused(e) => {
                panic!("{what}: unexpectedly CompileRefused: {e}")
            }
            RebuildOutcome::Restarted => panic!("{what}: unexpectedly Restarted"),
        }
    }

    pub(super) fn ended_stdout(ended: &Option<EndedRun>, what: &str) -> Vec<u8> {
        let e = ended
            .as_ref()
            .unwrap_or_else(|| panic!("{what}: no ended run"));
        assert_eq!(e.code, "0", "{what}: exit code");
        e.stdout.clone()
    }

    /// First spawn of a driver: no old run exists, so there is
    /// nothing to settle and nothing streamed (`(None, None)`).
    /// The new run's own output surfaces via the NEXT rebuild's
    /// `ended` (stream-on-retire).
    pub(super) fn spawn_first<R: RestartPolicy>(drv: &mut DevDriver<R>, src: &str, what: &str) {
        let (ended, settle) = rebuilt(drv, src, &[], what);
        assert!(ended.is_none(), "{what}: fresh spawn streams nothing");
        assert!(settle.is_none(), "{what}: fresh spawn settles nothing");
        wait_finished(drv, what);
    }

    fn closure_refs_of(drv: &DevDriver<ReexecRestart>) -> Vec<(GcRef, u32)> {
        drv.current_shared
            .as_ref()
            .expect("shared")
            .state
            .lock()
            .expect("shared state lock")
            .heap
            .closure_refs()
    }

    /// Call a retained closure body directly through its retired
    /// module's entry address. TEST-ONLY conditions (not a product
    /// path — production cross-generation calls go through real
    /// tasks with real shadows in Stage 3): the bodies under test
    /// allocate nothing (no safepoint can fire mid-call), the
    /// driver's threshold is `u64::MAX` (no background collect can
    /// fire either), and missed-shadow publication is a no-op (the
    /// `task_shadow_push` missing-slot skip). The `handle` word
    /// uses the frozen ADR-008 codec (`(gen << 32) | index`).
    ///
    /// Closed-world dispatch: every table (`entries`, `bodies`,
    /// `func_names`, `chan_types`, `externs`) and the AST function
    /// list come from the RETIRED generation, never the current
    /// one. Old code therefore resolves every callee to an old
    /// address — it can never call new code (the Stage-3 contract
    /// for production cross-generation calls).
    fn call_retained_closure(
        drv: &DevDriver<ReexecRestart>,
        retired_no: usize,
        r: GcRef,
        arg: u64,
    ) -> u64 {
        let shared = std::sync::Arc::clone(drv.current_shared.as_ref().expect("shared"));
        let body = match shared.state.lock().expect("shared state lock").heap.get(r) {
            Some(Payload::Closure { body, .. }) => *body,
            _ => panic!("retained handle is not a closure payload"),
        };
        let ret = &drv.retired[retired_no];
        let entry = *ret.session.bodies.get(&body).expect("retained body entry");
        let sess = &ret.session;
        let mut fns: Vec<FnDecl> = Vec::new();
        for item in &ret.program.items {
            if let ItemKind::Fn(d) = &item.kind {
                fns.push(d.clone());
            }
        }
        let ctx = JitTaskCtx {
            shared,
            task: TaskId(usize::MAX),
            scope: ScopeId(usize::MAX),
            yielder: None,
            entry: 0,
            captures: Vec::new(),
            root_name: "retain_proof".to_string(),
            root_span: Span::new(0, 0),
            is_main: false,
            failed: false,
            diag: None,
            skip_frame: false,
            trace: Vec::new(),
            fns: std::sync::Arc::new(fns),
            entries: std::sync::Arc::clone(&sess.entries),
            bodies: std::sync::Arc::clone(&sess.bodies),
            func_names: std::sync::Arc::clone(&sess.func_names),
            chan_types: std::sync::Arc::clone(&sess.chan_types),
            externs: std::sync::Arc::new(sorted_externs(&ret.checked)),
        };
        let mut boxed = Box::new(ctx);
        let handle = encode(r);
        let ret = unsafe {
            // SAFETY: `entry` is the retired module's compiled body
            // address (same transmute shape `compile.rs` uses for
            // helper calls); the call runs no safepoint (see above),
            // touches no task state, and the box outlives the call.
            let f: unsafe extern "C" fn(*mut JitTaskCtx, u64, u64) -> u64 =
                std::mem::transmute(entry);
            f((&mut *boxed) as *mut JitTaskCtx, handle, arg)
        };
        assert!(!boxed.failed, "retained call recorded a failure");
        assert!(boxed.diag.is_none(), "retained call recorded a diag");
        ret
    }

    /// Call a retained plain function directly through its retired
    /// module's entry address. Same test-only conditions as
    /// `call_retained_closure`; entry signature is `(ctx, args...)`
    /// with no handle word (the index arm of the call emission).
    fn call_retained_func(
        drv: &DevDriver<ReexecRestart>,
        retired_no: usize,
        name: &str,
        arg: u64,
    ) -> u64 {
        let shared = std::sync::Arc::clone(drv.current_shared.as_ref().expect("shared"));
        let ret = &drv.retired[retired_no];
        let entry = *ret
            .session
            .entries
            .get(name)
            .expect("retained function entry");
        let sess = &ret.session;
        let mut fns: Vec<FnDecl> = Vec::new();
        for item in &ret.program.items {
            if let ItemKind::Fn(d) = &item.kind {
                fns.push(d.clone());
            }
        }
        let ctx = JitTaskCtx {
            shared,
            task: TaskId(usize::MAX),
            scope: ScopeId(usize::MAX),
            yielder: None,
            entry: 0,
            captures: Vec::new(),
            root_name: "retain_proof".to_string(),
            root_span: Span::new(0, 0),
            is_main: false,
            failed: false,
            diag: None,
            skip_frame: false,
            trace: Vec::new(),
            fns: std::sync::Arc::new(fns),
            entries: std::sync::Arc::clone(&sess.entries),
            bodies: std::sync::Arc::clone(&sess.bodies),
            func_names: std::sync::Arc::clone(&sess.func_names),
            chan_types: std::sync::Arc::clone(&sess.chan_types),
            externs: std::sync::Arc::new(sorted_externs(&ret.checked)),
        };
        let mut boxed = Box::new(ctx);
        let ret = unsafe {
            // SAFETY: same conditions as `call_retained_closure`.
            let f: unsafe extern "C" fn(*mut JitTaskCtx, u64) -> u64 = std::mem::transmute(entry);
            f((&mut *boxed) as *mut JitTaskCtx, arg)
        };
        assert!(!boxed.failed, "retained call recorded a failure");
        assert!(boxed.diag.is_none(), "retained call recorded a diag");
        ret
    }

    // The closure calls back into its own module (`helper`):
    // the closed-world case. GEN2 changes `helper`'s body (same
    // signature, so relink still validates) — a retained call that
    // dispatches through the NEW tables would observe `a * 100`,
    // the closed world observes `a * 2`.
    const GEN1: &str = "fn adder(x: int) -> fn(int) -> int {\n    return fn(y: int) -> int { return helper(x + y); };\n}\nfn helper(a: int) -> int {\n    return a * 2;\n}\nfn main() -> int {\n    let add2 = adder(2);\n    println(add2(3));\n    println(helper(21));\n    return 0;\n}\n";
    const GEN2: &str = "fn adder(x: int) -> fn(int) -> int {\n    return fn(y: int) -> int { return helper(x + y); };\n}\nfn helper(a: int) -> int {\n    return a * 100;\n}\nfn main() -> int {\n    let add10 = adder(10);\n    println(add10(3));\n    println(helper(20));\n    return 0;\n}\n";
    const GEN3: &str = "fn adder(x: int) -> fn(int) -> int {\n    return fn(y: int) -> int { return helper(x + y); };\n}\nfn helper(a: int) -> int {\n    return a * 100;\n}\nfn main() -> int {\n    let add11 = adder(11);\n    println(add11(3));\n    println(helper(19));\n    return 0;\n}\n";

    /// Exit criterion (ii)+(iv), closure side: a carried closure
    /// keeps its module alive across a reload (real GC
    /// reachability — the collect roots ARE the carried values),
    /// the old code stays callable with old captures, then release
    /// + reclamation at the next check.
    #[test]
    fn closure_retained_callable_then_reclaimed() {
        let mut drv = test_driver();
        spawn_first(&mut drv, GEN1, "gen1 spawn");
        let n1 = cur_number(&drv);

        // Exactly one closure exists, stamped with gen 1 in the
        // retention table (`closure_refs` reports body ids, not
        // versions — the version lives in the table).
        let closures = closure_refs_of(&drv);
        assert_eq!(closures.len(), 1, "one closure allocated: {closures:?}");
        let (r, _) = closures[0];
        let stamp = drv
            .module_table
            .lock()
            .expect("module table lock")
            .get(&r)
            .copied();
        assert_eq!(stamp, Some(n1), "closure stamped with gen 1");
        let helper_idx = func_idx(&drv, "helper");

        // Reload with a main-only edit, carrying the closure and a
        // plain-function reference.
        let (ended, settle) = rebuilt(
            &mut drv,
            GEN2,
            &[RtVal::Heap(r), RtVal::Func(helper_idx)],
            "gen2",
        );
        assert_eq!(settle, Some(SettleOutcome::Drained), "gen2 drains");
        assert_eq!(ended_stdout(&ended, "gen1"), b"10\n42\n");
        wait_finished(&drv, "gen2");

        // Retention: gen 1 survives while the carried closure is
        // live; the carried values come back unchanged in shape.
        assert_eq!(drv.last_retire().retained, vec![n1], "gen 1 retained");
        assert!(drv.last_retire().dropped.is_empty(), "nothing dropped");
        let carried = drv.take_carried();
        assert_eq!(carried.len(), 2, "both values carried");
        assert!(
            matches!(carried[0], RtVal::Heap(rr) if rr == r),
            "closure unchanged"
        );
        let new_helper = func_idx(&drv, "helper");
        assert!(
            matches!(carried[1], RtVal::Func(i) if i == new_helper),
            "func relinked"
        );

        // Heap continuity: the SAME heap object survived the reload.
        let live = drv
            .current_shared
            .as_ref()
            .expect("shared")
            .state
            .lock()
            .expect("shared state lock")
            .heap
            .get(r)
            .is_some();
        assert!(live, "carried closure resolves in the new heap");

        // Callability, closed world: the OLD closure runs with OLD
        // captures through the OLD tables — adder(2), helper `a*2`:
        // helper(2 + 3) = 10. Dispatched through the NEW tables the
        // same call would observe helper `a*100` = 1000.
        assert_eq!(drv.retired.len(), 1, "one retired module");
        assert_eq!(call_retained_closure(&drv, 0, r, 3), 10, "old captures");

        // Release: drop every handle, reload again with no carry.
        // The next check reclaims gen 1 (the stated bound: release
        // at the next forced collection).
        drop(carried);
        let (ended, _) = rebuilt(&mut drv, GEN3, &[], "gen3");
        assert_eq!(ended_stdout(&ended, "gen2"), b"1300\n2000\n");
        wait_finished(&drv, "gen3");
        assert!(drv.last_retire().retained.is_empty(), "nothing retained");
        assert_eq!(drv.last_retire().dropped, vec![n1], "gen 1 reclaimed");
        assert!(drv.retired.is_empty(), "retired set empty");
        let refs = closure_refs_of(&drv);
        assert!(!refs.is_empty(), "gen 3 allocated its own closure");
        let table = drv.module_table.lock().expect("module table lock");
        assert!(
            table.iter().all(|(_, g)| *g != n1),
            "no gen-1 entry remains: {table:?}"
        );
    }

    const GEN1F: &str = "fn helper(a: int) -> int {\n    return a * 2;\n}\nfn main() -> int {\n    println(helper(21));\n    return 0;\n}\n";
    const GEN2F: &str = "fn helper(a: int) -> int {\n    return a + 1;\n}\nfn main() -> int {\n    println(helper(21));\n    return 0;\n}\n";
    const GEN3F: &str = "fn helper(a: int) -> int {\n    return a + 1;\n}\nfn main() -> int {\n    println(helper(20));\n    return 0;\n}\n";
    const GEN2G: &str = "fn main() -> int {\n    println(0);\n    return 0;\n}\n";

    /// Exit criterion (iii) positive + (iv) function side: a
    /// carried `Func` keeps its module alive even though it pins
    /// no heap object (the `force_retain` path — the heap holds no
    /// closures at all here, so heap reachability alone would drop
    /// it), the old entry stays callable, then release reclaims.
    #[test]
    fn func_retained_callable_then_reclaimed() {
        let mut drv = test_driver();
        spawn_first(&mut drv, GEN1F, "gen1f spawn");
        let n1 = cur_number(&drv);
        assert!(closure_refs_of(&drv).is_empty(), "no closures in play");
        let helper_idx = func_idx(&drv, "helper");

        // helper's body changes but its signature does not: the
        // carried reference relinks and gen 1 is force-retained
        // (the heap names nothing — without `force_retain` this
        // collect would drop it).
        let (ended, _) = rebuilt(&mut drv, GEN2F, &[RtVal::Func(helper_idx)], "gen2f");
        assert_eq!(ended_stdout(&ended, "gen1f"), b"42\n");
        wait_finished(&drv, "gen2f");
        assert!(closure_refs_of(&drv).is_empty(), "still no closures");
        assert_eq!(drv.last_retire().retained, vec![n1], "gen 1 force-retained");
        let carried = drv.take_carried();
        assert_eq!(carried.len(), 1, "func carried");
        let new_helper = func_idx(&drv, "helper");
        assert!(
            matches!(carried[0], RtVal::Func(i) if i == new_helper),
            "func relinked"
        );

        // The OLD entry runs the OLD body (a * 2, not a + 1).
        assert_eq!(call_retained_func(&drv, 0, "helper", 21), 42, "old body");

        // Release reclaims at the next check.
        drop(carried);
        let (ended, _) = rebuilt(&mut drv, GEN3F, &[], "gen3f");
        assert_eq!(ended_stdout(&ended, "gen2f"), b"22\n");
        assert_eq!(drv.last_retire().dropped, vec![n1], "gen 1 reclaimed");
        assert!(drv.retired.is_empty(), "retired set empty");
    }

    /// Exit criterion (iii) negative: removing the named function
    /// fails relink (`NameGone`) and routes to loud atomic restart
    /// — fresh heap, clean table, nothing carried — never a guessed
    /// index into the new generation.
    #[test]
    fn func_removed_routes_to_atomic_restart() {
        let mut drv = test_driver();
        spawn_first(&mut drv, GEN1F, "gen1f spawn");
        let helper_idx = func_idx(&drv, "helper");

        let (ended, settle) = rebuilt(&mut drv, GEN2G, &[RtVal::Func(helper_idx)], "gen2g");
        assert_eq!(settle, Some(SettleOutcome::Drained), "old run drains first");
        // The retired run still streams its own output (atomic
        // restart preserves the drain, not the state).
        assert_eq!(ended_stdout(&ended, "gen1f"), b"42\n");
        wait_finished(&drv, "gen2g");
        // Nothing preserved: loud, total.
        assert!(drv.take_carried().is_empty(), "carry dropped");
        assert!(drv.last_retire().retained.is_empty(), "nothing retained");
        assert!(drv.last_retire().dropped.is_empty(), "nothing was retired");
        assert!(drv.retired.is_empty(), "retired set empty");
        let table = drv.module_table.lock().expect("module table lock");
        assert!(table.is_empty(), "table clean");
    }

    /// The relink liveness gate: a table hit on a swept slot (or
    /// any unknown handle) fails the whole carry → atomic restart,
    /// never preservation of a dead reference. The swept-slot case
    /// exercises the `heap.get` gate specifically (the entry is
    /// still tabled — no hygiene has run — but the object is gone).
    #[test]
    fn stale_carry_fails_atomic() {
        let mut drv = test_driver();
        spawn_first(&mut drv, GEN1, "gen1 spawn");
        let (r, _) = closure_refs_of(&drv)[0];

        // Sweep the closure out from under its table entry.
        let swept = drv
            .current_shared
            .as_ref()
            .expect("shared")
            .state
            .lock()
            .expect("shared state lock")
            .heap
            .collect(&[])
            .collected;
        assert!(swept >= 1, "the sweep reclaimed the closure");
        {
            let st = drv
                .current_shared
                .as_ref()
                .expect("shared")
                .state
                .lock()
                .expect("shared state lock");
            assert!(st.heap.get(r).is_none(), "slot is gone");
            assert!(!st.heap.is_live(r), "swept object must not resolve");
        }

        // Table hit + dead slot → atomic restart, loudly.
        let (ended, _) = rebuilt(&mut drv, GEN2, &[RtVal::Heap(r)], "gen2 stale");
        assert_eq!(ended_stdout(&ended, "gen1"), b"10\n42\n");
        wait_finished(&drv, "gen2 stale");
        let n2 = cur_number(&drv);
        assert!(drv.take_carried().is_empty(), "carry dropped");
        assert!(drv.last_retire().retained.is_empty(), "nothing retained");
        {
            let table = drv.module_table.lock().expect("module table lock");
            assert!(!table.is_empty(), "gen 2 stamped its own closure");
            assert!(
                table.iter().all(|(_, g)| *g == n2),
                "no gen-1 entry survives"
            );
        }

        // Unknown handle (never tabled) → same loud outcome.
        let bogus = GcRef::from_parts(424242, 424242);
        let (ended, _) = rebuilt(&mut drv, GEN3, &[RtVal::Heap(bogus)], "gen3 bogus");
        assert_eq!(ended_stdout(&ended, "gen2"), b"1300\n2000\n");
        wait_finished(&drv, "gen3 bogus");
        assert!(drv.take_carried().is_empty(), "bogus carry dropped");
    }
}

/// End-to-end preserving-reload proofs (Phase 6, Stage 3): drive
/// `rebuild` through the real driver and assert the next
/// generation's OBSERVED output. Stream-on-retire means each
/// generation's stdout surfaces at the NEXT rebuild; every test
/// below is spawn → reload-under-test → flush, asserting the middle
/// generation's exact bytes.
#[cfg(test)]
mod preserve_proofs {
    use super::retain_proofs::{
        ended_stdout, rebuilt, spawn_first, test_config, test_driver, wait_finished,
    };
    use super::*;
    use std::time::Duration;

    /// Body-only change preserves state (matrix row 1): the new
    /// generation prints the CARRIED value, never a re-initialized
    /// one — initializers run once, on fresh load.
    #[test]
    fn body_change_preserves_state_e2e() {
        let mut drv = test_driver();
        let gen1 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    count = 41;\n    println(count);\n    return 0;\n}\n";
        // Same state, different body.
        let gen2 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n";
        // Trivial body tweak to flush gen2's output.
        let gen3 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count + 0);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 body");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 body");
        assert_eq!(ended_stdout(&ended, "gen1"), b"41\n");
        wait_finished(&drv, "gen2 body");
        // Declared carry is observable through the driver handoff.
        assert_eq!(drv.take_carried(), vec![RtVal::Int(41)]);
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 body");
        // Carried 41, NOT re-initialized 0.
        assert_eq!(ended_stdout(&ended, "gen2"), b"41\n");
        wait_finished(&drv, "gen3 body");
        assert!(drv.last_retire().carry_abort.is_none());
    }

    /// State-initializer change (matrix row 5): same names and
    /// types, new init code — the OLD value carries; initializers
    /// never rerun on the preserving path.
    #[test]
    fn init_change_carries_old_value_e2e() {
        let mut drv = test_driver();
        let gen1 = "state {\n    count: int = 7;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n";
        let gen2 = "state {\n    count: int = 999;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n";
        let gen3 = "state {\n    count: int = 999;\n}\nfn main() -> int {\n    println(count + 0);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 init");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 init");
        assert_eq!(ended_stdout(&ended, "gen1"), b"7\n");
        wait_finished(&drv, "gen2 init");
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 init");
        // Carried 7, NOT the new initializer's 999.
        assert_eq!(ended_stdout(&ended, "gen2"), b"7\n");
        wait_finished(&drv, "gen3 init");
    }

    /// Struct layout change WITH migrate (matrix row 3): the new
    /// generation observes migrated values end to end.
    #[test]
    fn migrate_end_to_end() {
        let mut drv = test_driver();
        let gen1 = "struct Cfg {\n    host: string,\n    port: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\", port: 1 };\n}\nfn main() -> int {\n    println(cfg.port);\n    cfg = Cfg { host: \"y\", port: 21 };\n    return 0;\n}\n";
        let gen2 = "struct Cfg {\n    host: string,\n    port: int,\n    tls: bool,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    let probe = if false { old.tls } else { false };\n    return Cfg { host: old.host, port: old.port + (if probe { 1 } else { 0 }), tls: old.port > 20 };\n}\nstate {\n    cfg: Cfg = Cfg { host: \"z\", port: 2, tls: false };\n}\nfn main() -> int {\n    println(cfg.port);\n    println(if cfg.tls { 1 } else { 0 });\n    return 0;\n}\n";
        let gen3 = "struct Cfg {\n    host: string,\n    port: int,\n    tls: bool,\n}\nmigrate fn Cfg(old: Cfg) -> Cfg {\n    let probe = if false { old.tls } else { false };\n    return Cfg { host: old.host, port: old.port + (if probe { 1 } else { 0 }), tls: old.port > 20 };\n}\nstate {\n    cfg: Cfg = Cfg { host: \"z\", port: 2, tls: false };\n}\nfn main() -> int {\n    println(cfg.port + 0);\n    println(if cfg.tls { 1 } else { 0 });\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 mig");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 mig");
        assert_eq!(ended_stdout(&ended, "gen1"), b"1\n");
        wait_finished(&drv, "gen2 mig");
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 mig");
        // Migrated: port 21 (carried, not re-init 2), tls true.
        assert_eq!(ended_stdout(&ended, "gen2"), b"21\n1\n");
        wait_finished(&drv, "gen3 mig");
        assert!(drv.last_retire().carry_abort.is_none());
    }

    /// Struct layout change WITHOUT migrate (matrix row 3): loud
    /// atomic restart — fresh initializers run, the abort reason is
    /// recorded, nothing is carried.
    #[test]
    fn missing_migrate_aborts_to_fresh_init_e2e() {
        let mut drv = test_driver();
        let gen1 = "struct Cfg {\n    host: string,\n    port: int,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"x\", port: 1 };\n}\nfn main() -> int {\n    println(cfg.port);\n    cfg = Cfg { host: \"y\", port: 21 };\n    return 0;\n}\n";
        let gen2 = "struct Cfg {\n    host: string,\n    port: int,\n    tls: bool,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"z\", port: 2, tls: true };\n}\nfn main() -> int {\n    println(cfg.port);\n    println(if cfg.tls { 1 } else { 0 });\n    return 0;\n}\n";
        let gen3 = "struct Cfg {\n    host: string,\n    port: int,\n    tls: bool,\n}\nstate {\n    cfg: Cfg = Cfg { host: \"z\", port: 2, tls: true };\n}\nfn main() -> int {\n    println(cfg.port + 0);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 abort");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 abort");
        assert_eq!(ended_stdout(&ended, "gen1"), b"1\n");
        wait_finished(&drv, "gen2 abort");
        // Loud fallback: reason recorded, nothing carried, fresh
        // init values observed (2/true, not carried 21).
        let abort = drv.last_retire().carry_abort.clone();
        assert!(abort.is_some(), "abort reason recorded");
        assert!(abort.unwrap().contains("no migrate"));
        assert!(drv.take_carried().is_empty());
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 abort");
        assert_eq!(ended_stdout(&ended, "gen2"), b"2\n1\n");
        wait_finished(&drv, "gen3 abort");
    }

    /// Function-signature change (matrix row 2): code restarts on
    /// the new tables while plain state carries (`migrate` plays
    /// no role in signature changes — pinned by amendment).
    #[test]
    fn signature_change_preserves_state_e2e() {
        let mut drv = test_driver();
        let gen1 = "state {\n    count: int = 0;\n}\nfn bump(x: int) -> int {\n    return x + 1;\n}\nfn main() -> int {\n    count = bump(41);\n    println(count);\n    return 0;\n}\n";
        let gen2 = "state {\n    count: int = 0;\n}\nfn bump(x: int, y: int) -> int {\n    return x + y;\n}\nfn main() -> int {\n    println(count);\n    count = bump(1, 2);\n    return 0;\n}\n";
        let gen3 = "state {\n    count: int = 0;\n}\nfn bump(x: int, y: int) -> int {\n    return x + y;\n}\nfn main() -> int {\n    println(count + 0);\n    count = bump(1, 2);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 sig");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 sig");
        assert_eq!(ended_stdout(&ended, "gen1"), b"42\n");
        wait_finished(&drv, "gen2 sig");
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 sig");
        // Carried 42 across the signature change (not re-init 0).
        assert_eq!(ended_stdout(&ended, "gen2"), b"42\n");
        wait_finished(&drv, "gen3 sig");
        assert!(drv.last_retire().carry_abort.is_none());
    }

    /// Drained and cancelled runs share the carry arm (matrix
    /// rows 1-2): cancellation lands on a suspension boundary,
    /// where state is statement-consistent, so the plan runs
    /// identically from the settled snapshot. A parked-Cancelled
    /// e2e is deliberately absent: a lone parked task trips the
    /// deadlock detector (E0216) before the drain timeout, and any
    /// live companion that suppresses it either completes (Drained)
    /// or ignores cancel (Restarted) — there is no deterministic
    /// single-threaded shape that settles Cancelled out of a park.
    /// The Drained e2e above exercises the shared arm; settle's
    /// Cancelled path is Phase-4-tested.
    #[test]
    fn drained_and_cancelled_share_the_carry_arm() {
        // Minimal carry through the shared arm (the Drained side;
        // Cancelled joins the same block after settle — see doc).
        let mut drv = test_driver();
        let gen1 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    count = 13;\n    println(count);\n    return 0;\n}\n";
        let gen2 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 shared-arm");
        // Fast drain: settles Drained through the shared arm.
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 shared-arm");
        assert_eq!(ended_stdout(&ended, "gen1"), b"13\n");
        wait_finished(&drv, "gen2 shared-arm");
        assert_eq!(drv.take_carried(), vec![RtVal::Int(13)]);
    }

    /// Declared channels survive the reload (matrix row 4): the
    /// table moves whole, so the new generation rendezvous on the
    /// SAME channel object. Counters alongside prove plain carry.
    #[test]
    fn channel_state_moves_whole_e2e() {
        let mut drv = test_driver();
        let gen1 = "state {\n    ch: chan<int>;\n    count: int = 0;\n}\nfn main() -> int {\n    go {\n        ch <- 1;\n    }\n    count = <-ch;\n    println(count);\n    return 0;\n}\n";
        let gen2 = "state {\n    ch: chan<int>;\n    count: int = 0;\n}\nfn main() -> int {\n    go {\n        ch <- 2;\n    }\n    count = <-ch;\n    println(count);\n    return 0;\n}\n";
        let gen3 = "state {\n    ch: chan<int>;\n    count: int = 0;\n}\nfn main() -> int {\n    go {\n        ch <- 3;\n    }\n    count = <-ch;\n    println(count + 0);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 chan");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 chan");
        assert_eq!(ended_stdout(&ended, "gen1"), b"1\n");
        wait_finished(&drv, "gen2 chan");
        // Carried count was 1 at the boundary (observed through the
        // handoff); the new run overwrote it through the moved
        // channel — the table moved whole or this rendezvous dies.
        assert_eq!(drv.take_carried()[1], RtVal::Int(1));
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 chan");
        assert_eq!(ended_stdout(&ended, "gen2"), b"2\n");
        wait_finished(&drv, "gen3 chan");
        assert!(drv.last_retire().carry_abort.is_none());
    }

    /// Timeout abandonment never carries (fallback integrity): the
    /// abandoned run's tasks may still touch its state, so reading
    /// it would race. Only drain and cancel preserve.
    #[test]
    fn timeout_abandonment_carries_nothing() {
        // `RecordRestart`, never `ReexecRestart`: the production
        // policy re-spawns the current binary, which under test
        // re-runs the suite into an unbounded re-exec chain.
        let restart = super::RecordRestart::default();
        let mut drv = DevDriver::new(
            super::DrainPolicy {
                drain_timeout: Duration::from_millis(50),
                cancel_grace: Duration::from_millis(50),
            },
            restart.clone(),
            test_config(),
            super::OptLevel::Off,
        );
        // Ignores cancellation (no suspension point in the loop):
        // settle abandons it to restart. Abandoned output is
        // dropped with the run — only fresh behavior is observed.
        let gen1 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    count = 5;\n    println(count);\n    while true {\n        count = count;\n    }\n    return 0;\n}\n";
        let gen2 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n";
        let gen3 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count + 0);\n    return 0;\n}\n";
        // No waiting: gen1 never finishes by design.
        match drv.rebuild_with(gen1, "t.gzv", &[]) {
            super::RebuildOutcome::Rebuilt { ended, settle } => {
                assert!(ended.is_none());
                assert!(settle.is_none());
            }
            other => panic!("gen1 stuck: unexpected {other:?}"),
        }
        match drv.rebuild_with(gen2, "t.gzv", &[]) {
            super::RebuildOutcome::Restarted => {}
            other => panic!("gen2 stuck: expected abandonment, got {other:?}"),
        }
        // The restart policy fired exactly once (recorded, not
        // executed — the process stays in the test).
        assert_eq!(restart.fired().len(), 1, "one recorded restart");
        // Post-abandon rebuilds spawn fresh from a clean table.
        let (ended, settle) = rebuilt(&mut drv, gen3, &[], "gen3 stuck");
        assert!(settle.is_none(), "fresh spawn settles nothing");
        assert!(ended.is_none(), "fresh spawn streams nothing");
        wait_finished(&drv, "gen3 stuck");
        assert!(drv.take_carried().is_empty());
        let gen4 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count + 1 - 1);\n    return 0;\n}\n";
        let (ended, _) = rebuilt(&mut drv, gen4, &[], "gen4 stuck");
        // Fresh init (0), never the abandoned run's 5.
        assert_eq!(ended_stdout(&ended, "gen3"), b"0\n");
        wait_finished(&drv, "gen4 stuck");
    }

    /// Broken edit on the preserving path (Stage 0 mixed soak): a
    /// `StillBroken` rebuild leaves the running generation
    /// undisturbed, and the next GOOD edit still preserves — the
    /// broken source never touches state, and preservation does not
    /// require an unbroken history.
    #[test]
    fn broken_edit_keeps_run_then_next_good_preserves() {
        let mut drv = test_driver();
        let gen1 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    count = 5;\n    println(count);\n    return 0;\n}\n";
        let broken = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    let x = ;\n    return 0;\n}\n";
        let gen2 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count);\n    return 0;\n}\n";
        let gen3 = "state {\n    count: int = 0;\n}\nfn main() -> int {\n    println(count + 0);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 mixed");
        // Broken edit: loud, nothing carried, old run undisturbed.
        match drv.rebuild_with(broken, "t.gzv", &[]) {
            super::RebuildOutcome::StillBroken(_) => {}
            other => panic!("mixed: expected StillBroken, got {other:?}"),
        }
        assert!(drv.take_carried().is_empty(), "broken: nothing carried");
        assert!(drv.last_retire().carry_abort.is_none());
        // Next good edit preserves across the broken one.
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 mixed");
        assert_eq!(ended_stdout(&ended, "gen1"), b"5\n");
        wait_finished(&drv, "gen2 mixed");
        assert_eq!(drv.take_carried(), vec![RtVal::Int(5)]);
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 mixed");
        assert_eq!(ended_stdout(&ended, "gen2"), b"5\n");
        wait_finished(&drv, "gen3 mixed");
        assert!(drv.last_retire().carry_abort.is_none());
    }

    /// No declared state behaves exactly as Phase 4 (pure
    /// drain-and-restart): new behavior on the next generation,
    /// nothing carried, no abort recorded.
    #[test]
    fn stateless_body_change_drains_like_phase4() {
        let mut drv = test_driver();
        let gen1 = "fn main() -> int {\n    println(1);\n    return 0;\n}\n";
        let gen2 = "fn main() -> int {\n    println(2);\n    return 0;\n}\n";
        let gen3 = "fn main() -> int {\n    println(2 + 0);\n    return 0;\n}\n";
        spawn_first(&mut drv, gen1, "gen1 stateless");
        let (ended, _) = rebuilt(&mut drv, gen2, &[], "gen2 stateless");
        assert_eq!(ended_stdout(&ended, "gen1"), b"1\n");
        wait_finished(&drv, "gen2 stateless");
        assert!(drv.take_carried().is_empty(), "stateless: nothing carried");
        assert!(drv.last_retire().carry_abort.is_none());
        let (ended, _) = rebuilt(&mut drv, gen3, &[], "gen3 stateless");
        assert_eq!(ended_stdout(&ended, "gen2"), b"2\n");
        wait_finished(&drv, "gen3 stateless");
        assert!(drv.take_carried().is_empty(), "stateless: nothing carried");
        assert!(drv.last_retire().carry_abort.is_none());
    }

    /// Stage 4 soak: ten consecutive preserving reloads over string
    /// state with heap accounting. Every generation prints the
    /// carried value (exact bytes, stream-on-retire) before extending
    /// it; a forced collect at the end reclaims all churn (bounded
    /// live, large reclaimed) while the carried value stays intact —
    /// no leaks, no corruption.
    #[test]
    fn soak_many_preserving_reloads_bounded_heap() {
        const N: usize = 10;
        let mut drv = test_driver();
        // A generation storing length `s` prints the length-(`s`-1)
        // tag it was seeded with. Distinct literals ⇒ body change ⇒
        // the preserving path (never `NoChange`).
        let gen_src = |store: usize| {
            let tag = "a".repeat(store);
            format!(
                "state {{\n    tag: string = \"a\";\n}}\nfn main() -> int {{\n    println(tag);\n    tag = \"{tag}\";\n    return 0;\n}}\n"
            )
        };
        // Per-generation heap accounting: `prune_retired` collects
        // the moved heap with exactly the carried roots on every
        // preserving rebuild, so live must stay flat while the
        // allocation counter proves churn happened (no accumulation
        // = no leak; exact outputs = no corruption).
        fn heap_live<R: RestartPolicy>(drv: &DevDriver<R>) -> (usize, u64) {
            let shared = drv.current_shared.as_ref().expect("shared");
            let st = shared.state.lock().expect("shared state lock");
            let stats = st.heap.stats();
            (stats.objects_live, stats.allocated_total)
        }
        spawn_first(&mut drv, &gen_src(2), "soak gen0");
        let mut max_live = 0usize;
        for i in 1..=N {
            let want = format!("{}\n", "a".repeat(i));
            let (ended, _) = rebuilt(&mut drv, &gen_src(i + 2), &[], "soak reload");
            assert_eq!(
                ended_stdout(&ended, "soak"),
                want.as_bytes(),
                "reload {i} streams the carried tag"
            );
            wait_finished(&drv, "soak reload");
            assert!(
                drv.last_retire().carry_abort.is_none(),
                "soak reload {i} preserves"
            );
            let (live, _) = heap_live(&drv);
            max_live = max_live.max(live);
        }
        // Flush the last generation's own output with one more body
        // tweak, then wait: the heap must be quiescent before reading.
        let want = format!("{}\n", "a".repeat(N + 1));
        let (ended, _) = rebuilt(&mut drv, &gen_src(N + 3), &[], "soak flush");
        assert_eq!(ended_stdout(&ended, "soak"), want.as_bytes());
        wait_finished(&drv, "soak flush");
        let (live, allocated_total) = heap_live(&drv);
        max_live = max_live.max(live);
        // The flush generation was seeded with the length-(N+1) tag.
        let carried = drv.take_carried();
        assert_eq!(carried.len(), 1, "one declared binding carried");
        let live_ref = match &carried[0] {
            RtVal::Heap(r) => *r,
            other => panic!("soak: tag carried as heap value, got {other:?}"),
        };
        {
            let shared = drv.current_shared.as_ref().expect("shared");
            let st = shared.state.lock().expect("shared state lock");
            match st.heap.get(live_ref) {
                Some(toto_runtime::heap::Payload::Str(s)) => {
                    // Seeded from the store=12 generation (the flush
                    // generation stores length N+3 but prints its seed).
                    assert_eq!(s, &"a".repeat(N + 2), "carried tag intact")
                }
                other => panic!("soak: carried tag corrupted: {other:?}"),
            }
        }
        assert!(
            max_live <= 4,
            "soak: heap live stays bounded over {N} reloads (max {max_live})"
        );
        assert!(
            allocated_total >= (N + 2) as u64,
            "soak: churn actually happened (allocated {allocated_total})"
        );
    }
}
