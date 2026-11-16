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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use toto_compiler::ast::Program;
use toto_compiler::hir::CheckedProgram;
use toto_compiler::mir::{MBlock, MInstr, MProgram};
use toto_compiler::{parse_and_check, render_panic};
use toto_runtime::{RtOutcome, RuntimeConfig};

use crate::compile::Jit;
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
    ) -> Result<Self, String> {
        let mir = toto_compiler::mir::lower(&checked);
        let mut jit = Jit::new_task().map_err(|e| e.to_string())?;
        let session = jit.compile_task(&mir).map_err(|e| e.to_string())?;
        Ok(Self {
            number,
            src,
            filename,
            program,
            checked,
            jit,
            session,
        })
    }

    /// Spawn the generation's run in the background. The returned
    /// handle owns nothing of the generation; the caller keeps the
    /// generation alive until the run is joined (the driver does).
    /// `Err` carries the immediate outcome when no run starts (same
    /// shape as `spawn_jit_run`; kept unboxed for call-site ergonomics).
    #[allow(clippy::result_large_err)]
    pub fn spawn(&self, config: RuntimeConfig) -> Result<Box<dyn LiveRun>, RtOutcome> {
        match spawn_jit_run(&self.program, &self.checked, config, &self.session) {
            Ok(run) => Ok(Box::new(run)),
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

/// The `toto dev` state machine: generations, drain-vs-cancel
/// rebuild, respawn. Owns no files and prints nothing; the CLI owns
/// the watcher, the loop, and all output.
pub struct DevDriver<R: RestartPolicy = ReexecRestart> {
    policy: DrainPolicy,
    restart: R,
    config: RuntimeConfig,
    next_gen: u64,
    current: Option<ActiveGeneration>,
}

impl<R: RestartPolicy> DevDriver<R> {
    /// New driver: drain policy, restart policy, and the scheduler
    /// config every spawned generation runs under.
    pub fn new(policy: DrainPolicy, restart: R, config: RuntimeConfig) -> Self {
        Self {
            policy,
            restart,
            config,
            next_gen: 1,
            current: None,
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
                            SettleOutcome::Restarted => RebuildOutcome::Restarted,
                            drained_or_cancelled => {
                                match next.spawn(self.config) {
                                    Ok(run) => {
                                        self.current = Some(ActiveGeneration {
                                            compiled: next,
                                            run,
                                        });
                                        self.next_gen = number + 1;
                                        RebuildOutcome::Rebuilt {
                                            ended,
                                            settle: Some(drained_or_cancelled),
                                        }
                                    }
                                    Err(outcome) => {
                                        // Spawn fails only when no run
                                        // starts at all (missing main:
                                        // checked code cannot do this).
                                        // Retire the old run's output
                                        // anyway; wait for the next
                                        // change.
                                        let ended = Some(next.render(&outcome));
                                        self.next_gen = number + 1;
                                        RebuildOutcome::Rebuilt {
                                            ended,
                                            settle: Some(drained_or_cancelled),
                                        }
                                    }
                                }
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
        ) {
            Err(e) => RebuildOutcome::CompileRefused(format!("generation {number}: {e}")),
            Ok(built) => match built.spawn(self.config) {
                Err(outcome) => {
                    self.next_gen = number + 1;
                    RebuildOutcome::Rebuilt {
                        ended: Some(built.render(&outcome)),
                        settle: None,
                    }
                }
                Ok(run) => {
                    self.current = Some(ActiveGeneration {
                        compiled: built,
                        run,
                    });
                    self.next_gen = number + 1;
                    RebuildOutcome::Rebuilt {
                        ended: None,
                        settle: None,
                    }
                }
            },
        }
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
