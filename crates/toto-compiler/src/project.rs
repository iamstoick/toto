//! Multi-file projects: import resolution, merged checking (Phase 5).
//!
//! A project is an entry file plus every file reachable through
//! `import "path";` statements. Import paths resolve lexically against
//! the importing file's directory (no filesystem canonicalization, so
//! disk and embedded/ARO loads share the exact same semantics).
//! Files load depth-first in import order; the entry file's items come
//! first in the merged program. The merged program (imports stripped,
//! items tagged with their origin file) goes through the existing
//! `check()` unchanged, so all name rules — including cross-file
//! duplicate `E0311` — apply with no special cases. Diagnostics carry
//! their origin file for rendering.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::ast::{Item, ItemKind, Program};
use crate::check::check_with_files;
use crate::diag::Diag;
use crate::hir::CheckedProgram;
use crate::lexer::lex;
use crate::parser::parse;
use crate::span::Span;

/// One loaded source file.
#[derive(Clone, Debug)]
pub struct ProjectFile {
    /// Display path used in diagnostics: the entry path as given for the
    /// entry file; imports relative to the working directory when the
    /// resolved path sits under it, else the resolved path.
    pub path: String,
    pub src: String,
}

/// A loaded project: files in DFS pre-order (entry first), the merged
/// program (imports stripped, items tagged with their file), and a
/// function-name → file-index map for runtime panic attribution.
/// Function names are unique program-wide (the merged check rejects
/// duplicates with `E0311`), so the map is exact.
#[derive(Clone, Debug)]
pub struct Project {
    pub files: Vec<ProjectFile>,
    pub entry: usize,
    pub program: Program,
    pub fn_files: HashMap<String, usize>,
}

/// One declared dependency for import resolution: `import
/// "<name>/rest"` loads `<dir>/<rest>` (Phase 7, Stage 3).
/// `dir` is an absolute package directory (display path); dep
/// files are stored under their resolution string (never
/// relativized), so embedded table loads (AOT binaries) resolve
/// identically at build and at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepEntry {
    pub name: String,
    pub dir: String,
}

impl Project {
    /// File index owning Toto function `name` (entry fallback for
    /// synthetic frames).
    pub fn file_of_fn(&self, name: &str) -> usize {
        self.fn_files.get(name).copied().unwrap_or(self.entry)
    }
}

/// Static diagnostics for one project file.
#[derive(Clone, Debug)]
pub struct FileDiags {
    pub file: usize,
    pub diags: Vec<Diag>,
}

/// A failed project load: the files that did load (for rendering)
/// plus the per-file diagnostics.
#[derive(Clone, Debug)]
pub struct ProjectErrors {
    pub files: Vec<ProjectFile>,
    pub errors: Vec<FileDiags>,
}

/// Load an entry file plus its transitive imports, merge, and check.
///
/// `entry_src` is the already-read entry source (reading the entry is
/// the caller's job: an unreadable entry is a CLI usage error, not a
/// diagnostic). `read` loads any other resolved path and returns `None`
/// when it does not exist (disk: `std::fs::read_to_string().ok()`;
/// embedded: table lookup). The entry display path is `entry_path`
/// exactly as given. `deps` maps dependency names to package
/// directories; an empty slice is exactly today's behavior (no
/// package can match, so every import resolves as before).
pub fn load_and_check(
    entry_path: &str,
    entry_src: &str,
    read: &dyn Fn(&str) -> Option<String>,
    deps: &[DepEntry],
) -> Result<(Project, CheckedProgram), ProjectErrors> {
    let mut loader = Loader::new(read, deps);
    loader.load_entry(entry_path, entry_src);
    if loader.diags.iter().any(|d| !d.is_empty()) {
        let mut errors = Vec::new();
        for (file, diags) in loader.diags.into_iter().enumerate() {
            if !diags.is_empty() {
                errors.push(FileDiags { file, diags });
            }
        }
        return Err(ProjectErrors {
            files: loader.files,
            errors,
        });
    }
    // Merge: entry file first (DFS pre-order), imports stripped, items
    // tagged with their origin display path.
    let mut items = Vec::new();
    let mut fn_files = HashMap::new();
    for (idx, file) in loader.files.iter().enumerate() {
        let prog = loader.programs[idx].clone();
        for mut item in prog.items {
            if matches!(item.kind, ItemKind::Import(_)) {
                continue;
            }
            if let ItemKind::Fn(decl) = &item.kind {
                fn_files.entry(decl.name.clone()).or_insert(idx);
            }
            item.file = Some(file.path.clone());
            items.push(item);
        }
    }
    // Merged programs have no single module doc (each file keeps
    // its own `Program.doc` in `self.programs`); per-module tools
    // like `toto doc` read files individually.
    let merged = Program {
        items,
        doc: Vec::new(),
    };
    let (checked, pairs) = check_with_files(&merged);
    if !pairs.is_empty() {
        let by_display: HashMap<&str, usize> = loader
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.as_str(), i))
            .collect();
        // Deterministic order: file index, then diagnostic order.
        let mut per_file: Vec<Vec<Diag>> = vec![Vec::new(); loader.files.len()];
        for (file, d) in pairs {
            let idx = file
                .as_deref()
                .and_then(|p| by_display.get(p).copied())
                .unwrap_or(loader.entry);
            per_file[idx].push(d);
        }
        let mut errors = Vec::new();
        for (idx, diags) in per_file.into_iter().enumerate() {
            if !diags.is_empty() {
                errors.push(FileDiags { file: idx, diags });
            }
        }
        return Err(ProjectErrors {
            files: loader.files,
            errors,
        });
    }
    let checked = checked.expect("no errors implies a checked program");
    let project = Project {
        files: loader.files,
        entry: loader.entry,
        program: merged,
        fn_files,
    };
    Ok((project, checked))
}

/// Lexically normalize a path string (resolve `.`/`..`, drop redundant
/// separators). Purely lexical: no filesystem access, identical for
/// disk and embedded loads.
fn normalize(path: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut absolute = false;
    let mut prefix: Option<String> = None;
    for comp in Path::new(path).components() {
        match comp {
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if out.last().is_some_and(|s| s != "..") {
                    out.pop();
                } else if !absolute {
                    out.push("..".to_string());
                }
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    let mut s = String::new();
    if let Some(p) = prefix {
        s.push_str(&p);
    }
    if absolute {
        s.push('/');
    }
    s.push_str(&out.join("/"));
    if s.is_empty() {
        s.push('.');
    }
    s
}

/// A resolved import: either a plain path (today's behavior) or a
/// dependency file (first segment names a declared dependency).
enum Resolution {
    Plain(String),
    Dep { dir: String, rest: String },
}

/// Resolve an import path against the importing file's display path.
/// Both display paths stay in the same (relative or absolute) space.
///
/// A `gzv.std/`-prefixed import names a standard-library module and
/// resolves canonically (no directory join), so stdlib imports work
/// from any file in any directory. The read callback serves these
/// from the embedded `stdlib` table (disk fallback for development).
/// `gzv.std/` is checked FIRST and is reserved: no dependency can
/// shadow it (dep names cannot contain `/` or `.`).
///
/// Otherwise, when the import's first segment names a declared
/// dependency (and a remainder follows), the import resolves into
/// the package directory. Relative `./` and `../` imports never
/// match (their first segment is `.`/`..`, never a dep name), and
/// a bare `import "<dep>"` with no remainder resolves plainly.
fn resolve(importer_display: &str, import: &str, deps: &[DepEntry]) -> Resolution {
    if import.starts_with("gzv.std/") {
        return Resolution::Plain(normalize(import));
    }
    if let Some((first, rest)) = import.split_once('/')
        && !rest.is_empty()
        && let Some(dep) = deps.iter().find(|d| d.name == first)
    {
        return Resolution::Dep {
            dir: dep.dir.clone(),
            rest: rest.to_string(),
        };
    }
    Resolution::Plain(plain_resolve(importer_display, import))
}

/// Today's joining resolution (importing-file-relative), shared
/// by the plain path and the dep-miss fallback.
fn plain_resolve(importer_display: &str, import: &str) -> String {
    let dir = Path::new(importer_display)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let joined = if dir.is_empty() {
        PathBuf::from(import)
    } else {
        Path::new(&dir).join(import)
    };
    normalize(&joined.to_string_lossy())
}

/// Resolve one import to the display path the loader would file it
/// under, or `None` when it would not load (missing file, ambiguous
/// dependency). Lint-only helper: Tier B maps each listed import to
/// its file with the exact loader semantics (dep rule with the plain
/// fallback, Amendment-1 ambiguity, no canonicalization). A `None`
/// result means "assume used", never a finding.
pub fn resolve_import_display(
    importer_display: &str,
    import: &str,
    read: &dyn Fn(&str) -> Option<String>,
    deps: &[DepEntry],
) -> Option<String> {
    match resolve(importer_display, import, deps) {
        Resolution::Dep { dir, rest } => {
            let dep_path = normalize(&format!("{dir}/{rest}"));
            if read(&dep_path).is_none() {
                let plain = plain_resolve(importer_display, import);
                return read(&plain).and(option_display(&plain));
            }
            if read(&normalize(import)).is_some() {
                return None;
            }
            Some(dep_path)
        }
        Resolution::Plain(resolved) => read(&resolved).and(option_display(&resolved)),
    }
}

fn option_display(resolved: &str) -> Option<String> {
    Some(display(resolved))
}

/// Display a resolved path: relative to the working directory when it
/// sits under it, else the resolved path as-is.
fn display(resolved: &str) -> String {
    let path = Path::new(resolved);
    let abs;
    let full = if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => {
                abs = cwd.join(path);
                &abs
            }
            Err(_) => path,
        }
    };
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = full.strip_prefix(&cwd)
        && !rel.as_os_str().is_empty()
    {
        return rel.to_string_lossy().into_owned();
    }
    resolved.to_string()
}

struct Loader<'a> {
    read: &'a dyn Fn(&str) -> Option<String>,
    deps: Vec<DepEntry>,
    files: Vec<ProjectFile>,
    programs: Vec<Program>,
    /// Resolved-path identity → file index (deduplicates diamonds).
    index: HashMap<String, usize>,
    /// Identities on the current DFS chain (cycle detection).
    stack: Vec<String>,
    entry: usize,
    diags: Vec<Vec<Diag>>,
}

impl<'a> Loader<'a> {
    fn new(read: &'a dyn Fn(&str) -> Option<String>, deps: &[DepEntry]) -> Self {
        Self {
            read,
            deps: deps.to_vec(),
            files: Vec::new(),
            programs: Vec::new(),
            index: HashMap::new(),
            stack: Vec::new(),
            entry: 0,
            diags: Vec::new(),
        }
    }

    fn load_entry(&mut self, entry_path: &str, entry_src: &str) {
        let resolved = normalize(entry_path);
        let disp = entry_path.to_string();
        self.push_file(disp, entry_src.to_string());
        self.entry = 0;
        self.stack.push(resolved.clone());
        self.index.insert(resolved, 0);
        self.parse_current(0);
        // Recovered programs still carry their imports, so resolution
        // proceeds best-effort; any file's diagnostics abort before check.
        let imports = import_paths(&self.programs[0]);
        self.load_imports(0, &imports);
        self.stack.pop();
    }

    fn push_file(&mut self, path: String, src: String) {
        self.files.push(ProjectFile { path, src });
        // Placeholder; replaced by `parse_current`.
        self.programs.push(Program {
            items: Vec::new(),
            doc: Vec::new(),
        });
        self.diags.push(Vec::new());
    }

    fn parse_current(&mut self, idx: usize) {
        let src = self.files[idx].src.clone();
        let (tokens, lex_errors) = lex(&src);
        let mut diags: Vec<Diag> = lex_errors
            .into_iter()
            .map(|e| Diag::new(e.code, e.message, e.span, e.help))
            .collect();
        // Lex errors abort parsing (same as single-file `parse_and_check`):
        // the placeholder empty program contributes no imports.
        if diags.is_empty() {
            let (program, parse_errors) = parse(tokens, &src);
            diags.extend(parse_errors);
            self.programs[idx] = program;
        }
        self.diags[idx] = diags;
    }

    fn load_imports(&mut self, importer: usize, imports: &[(String, Span)]) {
        for (path, path_span) in imports {
            match resolve(&self.files[importer].path, path, &self.deps) {
                Resolution::Dep { dir, rest } => {
                    self.load_dep_import(importer, path, *path_span, &dir, &rest)
                }
                Resolution::Plain(resolved) => {
                    self.load_plain_import(importer, path, *path_span, &resolved)
                }
            }
        }
    }

    /// Dependency import (Amendment 1 enforced here): the dep file
    /// wins only when the same literal path does NOT also exist
    /// from the project root. Both existing is a loud E0601 naming
    /// the dependency and the root path — never a silent shadow in
    /// either direction. A missing dep file falls back to the plain
    /// path (today's E0401 behavior when nothing exists).
    fn load_dep_import(
        &mut self,
        importer: usize,
        path: &str,
        path_span: Span,
        dir: &str,
        rest: &str,
    ) {
        let dep_path = normalize(&format!("{dir}/{rest}"));
        let Some(src) = (self.read)(&dep_path) else {
            // No such file in the package: plain importer-relative
            // resolution (the import may still work, or E0401).
            let plain = plain_resolve(&self.files[importer].path, path);
            self.load_plain_import(importer, path, path_span, &plain);
            return;
        };
        // Amendment 1: does the literal path also resolve from the
        // project root (the caller's read anchors root-relative
        // loads, so probing the literal path is the root check)?
        if (self.read)(&normalize(path)).is_some() {
            let dep_name = path.split('/').next().unwrap_or(path);
            self.diags[importer].push(Diag::new(
                "E0601",
                format!(
                    "ambiguous import `{path}`: matches both dependency `{dep_name}` and the project-root path `{}`; rename one"
                    ,normalize(path)),
                path_span,
                None,
            ));
            return;
        }
        // Dep files keep their resolution string as identity (never
        // relativized): build-time table keys match runtime
        // resolution exactly, on any working directory.
        if self.stack.contains(&dep_path) {
            self.diags[importer].push(Diag::new(
                "E0402",
                format!("circular import of `{path}`"),
                path_span,
                None,
            ));
            return;
        }
        if self.index.contains_key(&dep_path) {
            return;
        }
        self.push_file(dep_path.clone(), src);
        let idx = self.files.len() - 1;
        self.index.insert(dep_path.clone(), idx);
        self.stack.push(dep_path);
        self.parse_current(idx);
        let nested = import_paths(&self.programs[idx]);
        self.load_imports(idx, &nested);
        self.stack.pop();
    }

    fn load_plain_import(&mut self, importer: usize, path: &str, path_span: Span, resolved: &str) {
        if self.stack.contains(&resolved.to_string()) {
            self.diags[importer].push(Diag::new(
                "E0402",
                format!("circular import of `{path}`"),
                path_span,
                None,
            ));
            return;
        }
        if self.index.contains_key(resolved) {
            return;
        }
        let Some(src) = (self.read)(resolved) else {
            self.diags[importer].push(Diag::new(
                "E0401",
                format!("cannot resolve `import \"{path}\"`: file not found"),
                path_span,
                None,
            ));
            return;
        };
        let disp = display(resolved);
        self.push_file(disp, src);
        let idx = self.files.len() - 1;
        self.index.insert(resolved.to_string(), idx);
        self.stack.push(resolved.to_string());
        self.parse_current(idx);
        let nested = import_paths(&self.programs[idx]);
        self.load_imports(idx, &nested);
        self.stack.pop();
    }
}

/// `(path, path_span)` of every top-level import in a parsed program.
fn import_paths(program: &Program) -> Vec<(String, crate::span::Span)> {
    let mut out = Vec::new();
    for item in &program.items {
        if let Item {
            kind: ItemKind::Import(decl),
            ..
        } = item
        {
            out.push((decl.path.clone(), decl.path_span));
        }
    }
    out
}
