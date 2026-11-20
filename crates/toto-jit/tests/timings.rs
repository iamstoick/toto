//! JIT compile-time measurement: compiles every checking corpus
//! program in both modes and asserts each program compiles within a
//! size-relative bound (guards pathological slowdown, not a
//! benchmark). Prints the full per-function table (visible with
//! `-- --nocapture`; `scripts/check.sh` passes it so CI logs carry
//! the numbers for the phase report). Spike 3 toy baseline:
//! 450–950 µs/function (debug).

use std::path::PathBuf;

use toto_compiler::{mir, parse_and_check};
use toto_jit::Jit;

/// Size-relative bound, per compiled program and mode: a fixed cost
/// per function plus a rate per MIR instruction (nested closure/go
/// bodies included). Calibrated 2026-11-16 on a Zen 3 desktop
/// (debug): ~0.5–0.7ms fixed overhead per function (context setup,
/// verifier baseline) with linear scaling at ~30–60µs per emitted
/// CLIF instruction; heap-heavy programs emit ~10–20 CLIF per MIR,
/// so the rate covers the densest observed scaffolding with the
/// worst program at ~50% of its budget. Catches >=2× regressions and
/// orders-of-magnitude pathology; re-fit if the corpus or toolchain
/// changes materially (a flat per-function bound cannot fit both
/// 5-instruction bodies and 80-instruction programs).
const FIXED_PER_FN_MICROS: u64 = 8_000;
const PER_MIR_INSTR_MICROS: u64 = 2_200;

/// Total MIR instructions in a program, including nested closure and
/// `go` bodies (each compiles to its own function, so each counts
/// toward the program's compile cost).
fn mir_size(prog: &mir::MProgram) -> usize {
    fn blocks_size(blocks: &[mir::MBlock]) -> usize {
        blocks
            .iter()
            .map(|blk| {
                blk.instrs.len()
                    + blk
                        .instrs
                        .iter()
                        .map(|i| match i {
                            mir::MInstr::MakeClosure { body, .. }
                            | mir::MInstr::Spawn { body, .. } => blocks_size(&body.blocks),
                            _ => 0,
                        })
                        .sum::<usize>()
            })
            .sum()
    }
    prog.fns.iter().map(|f| blocks_size(&f.blocks)).sum()
}

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

#[test]
fn jit_compile_timings() {
    let root = corpus_root();
    let mut areas: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("corpus root")
        .map(|e| e.expect("area entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    let mut gate_compiled = 0u32;
    let mut task_compiled = 0u32;
    let mut task_refused_net = 0u32;
    let mut failures = Vec::new();
    println!("program,mode,function,micros");
    for area in areas {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&area)
            .expect("area dir")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
            .collect();
        files.sort();
        for case in files {
            let name = case
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            let src = std::fs::read_to_string(&case).expect("read case");
            let checked = match parse_and_check(&src) {
                Ok((_, checked)) => checked,
                Err(_) => continue,
            };
            let prog = mir::lower(&checked);
            let size = mir_size(&prog) as u64;
            // Gate mode covers sequential programs only (concurrency
            // is refused, by design).
            if !checked.uses_concurrency {
                let mut jit = Jit::new().expect("gate session");
                match jit.compile(&prog) {
                    Ok(()) => {
                        let mut total = 0u64;
                        let mut nfns = 0u64;
                        for (f, us) in &jit.timings {
                            println!("{name},gate,{f},{us}");
                            total += *us;
                            nfns += 1;
                        }
                        let budget = FIXED_PER_FN_MICROS * nfns + PER_MIR_INSTR_MICROS * size;
                        if total > budget {
                            failures.push(format!(
                                "{name}: gate total {total}µs over budget {budget}µs ({nfns} fns, {size} MIR)"
                            ));
                        }
                        gate_compiled += 1;
                    }
                    Err(e) => failures.push(format!("{name}: gate compile refused: {e}")),
                }
            }
            let mut jit = Jit::new_task().expect("task session");
            match jit.compile_task(&prog) {
                Ok(_) => {
                    let mut total = 0u64;
                    let mut nfns = 0u64;
                    for (f, us) in &jit.timings {
                        println!("{name},task,{f},{us}");
                        total += *us;
                        nfns += 1;
                    }
                    let budget = FIXED_PER_FN_MICROS * nfns + PER_MIR_INSTR_MICROS * size;
                    if total > budget {
                        failures.push(format!(
                            "{name}: task total {total}µs over budget {budget}µs ({nfns} fns, {size} MIR)"
                        ));
                    }
                    task_compiled += 1;
                }
                Err(e) => {
                    // Network waits need the executor poller
                    // (spec §17.5): the task JIT refuses them by
                    // design. Count the skip deliberately; any
                    // other refusal is still a failure.
                    if format!("{e}").contains("needs the M:N poller") {
                        task_refused_net += 1;
                    } else {
                        failures.push(format!("{name}: task compile refused: {e}"))
                    }
                }
            }
        }
    }
    // Exact census (160 cases: 47 sequential checking incl. diag10
    // (state01-05 joined in Phase 6; diag16_ffi_array_sig joined in
    // Wave 1), 22 concurrent (conc19_chan_eq joined in Phase 5;
    // state06-07 joined in Phase 6; conc20_net_hammer joined in
    // Wave 1 but refused by the task JIT — needs the M:N poller —
    // counted in `task_refused_net`), 91 static
    // (type37_index_range + diag13-15 FFI joined in Phase 5;
    // 22 Phase-6 negatives joined in Phase 6;
    // diag17_ffi_nonbyte_array joined in Wave 1)).
    assert_eq!(gate_compiled, 47, "gate-compiled count changed");
    assert_eq!(task_compiled, 68, "task-compiled count changed");
    assert_eq!(task_refused_net, 1, "task net-refusal count changed");
    assert!(
        failures.is_empty(),
        "{} timing/compile failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
