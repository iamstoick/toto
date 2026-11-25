//! Wave 5 Stage 3: statement + branch coverage over the
//! tree-walking interpreter (design approved 2026-11-25).
//!
//! Scope, honestly bounded:
//! - Sequential programs only (`toto run` path). The runtime
//!   executor lowers to MIR, where source mapping is approximate
//!   and counts vary run to run — out of scope (named
//!   follow-up, not silent smoothing).
//! - Coverable units are `Stmt` nodes plus if/while/for arm
//!   outcomes. Tails, state initializers, and migrate bodies are
//!   not statements and never execute here; `go`/`select` hit
//!   the E0215 routing tripwire instead of executing, so they
//!   contribute no branch arms (their statements stay listed
//!   and read zero, which is exactly what happened).
//! - Program stdout is captured and discarded during coverage
//!   runs: LCOV goes to stdout, the human summary to stderr.

use std::collections::{HashMap, HashSet};

use crate::ast::{Block, Expr, ExprKind, ItemKind, SelectCase, Stmt, StmtKind};
use crate::span::Span;

/// Execution counts keyed by source span: statements by their own
/// span, branches by (statement span, arm) with arm 0 = then/body
/// taken and arm 1 = else taken or loop skipped/exited.
#[derive(Default, Clone, Debug)]
pub struct CovHits {
    pub stmts: HashMap<(u32, u32), u64>,
    pub branches: HashMap<(u32, u32, u8), u64>,
    pub fns: HashSet<String>,
}

impl CovHits {
    pub fn hit_stmt(&mut self, span: Span) {
        *self.stmts.entry((span.start, span.end)).or_default() += 1;
    }

    pub fn hit_branch(&mut self, span: Span, arm: u8) {
        *self
            .branches
            .entry((span.start, span.end, arm))
            .or_default() += 1;
    }

    pub fn hit_fn(&mut self, name: &str) {
        self.fns.insert(name.to_string());
    }
}

/// One coverable unit with its origin file (multi-file programs
/// attribute each span to its file at report time).
pub struct CoverPoint {
    pub file: String,
    pub span: Span,
}

/// A branch point: both arms are always reported (0 if never
/// taken), so uncovered arms are visible, not absent.
pub struct BranchPoint {
    pub file: String,
    pub span: Span,
}

/// A named function and where it is declared.
pub struct FnPoint {
    pub file: String,
    pub name: String,
    pub span: Span,
}

pub struct Inventory {
    pub stmts: Vec<CoverPoint>,
    pub branches: Vec<BranchPoint>,
    pub fns: Vec<FnPoint>,
}

/// Static inventory of everything v1 can cover: every `Stmt`,
/// every if/while/for branch point, every named function. Runs
/// over the merged program; files come from item tags.
pub fn collect_inventory(program: &crate::ast::Program) -> Inventory {
    struct Walk<'a> {
        out_stmts: &'a mut Vec<CoverPoint>,
        out_branches: &'a mut Vec<BranchPoint>,
        file: String,
    }
    impl Walk<'_> {
        fn block(&mut self, block: &Block) {
            for stmt in &block.stmts {
                self.stmt(stmt);
            }
            // Tails are expressions, not coverable units — but
            // nested statement positions (closures, ifs) inside
            // them still need walking so executed code is never
            // unattributed.
            if let Some(tail) = &block.tail {
                self.expr(tail);
            }
        }
        fn stmt(&mut self, stmt: &Stmt) {
            let span = stmt.span;
            let file = self.file.clone();
            self.out_stmts.push(CoverPoint {
                file: file.clone(),
                span,
            });
            match &stmt.node {
                StmtKind::Let { value, .. } => self.expr(value),
                StmtKind::Assign { target, value } => {
                    match target {
                        crate::ast::AssignTarget::Var(..) => {}
                        crate::ast::AssignTarget::Field(base, ..)
                        | crate::ast::AssignTarget::Index(base, ..) => {
                            self.expr(base);
                        }
                    }
                    if let crate::ast::AssignTarget::Index(_, index, _) = target {
                        self.expr(index);
                    }
                    self.expr(value);
                }
                StmtKind::Expr(e) | StmtKind::Return(e) => self.expr(e),
                StmtKind::Break | StmtKind::Continue => {}
                StmtKind::While { cond, body } => {
                    self.expr(cond);
                    self.out_branches.push(BranchPoint {
                        file: self.file.clone(),
                        span,
                    });
                    self.block(body);
                }
                StmtKind::For { iter, body, .. } => {
                    self.expr(iter);
                    self.out_branches.push(BranchPoint {
                        file: self.file.clone(),
                        span,
                    });
                    self.block(body);
                }
                // `go`/`select` never execute in this interpreter
                // (E0215 tripwire): their bodies stay listed at zero.
                // Bodies are still walked so nested statements are
                // attributed, not silently dropped.
                StmtKind::Go { body } => self.block(body),
                StmtKind::Select { cases, default } => {
                    for case in cases {
                        match case {
                            SelectCase::Recv { chan, body, .. } => {
                                self.expr(chan);
                                self.block(body);
                            }
                            SelectCase::Send { chan, value, body } => {
                                self.expr(chan);
                                self.expr(value);
                                self.block(body);
                            }
                        }
                    }
                    if let Some(body) = default {
                        self.block(body);
                    }
                }
            }
        }
        fn expr(&mut self, expr: &Expr) {
            match &expr.node {
                ExprKind::If {
                    cond,
                    then_b,
                    else_b,
                } => {
                    self.expr(cond);
                    // `if` in any position (statement or value) is a
                    // branch point: the executor records then/else.
                    let file = self.file.clone();
                    self.out_branches.push(BranchPoint {
                        file,
                        span: expr.span,
                    });
                    self.block(then_b);
                    if let Some(else_b) = else_b {
                        self.block(else_b);
                    }
                }
                ExprKind::Closure { body, .. } => self.block(body),
                ExprKind::StructLit { fields, .. } => {
                    for (_, value, _) in fields {
                        self.expr(value);
                    }
                }
                ExprKind::Array(items) => {
                    for item in items {
                        self.expr(item);
                    }
                }
                ExprKind::Range(a, b)
                | ExprKind::Binary {
                    left: a, right: b, ..
                } => {
                    self.expr(a);
                    self.expr(b);
                }
                ExprKind::Unary { expr, .. } => self.expr(expr),
                ExprKind::Call { callee, args } => {
                    self.expr(callee);
                    for arg in args {
                        self.expr(arg);
                    }
                }
                ExprKind::Index { base, index } => {
                    self.expr(base);
                    self.expr(index);
                }
                ExprKind::Send { chan, value } => {
                    self.expr(chan);
                    self.expr(value);
                }
                ExprKind::Recv { chan } => self.expr(chan),
                ExprKind::Field { base, .. } => self.expr(base),
                ExprKind::Question(inner)
                | ExprKind::Ok(inner)
                | ExprKind::Err(inner)
                | ExprKind::Some(inner) => self.expr(inner),
                ExprKind::ChanNew { cap, .. } => {
                    if let Some(cap) = cap {
                        self.expr(cap);
                    }
                }
                ExprKind::Int(_)
                | ExprKind::Float(_)
                | ExprKind::Str(_)
                | ExprKind::Bool(_)
                | ExprKind::Unit
                | ExprKind::None
                | ExprKind::Var(_) => {}
            }
        }
    }
    let mut stmts = Vec::new();
    let mut branches = Vec::new();
    let mut fns = Vec::new();
    for item in &program.items {
        let file = item.file.clone().unwrap_or_default();
        match &item.kind {
            ItemKind::Fn(decl) => {
                fns.push(FnPoint {
                    file: file.clone(),
                    name: decl.name.clone(),
                    span: decl.name_span,
                });
                let mut walk = Walk {
                    out_stmts: &mut stmts,
                    out_branches: &mut branches,
                    file,
                };
                walk.block(&decl.body);
            }
            _ => {}
        }
    }
    Inventory {
        stmts,
        branches,
        fns,
    }
}

/// LCOV summary numbers for the human line and `--fail-under`.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct CoverageSummary {
    pub lines_found: u64,
    pub lines_hit: u64,
    pub branches_found: u64,
    pub branches_hit: u64,
    pub fns_found: u64,
    pub fns_hit: u64,
}

impl CoverageSummary {
    pub fn line_pct(&self) -> u64 {
        if self.lines_found == 0 {
            100
        } else {
            self.lines_hit * 100 / self.lines_found
        }
    }
}

fn line_of(src: &str, off: u32) -> u32 {
    src.get(..off as usize)
        .map(|prefix| prefix.bytes().filter(|&b| b == b'\n').count() as u32 + 1)
        .unwrap_or(1)
}

/// Render LCOV (`SF`/`FN`/`FNF`/`FNH`/`DA`/`LF`/`LH`/`BRDA`/`BRF`/
/// `BRH`/`end_of_record` per file, entry file first) and compute
/// the summary. Deterministic: files in program order, lines and
/// branch blocks in source order.
pub fn render_lcov(
    files: &[(String, String)],
    inv: &Inventory,
    hits: &CovHits,
) -> (String, CoverageSummary) {
    let mut out = String::new();
    let mut summary = CoverageSummary::default();
    // Per-file sections in the given file order (the driver passes
    // entry first).
    for (path, src) in files.iter() {
        out.push_str(&format!("SF:{path}\n"));
        // Functions declared in this file.
        let mut file_fns: Vec<&FnPoint> = inv.fns.iter().filter(|f| f.file == *path).collect();
        file_fns.sort_by_key(|f| (f.span.start, f.span.end));
        let mut fnh = 0;
        for f in &file_fns {
            let line = line_of(src, f.span.start);
            out.push_str(&format!("FN:{line},{}\n", f.name));
            if hits.fns.contains(&f.name) {
                fnh += 1;
            }
        }
        out.push_str(&format!("FNF:{}\n", file_fns.len()));
        out.push_str(&format!("FNH:{fnh}\n"));
        summary.fns_found += file_fns.len() as u64;
        summary.fns_hit += fnh;
        // Statements grouped by line; hits summed per line.
        let mut by_line: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
        for s in inv.stmts.iter().filter(|s| s.file == *path) {
            let line = line_of(src, s.span.start);
            let h = hits
                .stmts
                .get(&(s.span.start, s.span.end))
                .copied()
                .unwrap_or(0);
            *by_line.entry(line).or_insert(0) += h;
        }
        let mut lf = 0;
        let mut lh = 0;
        for (line, hit) in &by_line {
            lf += 1;
            if *hit > 0 {
                lh += 1;
            }
            out.push_str(&format!("DA:{line},{hit}\n"));
        }
        out.push_str(&format!("LF:{lf}\n"));
        out.push_str(&format!("LH:{lh}\n"));
        summary.lines_found += lf;
        summary.lines_hit += lh;
        // Branches grouped by line; block numbers assigned in
        // source (span) order so identical sources always agree.
        let mut points: Vec<&BranchPoint> =
            inv.branches.iter().filter(|b| b.file == *path).collect();
        points.sort_by_key(|b| (line_of(src, b.span.start), b.span.start, b.span.end));
        let mut brf = 0u64;
        let mut brh = 0u64;
        let mut last_line = 0u32;
        let mut block = 0u32;
        let mut first = true;
        for bp in &points {
            let line = line_of(src, bp.span.start);
            if first || line != last_line {
                block = 0;
                first = false;
                last_line = line;
            } else {
                block += 1;
            }
            for arm in [0u8, 1u8] {
                let h = hits
                    .branches
                    .get(&(bp.span.start, bp.span.end, arm))
                    .copied()
                    .unwrap_or(0);
                brf += 1;
                if h > 0 {
                    brh += 1;
                }
                out.push_str(&format!("BRDA:{line},{block},{arm},{h}\n"));
            }
        }
        out.push_str(&format!("BRF:{brf}\n"));
        out.push_str(&format!("BRH:{brh}\n"));
        summary.branches_found += brf;
        summary.branches_hit += brh;
        out.push_str("end_of_record\n");
    }
    (out, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn workspace_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .to_path_buf()
    }

    /// Self-measurement on a corpus program (Stage 1 acceptance):
    /// the loop/range corpus case runs under coverage in-process.
    /// Its mix — one full loop plus two empty ranges — pins both
    /// taken and skipped branch arms (4/6 branches, 10/12 lines).
    #[test]
    fn corpus_self_measurement() {
        let root = workspace_root();
        let rel = "tests/corpus/interp/loop02_for_range.gzv";
        let src = std::fs::read_to_string(root.join(rel)).expect("corpus readable");
        let (project, checked) = crate::project::load_and_check(
            rel,
            &src,
            &|p| std::fs::read_to_string(root.join(p)).ok(),
            &[],
        )
        .expect("corpus program checks");
        assert!(
            !checked.uses_concurrency,
            "self-measurement target must be sequential"
        );
        let (outcome, hits) = crate::run_program_cov(&project.program, &checked.externs);
        assert!(outcome.0.is_ok(), "self-measurement runs clean");
        let files = vec![(rel.to_string(), src)];
        let inv = collect_inventory(&project.program);
        let (lcov, summary) = render_lcov(&files, &inv, &hits);
        println!(
            "self-measurement {rel}: {}% lines ({}/{}), {}% branches ({}/{}), {}/{} fns",
            summary.line_pct(),
            summary.lines_hit,
            summary.lines_found,
            if summary.branches_found == 0 {
                100
            } else {
                summary.branches_hit * 100 / summary.branches_found
            },
            summary.branches_hit,
            summary.branches_found,
            summary.fns_hit,
            summary.fns_found,
        );
        assert_eq!(
            (summary.lines_hit, summary.lines_found),
            (10, 12),
            "lines:\n{lcov}"
        );
        assert_eq!(
            (summary.branches_hit, summary.branches_found),
            (4, 6),
            "branches:\n{lcov}"
        );
        assert_eq!((summary.fns_hit, summary.fns_found), (1, 1));
        assert!(lcov.contains("SF:tests/corpus/interp/loop02_for_range.gzv\n"));
        assert!(lcov.contains("DA:9,0\n"));
    }
}
