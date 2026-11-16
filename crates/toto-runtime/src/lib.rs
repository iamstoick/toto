//! Production Toto runtime (Phase 3): GC heap, corosensei scheduler,
//! channels, and the MIR executor.
//!
//! The `spikes/` prototypes stay untouched as reference; this crate is
//! the production line. Stage 1 is single-threaded cooperative with an
//! `Arc`-based ownership design that is sound under M:N from the start
//! (no `Rc`/`RefCell` anywhere here).

pub mod channel;
pub mod conv;
pub mod exec;
pub mod heap;
pub mod measure;
pub mod roots;
pub mod sched;
pub mod value;

use std::collections::HashMap;
use std::sync::Arc;

use toto_compiler::ast::{FnDecl, ItemKind};
use toto_compiler::diag::Diag;
use toto_compiler::hir::{CheckedProgram, HStructDecl};
use toto_compiler::interp::TraceFrame;
use toto_compiler::span::Span;

use sched::{ExecCode, RunStats, SchedOutcome, Scheduler};

/// Runtime configuration. Defaults are documented in the Phase 3
/// report; tests use tiny thresholds to force frequent collections.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeConfig {
    /// Collect when `bytes_since_gc` reaches this at a safepoint.
    pub gc_threshold_bytes: u64,
    /// Seed for the `select` tie-break RNG (deterministic mode).
    pub rng_seed: u64,
    /// Fixed coroutine stack size (spec: 64 KiB with guard pages).
    pub stack_bytes: usize,
    /// TEST-ONLY hook: when true, every allocation/switch safepoint
    /// runs a full collection regardless of the byte threshold (the
    /// `gc_collect_for_test` equivalent for the production runtime).
    /// Never set in shipped code paths; the CLI leaves it false.
    pub collect_at_every_safepoint_for_test: bool,
    /// Worker threads for the M:N scheduler (Stage 2). `1` is the
    /// deterministic single-threaded mode: one FIFO queue, fixed-seed
    /// RNG, exact goldens. Higher values enable work-stealing across
    /// OS threads (genuinely nondeterministic interleavings: use
    /// invariant assertions, not exact goldens).
    pub workers: usize,
    /// Dedicated blocking-pool threads for file-system builtins
    /// (Stage 2 policy: no worker ever stalls on IO).
    pub blocking_pool_threads: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            gc_threshold_bytes: 1 << 20,
            rng_seed: 0x9E37_79B9_7F4A_7C15,
            stack_bytes: 64 * 1024,
            collect_at_every_safepoint_for_test: false,
            workers: 1,
            blocking_pool_threads: 2,
        }
    }
}

/// What running a checked program produced (same shape as the
/// compiler's `Outcome` pieces so the CLI renders identically).
pub struct RtOutcome {
    pub result: Result<i32, (Diag, Vec<TraceFrame>)>,
    pub stdout: Vec<u8>,
    pub stats: RunStats,
}

/// Run a checked concurrent program to completion: lower to MIR, drive
/// all tasks on coroutine stacks under the scheduler, on the GC heap.
/// Runs on a dedicated large-stack host thread (like the interpreters).
/// Never panics on user programs: every failure surfaces in `result`.
pub fn run_checked(
    program: &toto_compiler::ast::Program,
    checked: &CheckedProgram,
    config: RuntimeConfig,
) -> RtOutcome {
    let prog = toto_compiler::mir::lower(checked);
    let mut structs: HashMap<String, HStructDecl> = HashMap::new();
    for (name, decl) in &checked.structs {
        structs.insert(name.clone(), decl.clone());
    }
    let mut fns: Vec<FnDecl> = Vec::new();
    for item in &program.items {
        if let ItemKind::Fn(decl) = &item.kind {
            fns.push(decl.clone());
        }
    }
    let fn_blocks: Vec<Arc<[toto_compiler::mir::MBlock]>> = prog
        .fns
        .iter()
        .map(|f| Arc::from(f.blocks.as_slice()))
        .collect();
    let main_idx = match prog.fn_index.get("main") {
        Some(&i) => i,
        None => {
            return RtOutcome {
                result: Err((
                    Diag::new(
                        "E0204",
                        "program has no `fn main() -> int`".to_string(),
                        Span::new(0, 0),
                        None,
                    ),
                    Vec::new(),
                )),
                stdout: Vec::new(),
                stats: RunStats {
                    tasks_spawned: 0,
                    switches: 0,
                    steals: 0,
                    high_water_max: 0,
                    collections: 0,
                    allocated: 0,
                    reclaimed: 0,
                },
            };
        }
    };
    let main_span = prog.main_span;
    let code = Arc::new(ExecCode {
        prog,
        structs,
        fns,
        fn_blocks,
        externs: checked.externs.clone(),
    });
    // Test-isolation hygiene (mirrors `Interp::run`): the callback
    // queue is process-global, so discard stale firings before tasks
    // start. No callback can legitimately be live yet.
    for (slot, _) in toto_compiler::ffi::drain_queue() {
        toto_compiler::ffi::release_slot(slot);
    }
    let outcome = std::thread::Builder::new()
        .name("toto-runtime".to_string())
        .stack_size(256 << 20)
        .spawn(move || {
            let sched = Scheduler::new(
                code,
                config.gc_threshold_bytes,
                config.rng_seed,
                config.stack_bytes,
                config.collect_at_every_safepoint_for_test,
                config.workers,
                config.blocking_pool_threads,
            );
            sched.spawn_main(main_idx, main_span);
            sched.run()
        })
        .expect("failed to spawn runtime thread")
        .join()
        .expect("runtime thread failed");
    compose(outcome, main_span)
}

/// Compose a scheduler outcome into a run outcome (shared by the
/// MIR-executor path and the Phase 4 JIT path: identical exit-code,
/// failure-propagation, and stats semantics by construction).
pub fn compose(outcome: SchedOutcome, main_span: Span) -> RtOutcome {
    let SchedOutcome {
        main,
        root_failure,
        stats,
        output,
    } = outcome;
    // A failed scope re-panics with the first panic after all children
    // settle (spec §13.2), even when `main` itself returned normally.
    if let Some(f) = root_failure {
        return RtOutcome {
            result: Err((f.diag, f.trace)),
            stdout: output,
            stats,
        };
    }
    match main {
        exec::TaskResult::Return(crate::value::RtVal::Int(n)) if (0..=255).contains(&n) => {
            RtOutcome {
                result: Ok(n as i32),
                stdout: output,
                stats,
            }
        }
        exec::TaskResult::Return(crate::value::RtVal::Int(n)) => RtOutcome {
            result: Err((
                Diag::new(
                    "E0204",
                    format!("main returned {n}, which is not a valid exit code 0-255"),
                    main_span,
                    None,
                ),
                Vec::new(),
            )),
            stdout: output,
            stats,
        },
        exec::TaskResult::Return(other) => {
            let tn = match other {
                crate::value::RtVal::Float(_) => "float",
                crate::value::RtVal::Bool(_) => "bool",
                crate::value::RtVal::Unit => "unit",
                crate::value::RtVal::None => "Option",
                crate::value::RtVal::Range(..) => "Range",
                crate::value::RtVal::Heap(_) => "heap value",
                crate::value::RtVal::Chan(_) => "chan",
                crate::value::RtVal::Func(_) | crate::value::RtVal::Closure(_) => "function",
                crate::value::RtVal::Ok(_) | crate::value::RtVal::Err(_) => "Result",
                crate::value::RtVal::Some(_) => "Option",
                crate::value::RtVal::Ptr(_) => "pointer",
                crate::value::RtVal::Int(_) => "int",
            };
            RtOutcome {
                result: Err((
                    Diag::new(
                        "E0204",
                        format!("main must return an int, not {tn}"),
                        main_span,
                        None,
                    ),
                    Vec::new(),
                )),
                stdout: output,
                stats,
            }
        }
        exec::TaskResult::Panic { diag, trace } => RtOutcome {
            result: Err((diag, trace)),
            stdout: output,
            stats,
        },
    }
}
