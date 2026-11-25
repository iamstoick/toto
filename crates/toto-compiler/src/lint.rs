//! Wave 5 Stage 1: hygiene lints (design approved 2026-11-24).
//!
//! Two tiers, matching the `toto lint` driver contract:
//!
//! - Tier A (`lint_file`) is purely syntactic over one parsed file:
//!   E0501 flags statements (and block tails) after a statement that
//!   always exits its block (`return`/`break`/`continue`); E0502 flags
//!   local bindings that are never read (`let`, `for` variables,
//!   `select` receive bindings, function and closure parameters).
//!   Bindings starting with `_` are exempt: the documented way to say
//!   "intentionally unused". Tier A needs no type information, so it
//!   also covers library files, which fail the `fn main` check and
//!   never reach Tier B.
//! - Tier B (`unused_imports`) needs the merged, checked program:
//!   E0503 flags a listed file's own import edge when nothing reachable
//!   through it is referenced. Files carrying `state`/`migrate` items
//!   are exempt: loading them runs initializers.
//!
//! Findings reuse `Diag` (stable code, span, help) and render as
//! warnings; findings never claim more than the analysis proves.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    AssignTarget, Block, Expr, ExprKind, FnDecl, ItemKind, Program, SelectCase, StmtKind, TypeKind,
};
use crate::diag::Diag;
use crate::project::{DepEntry, Project, resolve_import_display};

/// Tier A hygiene for one parsed file: unreachable statements (E0501)
/// and unused bindings (E0502). Runs on the file's own AST, so library
/// files lint the same as programs.
pub fn lint_file(program: &Program) -> Vec<Diag> {
    let mut out = Vec::new();
    for item in &program.items {
        if let ItemKind::Fn(decl) = &item.kind {
            unreachable_in_block(&decl.body, &mut out);
            let mut uses = UseScopes::default();
            uses.walk_fn(decl, &mut out);
        }
    }
    out.sort_by_key(|d| (d.span.start, d.span.end));
    out
}

fn unreachable_in_block(block: &Block, out: &mut Vec<Diag>) {
    let mut dead = false;
    for stmt in &block.stmts {
        if dead {
            out.push(Diag::new(
                "E0501",
                "unreachable statement: a previous statement in this block always exits"
                    .to_string(),
                stmt.span,
                Some("remove it".to_string()),
            ));
        }
        unreachable_in_stmt(&stmt.node, out);
        if matches!(
            stmt.node,
            StmtKind::Return(_) | StmtKind::Break | StmtKind::Continue
        ) {
            dead = true;
        }
    }
    if dead && let Some(tail) = &block.tail {
        out.push(Diag::new(
            "E0501",
            "unreachable expression: a previous statement in this block always exits"
                .to_string(),
            tail.span,
            Some("remove it".to_string()),
        ));
    }
}

fn unreachable_in_stmt(stmt: &StmtKind, out: &mut Vec<Diag>) {
    match stmt {
        StmtKind::Let { value, .. } => unreachable_in_expr(value, out),
        StmtKind::Assign { target, value } => {
            match target {
                AssignTarget::Var(..) => {}
                AssignTarget::Field(base, ..) => unreachable_in_expr(base, out),
                AssignTarget::Index(base, index, ..) => {
                    unreachable_in_expr(base, out);
                    unreachable_in_expr(index, out);
                }
            }
            unreachable_in_expr(value, out);
        }
        StmtKind::Expr(e) | StmtKind::Return(e) => unreachable_in_expr(e, out),
        StmtKind::Break | StmtKind::Continue => {}
        StmtKind::While { cond, body } => {
            unreachable_in_expr(cond, out);
            unreachable_in_block(body, out);
        }
        StmtKind::For { iter, body, .. } => {
            unreachable_in_expr(iter, out);
            unreachable_in_block(body, out);
        }
        StmtKind::Go { body } => unreachable_in_block(body, out),
        StmtKind::Select { cases, default } => {
            for case in cases {
                match case {
                    SelectCase::Recv { chan, body, .. } => {
                        unreachable_in_expr(chan, out);
                        unreachable_in_block(body, out);
                    }
                    SelectCase::Send { chan, value, body } => {
                        unreachable_in_expr(chan, out);
                        unreachable_in_expr(value, out);
                        unreachable_in_block(body, out);
                    }
                }
            }
            if let Some(body) = default {
                unreachable_in_block(body, out);
            }
        }
    }
}

fn unreachable_in_expr(expr: &Expr, out: &mut Vec<Diag>) {
    match &expr.node {
        ExprKind::If { then_b, else_b, .. } => {
            unreachable_in_block(then_b, out);
            if let Some(else_b) = else_b {
                unreachable_in_block(else_b, out);
            }
        }
        ExprKind::Closure { body, .. } => unreachable_in_block(body, out),
        _ => {}
    }
}

/// One declared binding: span, whether it was read, whether an
/// unread binding is reported (parameters and loop variables are
/// tracked for shadowing but never flagged).
type DeclEntry = (crate::span::Span, bool, bool);

/// Lexical-scope use tracker: one frame per block, innermost first.
/// Shadowing is legal (spec §5.6), so each name maps to a stack: a use
/// marks the innermost live declaration.
#[derive(Default)]
struct UseScopes {
    frames: Vec<HashMap<String, Vec<DeclEntry>>>,
}

impl UseScopes {
    fn walk_fn(&mut self, decl: &FnDecl, out: &mut Vec<Diag>) {
        self.frames.push(HashMap::new());
        // Parameters are interface conformance (handlers, callbacks):
        // tracked for shadowing, never flagged.
        for param in &decl.params {
            self.declare(&param.name, param.name_span, false);
        }
        self.walk_block(&decl.body, &[], out);
        self.pop(out);
    }

    fn walk_block(
        &mut self,
        block: &Block,
        extra: &[(String, crate::span::Span, bool)],
        out: &mut Vec<Diag>,
    ) {
        self.frames.push(HashMap::new());
        for (name, span, report) in extra {
            self.declare(name, *span, *report);
        }
        for stmt in &block.stmts {
            self.walk_stmt(stmt, out);
        }
        if let Some(tail) = &block.tail {
            self.walk_expr(tail, out);
        }
        self.pop(out);
    }

    fn walk_stmt(&mut self, stmt: &crate::ast::Stmt, out: &mut Vec<Diag>) {
        match &stmt.node {
            StmtKind::Let {
                name,
                name_span,
                value,
                ..
            } => {
                // The initializer sees the OUTER scope: `let x = x + 1`
                // reads the shadowed binding.
                self.walk_expr(value, out);
                self.declare(name, *name_span, true);
            }
            StmtKind::Assign { target, value } => {
                match target {
                    AssignTarget::Var(..) => {}
                    AssignTarget::Field(base, ..) => self.walk_expr(base, out),
                    AssignTarget::Index(base, index, ..) => {
                        self.walk_expr(base, out);
                        self.walk_expr(index, out);
                    }
                }
                self.walk_expr(value, out);
            }
            StmtKind::Expr(e) | StmtKind::Return(e) => self.walk_expr(e, out),
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::While { cond, body } => {
                self.walk_expr(cond, out);
                self.walk_block(body, &[], out);
            }
            StmtKind::For {
                name,
                name_span,
                iter,
                body,
            } => {
                self.walk_expr(iter, out);
                // Loop variables are idiomatic counters: tracked for
                // shadowing, never flagged.
                self.walk_block(body, &[(name.clone(), *name_span, false)], out);
            }
            StmtKind::Go { body } => self.walk_block(body, &[], out),
            StmtKind::Select { cases, default } => {
                for case in cases {
                    match case {
                        SelectCase::Recv { name, chan, body } => {
                            self.walk_expr(chan, out);
                            match name {
                                Some((n, s)) => {
                                    self.walk_block(body, &[(n.clone(), *s, true)], out)
                                }
                                None => self.walk_block(body, &[], out),
                            }
                        }
                        SelectCase::Send { chan, value, body } => {
                            self.walk_expr(chan, out);
                            self.walk_expr(value, out);
                            self.walk_block(body, &[], out);
                        }
                    }
                }
                if let Some(body) = default {
                    self.walk_block(body, &[], out);
                }
            }
        }
    }

    fn walk_expr(&mut self, expr: &Expr, out: &mut Vec<Diag>) {
        match &expr.node {
            ExprKind::Var(name) => self.mark_used(name),
            ExprKind::If {
                cond,
                then_b,
                else_b,
            } => {
                self.walk_expr(cond, out);
                self.walk_block(then_b, &[], out);
                if let Some(else_b) = else_b {
                    self.walk_block(else_b, &[], out);
                }
            }
            ExprKind::Closure { params, body, .. } => {
                let extra: Vec<(String, crate::span::Span, bool)> = params
                    .iter()
                    .map(|p| (p.name.clone(), p.name_span, false))
                    .collect();
                self.walk_block(body, &extra, out);
            }
            ExprKind::StructLit { fields, .. } => {
                for (_, value, _) in fields {
                    self.walk_expr(value, out);
                }
            }
            ExprKind::Array(items) => {
                for item in items {
                    self.walk_expr(item, out);
                }
            }
            ExprKind::Range(a, b) => {
                self.walk_expr(a, out);
                self.walk_expr(b, out);
            }
            ExprKind::Binary { left, right, .. } => {
                self.walk_expr(left, out);
                self.walk_expr(right, out);
            }
            ExprKind::Unary { expr, .. } => self.walk_expr(expr, out),
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee, out);
                for arg in args {
                    self.walk_expr(arg, out);
                }
            }
            ExprKind::Ok(inner) | ExprKind::Err(inner) | ExprKind::Some(inner) => {
                self.walk_expr(inner, out)
            }
            ExprKind::Send { chan, value } => {
                self.walk_expr(chan, out);
                self.walk_expr(value, out);
            }
            ExprKind::Recv { chan } => self.walk_expr(chan, out),
            ExprKind::ChanNew { cap, .. } => {
                if let Some(cap) = cap {
                    self.walk_expr(cap, out);
                }
            }
            ExprKind::Question(inner) => self.walk_expr(inner, out),
            ExprKind::Field { base, .. } => self.walk_expr(base, out),
            ExprKind::Index { base, index } => {
                self.walk_expr(base, out);
                self.walk_expr(index, out);
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit
            | ExprKind::None => {}
        }
    }

    fn declare(&mut self, name: &str, span: crate::span::Span, report: bool) {
        if let Some(frame) = self.frames.last_mut() {
            frame
                .entry(name.to_string())
                .or_default()
                .push((span, false, report));
        }
    }

    fn mark_used(&mut self, name: &str) {
        for frame in self.frames.iter_mut().rev() {
            if let Some(stack) = frame.get_mut(name) {
                if let Some(entry) = stack.last_mut() {
                    entry.1 = true;
                }
                return;
            }
        }
    }

    fn pop(&mut self, out: &mut Vec<Diag>) {
        if let Some(frame) = self.frames.pop() {
            let mut stale: Vec<(crate::span::Span, String)> = Vec::new();
            for (name, stack) in &frame {
                if name.starts_with('_') {
                    continue;
                }
                for (span, used, report) in stack {
                    if !used && *report {
                        stale.push((*span, name.clone()));
                    }
                }
            }
            stale.sort_by_key(|(span, _)| (span.start, span.end));
            for (span, name) in stale {
                out.push(Diag::new(
                    "E0502",
                    format!("unused binding `{name}`: never read"),
                    span,
                    Some("remove it or prefix it with `_`".to_string()),
                ));
            }
        }
    }
}

/// Tier B: unused imports (E0503) over a successfully loaded project.
///
/// For each import written in the entry file, resolve it exactly as the
/// loader would; when the target file is provably unneeded — none of
/// its items is referenced anywhere and it carries no `state`/`migrate`
/// items (whose loading runs initializers) — the edge is dead.
/// Need flows transitively: a file that only conduits to a needed file
/// is itself needed. Unresolvable edges are skipped, never flagged.
pub fn unused_imports(
    project: &Project,
    read: &dyn Fn(&str) -> Option<String>,
    deps: &[DepEntry],
) -> Vec<Diag> {
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for (i, file) in project.files.iter().enumerate() {
        index_of.insert(file.path.as_str(), i);
    }
    // Every file parsed clean or the load would have failed; a file
    // that cannot be re-parsed here keeps its imports unknown, which
    // marks it needed below (conservative: fewer findings).
    let mut direct: Vec<Vec<String>> = Vec::with_capacity(project.files.len());
    let mut unparseable = HashSet::new();
    for (i, file) in project.files.iter().enumerate() {
        let (tokens, lex_errors) = crate::lexer::lex(&file.src);
        if !lex_errors.is_empty() {
            unparseable.insert(i);
            direct.push(Vec::new());
            continue;
        }
        let (program, parse_errors) = crate::parser::parse(tokens, &file.src);
        if !parse_errors.is_empty() {
            unparseable.insert(i);
            direct.push(Vec::new());
            continue;
        }
        direct.push(
            program
                .items
                .iter()
                .filter_map(|item| match &item.kind {
                    ItemKind::Import(decl) => Some(decl.path.clone()),
                    _ => None,
                })
                .collect(),
        );
    }
    // Top-level names referenced anywhere in the merged program.
    // Over-approximate (every Var occurrence counts): missing a
    // reference would be a false positive, adding one only a miss.
    let mut referenced: HashSet<String> = HashSet::new();
    for item in &project.program.items {
        collect_refs_item(item, &mut referenced);
    }
    // Per-file facts for the need computation.
    let mut fns: Vec<HashSet<String>> = vec![HashSet::new(); project.files.len()];
    let mut structs: Vec<HashSet<String>> = vec![HashSet::new(); project.files.len()];
    let mut effect: Vec<bool> = vec![false; project.files.len()];
    for item in &project.program.items {
        let idx = item
            .file
            .as_deref()
            .and_then(|p| index_of.get(p).copied())
            .unwrap_or(project.entry);
        match &item.kind {
            ItemKind::Fn(decl) => {
                fns[idx].insert(decl.name.clone());
            }
            ItemKind::Struct(decl) => {
                structs[idx].insert(decl.name.clone());
            }
            ItemKind::State(_) | ItemKind::Migrate(_) => {
                effect[idx] = true;
            }
            ItemKind::Import(_) | ItemKind::Extern(_) => {}
        }
    }
    // Resolve child edges; a file with an unresolvable edge is needed.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); project.files.len()];
    let mut unresolved = HashSet::new();
    for (i, file) in project.files.iter().enumerate() {
        if unparseable.contains(&i) {
            unresolved.insert(i);
            continue;
        }
        for path in &direct[i] {
            match resolve_import_display(&file.path, path, read, deps)
                .and_then(|disp| index_of.get(disp.as_str()).copied())
            {
                Some(idx) => children[i].push(idx),
                None => {
                    unresolved.insert(i);
                }
            }
        }
    }
    let mut need = NeedCtx {
        fns: &fns,
        structs: &structs,
        effect: &effect,
        children: &children,
        referenced: &referenced,
        unresolved: &unresolved,
        memo: HashMap::new(),
    };
    // Entry imports from the entry source with their spans.
    let entry_src = &project.files[project.entry].src;
    let (tokens, _) = crate::lexer::lex(entry_src);
    let (entry_program, _) = crate::parser::parse(tokens, entry_src);
    let mut out = Vec::new();
    for item in &entry_program.items {
        if let ItemKind::Import(decl) = &item.kind {
            let disp =
                resolve_import_display(&project.files[project.entry].path, &decl.path, read, deps);
            let idx = disp.and_then(|d| index_of.get(d.as_str()).copied());
            match idx {
                Some(idx) if !need.needed(idx, &mut Vec::new()) => {
                    out.push(Diag::new(
                        "E0503",
                        format!(
                            "unused import `{}`: nothing reachable through it is used",
                            decl.path
                        ),
                        decl.path_span,
                        Some("remove the import".to_string()),
                    ));
                }
                _ => {}
            }
        }
    }
    out.sort_by_key(|d: &Diag| (d.span.start, d.span.end));
    out
}

/// Program-wide reachability facts for the need computation.
struct NeedCtx<'a> {
    fns: &'a [HashSet<String>],
    structs: &'a [HashSet<String>],
    effect: &'a [bool],
    children: &'a [Vec<usize>],
    referenced: &'a HashSet<String>,
    unresolved: &'a HashSet<usize>,
    memo: HashMap<usize, bool>,
}

impl NeedCtx<'_> {
    fn needed(&mut self, idx: usize, visiting: &mut Vec<usize>) -> bool {
        if let Some(known) = self.memo.get(&idx) {
            return *known;
        }
        if visiting.contains(&idx) {
            return true;
        }
        // Conservative: an unresolvable edge or a file whose own
        // imports are unknown keeps the file.
        if self.unresolved.contains(&idx) {
            self.memo.insert(idx, true);
            return true;
        }
        if self.effect[idx]
            || self.fns[idx].iter().any(|n| self.referenced.contains(n))
            || self.structs[idx].iter().any(|n| self.referenced.contains(n))
        {
            self.memo.insert(idx, true);
            return true;
        }
        visiting.push(idx);
        let mut keep = false;
        for child in &self.children[idx] {
            if self.needed(*child, visiting) {
                keep = true;
                break;
            }
        }
        visiting.pop();
        self.memo.insert(idx, keep);
        keep
    }
}

fn collect_refs_item(item: &crate::ast::Item, referenced: &mut HashSet<String>) {
    match &item.kind {
        ItemKind::Fn(decl) => {
            let mut refs = RefCollector { out: referenced };
            refs.collect_block(&decl.body);
        }
        ItemKind::Struct(decl) => {
            for field in &decl.fields {
                collect_refs_type(&field.ty, referenced);
            }
        }
        ItemKind::State(decl) => {
            for binding in &decl.bindings {
                collect_refs_type(&binding.ty, referenced);
                if let Some(init) = &binding.init {
                    let mut refs = RefCollector { out: referenced };
                    refs.collect_expr(init);
                }
            }
        }
        ItemKind::Migrate(decl) => {
            let mut refs = RefCollector { out: referenced };
            refs.collect_block(&decl.func.body);
        }
        ItemKind::Import(_) => {}
        ItemKind::Extern(block) => {
            for decl in &block.decls {
                for param in &decl.params {
                    collect_refs_type(&param.ty, referenced);
                }
                collect_refs_type(&decl.ret, referenced);
            }
        }
    }
}

fn collect_refs_type(ty: &crate::ast::Type, referenced: &mut HashSet<String>) {
    match &ty.kind {
        TypeKind::Named { name, args } => {
            referenced.insert(name.clone());
            for arg in args {
                collect_refs_type(arg, referenced);
            }
        }
        TypeKind::Ptr(inner) => collect_refs_type(inner, referenced),
        TypeKind::Fn { params, ret } => {
            for param in params {
                collect_refs_type(param, referenced);
            }
            collect_refs_type(ret, referenced);
        }
    }
}

struct RefCollector<'a> {
    out: &'a mut HashSet<String>,
}

impl RefCollector<'_> {
    fn collect_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.collect_stmt(&stmt.node);
        }
        if let Some(tail) = &block.tail {
            self.collect_expr(tail);
        }
    }

    fn collect_stmt(&mut self, stmt: &StmtKind) {
        match stmt {
            StmtKind::Let { value, .. } => self.collect_expr(value),
            StmtKind::Assign { target, value } => {
                match target {
                    AssignTarget::Var(..) => {}
                    AssignTarget::Field(base, ..) => self.collect_expr(base),
                    AssignTarget::Index(base, index, ..) => {
                        self.collect_expr(base);
                        self.collect_expr(index);
                    }
                }
                self.collect_expr(value);
            }
            StmtKind::Expr(e) | StmtKind::Return(e) => self.collect_expr(e),
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::While { cond, body } => {
                self.collect_expr(cond);
                self.collect_block(body);
            }
            StmtKind::For { iter, body, .. } => {
                self.collect_expr(iter);
                self.collect_block(body);
            }
            StmtKind::Go { body } => self.collect_block(body),
            StmtKind::Select { cases, default } => {
                for case in cases {
                    match case {
                        SelectCase::Recv { chan, body, .. } => {
                            self.collect_expr(chan);
                            self.collect_block(body);
                        }
                        SelectCase::Send { chan, value, body } => {
                            self.collect_expr(chan);
                            self.collect_expr(value);
                            self.collect_block(body);
                        }
                    }
                }
                if let Some(body) = default {
                    self.collect_block(body);
                }
            }
        }
    }

    fn collect_expr(&mut self, expr: &Expr) {
        match &expr.node {
            ExprKind::Var(name) => {
                self.out.insert(name.clone());
            }
            ExprKind::StructLit { name, fields } => {
                self.out.insert(name.clone());
                for (_, value, _) in fields {
                    self.collect_expr(value);
                }
            }
            ExprKind::If {
                cond,
                then_b,
                else_b,
            } => {
                self.collect_expr(cond);
                self.collect_block(then_b);
                if let Some(else_b) = else_b {
                    self.collect_block(else_b);
                }
            }
            ExprKind::Closure { body, .. } => self.collect_block(body),
            ExprKind::Array(items) => {
                for item in items {
                    self.collect_expr(item);
                }
            }
            ExprKind::Range(a, b) => {
                self.collect_expr(a);
                self.collect_expr(b);
            }
            ExprKind::Binary { left, right, .. } => {
                self.collect_expr(left);
                self.collect_expr(right);
            }
            ExprKind::Unary { expr, .. } => self.collect_expr(expr),
            ExprKind::Call { callee, args } => {
                self.collect_expr(callee);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            ExprKind::Ok(inner) | ExprKind::Err(inner) | ExprKind::Some(inner) => {
                self.collect_expr(inner)
            }
            ExprKind::Send { chan, value } => {
                self.collect_expr(chan);
                self.collect_expr(value);
            }
            ExprKind::Recv { chan } => self.collect_expr(chan),
            ExprKind::ChanNew { cap, .. } => {
                if let Some(cap) = cap {
                    self.collect_expr(cap);
                }
            }
            ExprKind::Question(inner) => self.collect_expr(inner),
            ExprKind::Field { base, .. } => self.collect_expr(base),
            ExprKind::Index { base, index } => {
                self.collect_expr(base);
                self.collect_expr(index);
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit
            | ExprKind::None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .to_path_buf()
    }

    fn collect_gzv(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries: Vec<_> = std::fs::read_dir(dir)
            .expect("corpus dir readable")
            .map(|e| e.expect("dir entry").path())
            .collect();
        let mut entries = entries;
        entries.sort();
        for path in entries {
            if path.is_dir() {
                collect_gzv(&path, out);
            } else if path.extension().is_some_and(|e| e == "gzv") {
                out.push(path);
            }
        }
    }

    fn golden_status(path: &Path) -> Option<String> {
        let status_path = path.with_extension("status");
        std::fs::read_to_string(status_path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Tier-A corpus-clean proof (Stage 1 acceptance): every parseable
    /// repo program that is not a known error-golden lints silent.
    /// Exclusions live in `tests/lint-exceptions.txt` (shared with the
    /// check.sh lint gate): each is a TRUE positive on intentional
    /// scaffolding, never a rule bug, with its reason recorded there.
    /// Broken support fixtures are excluded there too: they are not
    /// lintable programs (covered by the loader/corpus tests).
    #[test]
    fn corpus_tier_a_clean() {
        let root = workspace_root();
        let exceptions_src = std::fs::read_to_string(root.join("tests/lint-exceptions.txt"))
            .expect("lint exceptions readable");
        let excluded: Vec<String> = exceptions_src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
        assert!(
            !excluded.is_empty(),
            "lint exceptions must not be empty (the sweep is the evidence)"
        );
        let mut files = Vec::new();
        for area in ["tests/corpus", "stdlib", "bench", "apps"] {
            collect_gzv(&root.join(area), &mut files);
        }
        let mut parsed = 0usize;
        let mut skipped_unparseable = Vec::new();
        let mut skipped_error_golden = 0usize;
        let mut failures = Vec::new();
        for file in &files {
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(file)
                .to_string_lossy()
                .into_owned();
            if excluded.iter().any(|e| rel == *e) {
                continue;
            }
            // Known error-goldens are not style-clean exemplars.
            if file.starts_with(root.join("tests/corpus")) {
                match golden_status(file).as_deref() {
                    Some("0") | None => {}
                    _ => {
                        skipped_error_golden += 1;
                        continue;
                    }
                }
            }
            let src = std::fs::read_to_string(file).expect("gzv readable");
            let (tokens, lex_errors) = crate::lexer::lex(&src);
            if !lex_errors.is_empty() {
                skipped_unparseable.push(rel);
                continue;
            }
            let (program, parse_errors) = crate::parser::parse(tokens, &src);
            if !parse_errors.is_empty() {
                skipped_unparseable.push(rel);
                continue;
            }
            parsed += 1;
            let hits = lint_file(&program);
            if !hits.is_empty() {
                let codes: Vec<_> = hits.iter().map(|d| d.code).collect();
                failures.push(format!("{rel}: {codes:?}"));
            }
        }
        println!(
            "tier-a sweep: {parsed} parsed files linted, {} skipped error-golden, {} skipped unparseable, {} with findings",
            skipped_error_golden,
            skipped_unparseable.len(),
            failures.len()
        );
        for skip in &skipped_unparseable {
            println!("  unparseable: {skip}");
        }
        // Floor against a vacuous pass: the sweep must actually cover
        // the corpus (today 146 parsed + 9 reasoned exclusions + 108
        // error-goldens). The printed breakdown above is the evidence.
        assert!(parsed >= 100, "tier-a sweep covered only {parsed} files");
        assert!(
            failures.is_empty(),
            "tier-a findings on clean files:\n{}",
            failures.join("\n")
        );
    }
}
