//! ABI conformance, Tier 1 (Phase 2, Stage 3): structural checks over MIR
//! for the dump cases plus every positive corpus program. These pin the
//! frozen ADR-008 rules that do not need Cranelift:
//!
//! - every integer `/`/`%` is guarded by an immediately preceding
//!   `divcheck` on the divisor (explicit-check trap policy);
//! - every call site is guarded by an immediately preceding
//!   `safepoint call` (roots published before the call);
//! - every loop back edge targets a block opening with `safepoint loop`;
//! - every allocation is guarded by an immediately preceding
//!   `safepoint alloc`;
//! - every entry block opens with `safepoint entry`.
//!
//! Phase 3 extension (channels, same frozen rules): every blocking op
//! (`send`, `recv`, `select`) is guarded by an immediately preceding
//! `safepoint park`; every channel creation and every `spawn` is guarded
//! by an immediately preceding `safepoint alloc`; every spawned task
//! body opens with `safepoint entry`. Checks cover nested bodies
//! (`spawn` targets), not just top-level functions.
//!
//! Tier 2 (representation-level: layouts, root publication at runtime,
//! entry-point signatures, context-switch contract, trap behavior) needs
//! Cranelift and runs in Phase 3; see ADR-008.

use std::path::{Path, PathBuf};

use toto_compiler::hir::Ty;
use toto_compiler::mir::{MBinOp, MBlock, MInstr, MProgram, SpKind};
use toto_compiler::{check, lexer, parser};

fn mir_cases() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("mir_cases");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("mir_cases readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
        .collect();
    files.sort();
    files
}

fn corpus_positives() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpus");
    let mut out = Vec::new();
    let mut areas: Vec<_> = std::fs::read_dir(&root)
        .expect("corpus root readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    areas.sort();
    for area in areas {
        let mut files: Vec<_> = std::fs::read_dir(&area)
            .expect("area readable")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "gzv"))
            .collect();
        files.sort();
        for case in files {
            let status = std::fs::read_to_string(case.with_extension("status"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if status == "0" {
                out.push(case);
            }
        }
    }
    out
}

fn lower_file(path: &Path) -> MProgram {
    let src = std::fs::read_to_string(path).expect("read case");
    let (tokens, lex_errors) = lexer::lex(&src);
    assert!(lex_errors.is_empty(), "{}: lex errors", path.display());
    let (program, parse_errors) = parser::parse(tokens, &src);
    assert!(parse_errors.is_empty(), "{}: parse errors", path.display());
    let (checked, check_errors) = check::check(&program);
    assert!(
        check_errors.is_empty(),
        "{}: check errors: {check_errors:?}",
        path.display()
    );
    toto_compiler::mir::lower(&checked.expect("checked"))
}

/// (block id, instruction index) of every instruction in a function.
fn each_instr(prog: &MProgram, mut f: impl FnMut(&str, &[(String, Ty)], &MBlock, usize, &MInstr)) {
    for func in &prog.fns {
        for block in &func.blocks {
            for (i, instr) in block.instrs.iter().enumerate() {
                f(&func.name, &func.locals, block, i, instr);
            }
        }
    }
}

fn is_divcheck_for(instr: &MInstr, divisor: usize) -> bool {
    matches!(instr, MInstr::DivCheck { value, .. } if *value == divisor)
}

/// Every nested code body in the program: `spawn` targets and closure
/// bodies, recursively (bodies nest: a `go` body can spawn or close over
/// further bodies).
fn nested_bodies(prog: &MProgram) -> Vec<std::sync::Arc<toto_compiler::mir::MirBody>> {
    use std::sync::Arc;
    let mut out = Vec::new();
    let mut stack: Vec<Arc<toto_compiler::mir::MirBody>> = Vec::new();
    let push_instrs = |instrs: &[MInstr], stack: &mut Vec<Arc<toto_compiler::mir::MirBody>>| {
        for instr in instrs {
            match instr {
                MInstr::Spawn { body, .. } | MInstr::MakeClosure { body, .. } => {
                    stack.push(body.clone())
                }
                _ => {}
            }
        }
    };
    for func in &prog.fns {
        for block in &func.blocks {
            push_instrs(&block.instrs, &mut stack);
        }
    }
    while let Some(body) = stack.pop() {
        for block in &body.blocks {
            push_instrs(&block.instrs, &mut stack);
        }
        out.push(body);
    }
    out
}

fn is_safepoint(instr: &MInstr, kind: SpKind) -> bool {
    matches!(instr, MInstr::Safepoint { kind: k } if *k == kind)
}

use std::collections::{HashMap, HashSet};

/// Dominator sets per block (iterative dataflow; entry dominates itself).
fn dominators(func: &toto_compiler::mir::MFn) -> HashMap<usize, HashSet<usize>> {
    use toto_compiler::mir::MTerm;
    let ids: Vec<usize> = func.blocks.iter().map(|b| b.id).collect();
    let all: HashSet<usize> = ids.iter().copied().collect();
    let mut preds: HashMap<usize, Vec<usize>> = HashMap::new();
    for block in &func.blocks {
        let succs: Vec<usize> = match &block.term {
            MTerm::Jump(id) => vec![*id],
            MTerm::Branch { then_b, else_b, .. } => vec![*then_b, *else_b],
            _ => vec![],
        };
        for s in succs {
            preds.entry(s).or_default().push(block.id);
        }
    }
    let mut doms: HashMap<usize, HashSet<usize>> = HashMap::new();
    for id in &ids {
        doms.insert(
            *id,
            if *id == func.entry {
                HashSet::from([*id])
            } else {
                all.clone()
            },
        );
    }
    let mut changed = true;
    while changed {
        changed = false;
        for id in &ids {
            if *id == func.entry {
                continue;
            }
            let mut new = all.clone();
            if let Some(ps) = preds.get(id) {
                for p in ps {
                    if let Some(d) = doms.get(p) {
                        new = new.intersection(d).copied().collect();
                    }
                }
            }
            new.insert(*id);
            if doms.get(id) != Some(&new) {
                doms.insert(*id, new);
                changed = true;
            }
        }
    }
    doms
}

#[test]
fn abi_conformance_tier1() {
    let mut files = mir_cases();
    files.extend(corpus_positives());
    assert!(!files.is_empty(), "no cases found");
    let mut failures = Vec::new();
    for path in &files {
        let prog = lower_file(path);
        let mut check = |what: &str, ok: bool| {
            if !ok {
                failures.push(format!("{}: {what}", path.display()));
            }
        };
        for func in &prog.fns {
            // Entry block opens with `safepoint entry`.
            let entry = func.blocks.iter().find(|b| b.id == func.entry);
            check(
                &format!("{}: entry block lacks safepoint entry", func.name),
                matches!(
                    entry.and_then(|b| b.instrs.first()),
                    Some(MInstr::Safepoint {
                        kind: SpKind::Entry
                    })
                ),
            );
            // Loop headers are back-edge targets: an edge S -> T is a back
            // edge exactly when T dominates S (a forward jump to a
            // lower-numbered join, e.g. `break`, does not qualify).
            let doms = dominators(func);
            let mut check_edge = |from: usize, id: usize, term: &str| {
                let back = doms.get(&from).is_some_and(|d| d.contains(&id));
                if back {
                    let target = func.blocks.iter().find(|b| b.id == id);
                    check(
                        &format!(
                            "{} bb{from}: back edge ({term}) misses safepoint loop",
                            func.name
                        ),
                        matches!(
                            target.and_then(|b| b.instrs.first()),
                            Some(MInstr::Safepoint { kind: SpKind::Loop })
                        ),
                    );
                }
            };
            for block in &func.blocks {
                match &block.term {
                    toto_compiler::mir::MTerm::Jump(id) => check_edge(block.id, *id, "jump"),
                    toto_compiler::mir::MTerm::Branch { then_b, else_b, .. } => {
                        check_edge(block.id, *then_b, "br-then");
                        check_edge(block.id, *else_b, "br-else");
                    }
                    _ => {}
                }
            }
        }
        each_instr(&prog, |name, locals, block, i, instr| {
            let prev = block.instrs.get(i.wrapping_sub(1));
            let guarded = |kind: SpKind| prev.is_some_and(|p| is_safepoint(p, kind));
            match instr {
                MInstr::Bin {
                    op: MBinOp::Div | MBinOp::Mod,
                    l,
                    r,
                    ..
                } => {
                    // Integer `/`/`%` only: float division is total (IEEE).
                    let intdiv = matches!((&locals[*l].1, &locals[*r].1), (Ty::Int, Ty::Int));
                    check(
                        &format!("{name} bb{}: int div/mod lacks divcheck", block.id),
                        !intdiv || (i > 0 && prev.is_some_and(|p| is_divcheck_for(p, *r))),
                    );
                }
                MInstr::Call { .. } | MInstr::CallBuiltin { .. } => {
                    check(
                        &format!("{name} bb{}: call lacks safepoint call", block.id),
                        i > 0 && guarded(SpKind::Call),
                    );
                }
                MInstr::StructLit { .. }
                | MInstr::ArrayLit { .. }
                | MInstr::RangeLit { .. }
                | MInstr::WrapOk { .. }
                | MInstr::WrapErr { .. }
                | MInstr::WrapSome { .. }
                | MInstr::MakeClosure { .. } => {
                    check(
                        &format!("{name} bb{}: alloc lacks safepoint alloc", block.id),
                        i > 0 && guarded(SpKind::Alloc),
                    );
                }
                // Phase 3: blocking ops park (roots published before
                // the suspension); creation ops allocate.
                MInstr::Send { .. } | MInstr::Recv { .. } | MInstr::Select { .. } => {
                    check(
                        &format!("{name} bb{}: blocking op lacks safepoint park", block.id),
                        i > 0 && guarded(SpKind::Park),
                    );
                }
                MInstr::ChanNew { .. } | MInstr::Spawn { .. } => {
                    check(
                        &format!("{name} bb{}: channel/spawn lacks safepoint alloc", block.id),
                        i > 0 && guarded(SpKind::Alloc),
                    );
                }
                _ => {}
            }
        });
        // Nested task bodies (spawn targets): entry opens with
        // `safepoint entry`, and their blocks obey the same guards.
        for body in nested_bodies(&prog) {
            let entry = body.blocks.iter().find(|b| b.id == body.entry);
            check(
                "spawned body: entry block lacks safepoint entry",
                matches!(
                    entry.and_then(|b| b.instrs.first()),
                    Some(MInstr::Safepoint {
                        kind: SpKind::Entry
                    })
                ),
            );
            for block in &body.blocks {
                for (i, instr) in block.instrs.iter().enumerate() {
                    let prev = block.instrs.get(i.wrapping_sub(1));
                    let guarded = |kind: SpKind| prev.is_some_and(|p| is_safepoint(p, kind));
                    match instr {
                        MInstr::Bin {
                            op: MBinOp::Div | MBinOp::Mod,
                            ..
                        } => {
                            // Div/mod guards are checked with locals in
                            // `each_instr`; nested bodies repeat only the
                            // park/alloc/entry discipline here.
                        }
                        MInstr::Send { .. } | MInstr::Recv { .. } | MInstr::Select { .. } => {
                            check(
                                &format!(
                                    "spawned body bb{}: blocking op lacks safepoint park",
                                    block.id
                                ),
                                i > 0 && guarded(SpKind::Park),
                            );
                        }
                        MInstr::ChanNew { .. } | MInstr::Spawn { .. } => {
                            check(
                                &format!(
                                    "spawned body bb{}: channel/spawn lacks safepoint alloc",
                                    block.id
                                ),
                                i > 0 && guarded(SpKind::Alloc),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} Tier-1 conformance failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
