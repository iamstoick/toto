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
use std::time::Duration;

use toto_compiler::hir::CheckedProgram;
use toto_compiler::mir::{MBlock, MInstr, MProgram};

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
