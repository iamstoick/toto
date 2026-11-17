//! `toto add` / `toto update` / `toto publish` (Phase 7, Stage 3).
//!
//! - `add` binds a source (`--path`, `--git`, or a `.gzv-pkg`
//!   file), edits `[dependencies]`, resolves with network, writes
//!   `toto.lock`. A bare name (no source) is E0606; an unpinned
//!   git source is E0605 — both BEFORE the manifest is touched.
//! - `update` re-resolves (all, or one subtree) and rewrites the
//!   lock loudly. It is the blessed recovery from lockfile
//!   disagreement (build/test/run never re-resolve silently).
//! - `publish` packs the project into `<name>-<version>.gzv-pkg`
//!   locally. No upload, no registry, no auth.
//!
//! Manifest edits rewrite ONLY the `[dependencies]` table
//! (sorted, regenerated); the `[project]` table is preserved
//! byte-for-byte. Comments inside `[dependencies]` are not kept.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::ExitCode;

use super::manifest::{DepReq, DepSource, Manifest};
use super::packages::{
    Cache, LockedPkg, PkgError, locked_source_key, pack_pkg, read_lockfile, resolve, write_lockfile,
};
use super::semver::{Req, Version};

fn fail(e: PkgError) -> ExitCode {
    eprintln!("{e}");
    ExitCode::from(3)
}

fn manifest_fail(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(3)
}

/// Render one dependency entry in manifest spelling.
fn render_dep(dep: &DepReq) -> String {
    let ver = dep
        .version
        .map(|q| format!(", version = \"{q}\""))
        .unwrap_or_default();
    match &dep.source {
        DepSource::Bare => {
            let q = dep.version.map(|v| v.to_string()).unwrap_or_default();
            format!("\"{q}\"")
        }
        DepSource::Path(p) => format!("{{ path = \"{p}\"{ver} }}"),
        DepSource::Git { url, rev, tag } => {
            let pin = rev
                .as_ref()
                .map(|r| format!("rev = \"{r}\""))
                .or_else(|| tag.as_ref().map(|t| format!("tag = \"{t}\"")))
                .unwrap_or_default();
            format!("{{ git = \"{url}\", {pin}{ver} }}")
        }
    }
}

/// Rewrite only the `[dependencies]` table (sorted); everything
/// before it is preserved byte-for-byte.
fn rewrite_deps_table(text: &str, deps: &BTreeMap<String, DepReq>) -> String {
    let mut head: Vec<&str> = Vec::new();
    let mut skip = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            let h = t.split('#').next().unwrap_or("").trim();
            if h == "[dependencies]" {
                skip = true;
                continue;
            }
            skip = false;
        }
        if !skip {
            head.push(line);
        }
    }
    let mut out = head.join("\n");
    while out.ends_with('\n') || out.ends_with('\r') {
        out.pop();
    }
    out.push('\n');
    if !deps.is_empty() {
        out.push_str("\n[dependencies]\n");
        for (name, dep) in deps {
            out.push_str(&format!("{name} = {}\n", render_dep(dep)));
        }
    }
    out
}

fn write_manifest(root: &Path, manifest: &Manifest) -> Result<(), PkgError> {
    let path = root.join("toto.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| PkgError::new("E0608", format!("cannot read `{}`: {e}", path.display())))?;
    let next = rewrite_deps_table(&text, &manifest.dependencies);
    std::fs::write(&path, next)
        .map_err(|e| PkgError::new("E0608", format!("cannot write `{}`: {e}", path.display())))
}

/// `toto add <name> (--path DIR | --git URL [--rev R | --tag T] | <file.gzv-pkg>) [--version REQ]`
pub fn add(root: &Path, manifest: Manifest, cache: &Cache, args: &[String]) -> ExitCode {
    let mut name: Option<String> = None;
    let mut path_src: Option<String> = None;
    let mut git_src: Option<String> = None;
    let mut rev: Option<String> = None;
    let mut tag: Option<String> = None;
    let mut version: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                i += 1;
                if i >= args.len() {
                    return manifest_fail("toto: `add --path` needs a directory");
                }
                path_src = Some(args[i].clone());
            }
            "--git" => {
                i += 1;
                if i >= args.len() {
                    return manifest_fail("toto: `add --git` needs a URL");
                }
                git_src = Some(args[i].clone());
            }
            "--rev" => {
                i += 1;
                if i >= args.len() {
                    return manifest_fail("toto: `add --rev` needs a revision");
                }
                rev = Some(args[i].clone());
            }
            "--tag" => {
                i += 1;
                if i >= args.len() {
                    return manifest_fail("toto: `add --tag` needs a tag");
                }
                tag = Some(args[i].clone());
            }
            "--version" => {
                i += 1;
                if i >= args.len() {
                    return manifest_fail("toto: `add --version` needs a requirement");
                }
                version = Some(args[i].clone());
            }
            other if other.starts_with("--") => {
                return manifest_fail(&format!("toto: unexpected `add` flag: {other}"));
            }
            other => {
                if name.is_some() {
                    return manifest_fail("toto: `add` takes one package name");
                }
                name = Some(other.to_string());
            }
        }
        i += 1;
    }
    let Some(name) = name else {
        return manifest_fail("toto: `add` needs a package name");
    };
    if name == manifest.name {
        return fail(PkgError::new(
            "E0608",
            format!("package `{name}` has the same name as the project"),
        ));
    }
    let version_req = match version {
        Some(s) => match Req::parse(&s) {
            Ok(q) => Some(q),
            Err(e) => return manifest_fail(&format!("toto: bad `--version`: {e}")),
        },
        None => None,
    };
    // Determine the source BEFORE touching the manifest.
    let dep = if let Some(p) = path_src {
        if git_src.is_some() {
            return manifest_fail("toto: `add --path` and `--git` are exclusive");
        }
        if rev.is_some() || tag.is_some() {
            return manifest_fail("toto: `--rev`/`--tag` need `--git`, not `--path`");
        }
        if p.ends_with(".gzv-pkg") {
            if !root.join(&p).is_file() {
                return manifest_fail(&format!("toto: package file `{p}` not found"));
            }
        } else if !root.join(&p).join("toto.toml").is_file() {
            return manifest_fail(&format!("toto: path dependency `{p}` has no toto.toml"));
        }
        DepReq {
            source: DepSource::Path(p),
            version: version_req,
        }
    } else if let Some(url) = git_src {
        if rev.is_none() && tag.is_none() {
            return fail(PkgError::new(
                "E0605",
                format!(
                    "unpinned git source `{url}`: pin `--rev` or `--tag`; branches and HEAD are never resolved"
                ),
            ));
        }
        if rev.is_some() && tag.is_some() {
            return manifest_fail("toto: `--rev` and `--tag` are exclusive");
        }
        DepReq {
            source: DepSource::Git { url, rev, tag },
            version: version_req,
        }
    } else if name.ends_with(".gzv-pkg") {
        // `toto add foo-1.0.gzv-pkg`: name is the file; the package
        // name comes from the packed manifest (checked below).
        if !root.join(&name).is_file() {
            return manifest_fail(&format!("toto: package file `{name}` not found"));
        }
        DepReq {
            source: DepSource::Path(name.clone()),
            version: version_req,
        }
    } else {
        return fail(PkgError::new(
            "E0606",
            format!(
                "requirement `{name}` names no source: use `--path`, `--git`, or a `.gzv-pkg` file"
            ),
        ));
    };
    // For pkg files the dependency key is the packed name: unpack
    // the name from the manifest inside (validates the file now).
    let key = if matches!(&dep.source, DepSource::Path(p) if p.ends_with(".gzv-pkg")) {
        let file = match &dep.source {
            DepSource::Path(p) => p.clone(),
            _ => unreachable!(),
        };
        let bytes = match std::fs::read(root.join(&file)) {
            Ok(b) => b,
            Err(e) => return manifest_fail(&format!("toto: cannot read `{file}`: {e}")),
        };
        match super::packages::pkg_name_of(&bytes) {
            Ok(n) => n,
            Err(e) => return manifest_fail(&format!("toto: cannot read `{file}`: {e}")),
        }
    } else {
        name.clone()
    };
    if key == manifest.name {
        return fail(PkgError::new(
            "E0608",
            format!("package `{key}` has the same name as the project"),
        ));
    }
    let mut manifest = manifest;
    let replaced = manifest.dependencies.contains_key(&key);
    manifest.dependencies.insert(key.clone(), dep);
    if let Err(e) = write_manifest(root, &manifest) {
        return fail(e);
    }
    // Re-parse (validates what was written) then resolve + lock.
    let path = root.join("toto.toml");
    let manifest = match super::manifest::read_manifest(&path) {
        Ok(m) => m,
        Err(e) => return manifest_fail(&e.message),
    };
    match super::packages::resolve(root, &manifest, cache, true) {
        Ok(res) => {
            if let Err(e) = write_lockfile(root, &res.lock) {
                return fail(e);
            }
            println!(
                "toto: added `{key}`{} ({} packages locked)",
                if replaced { " (replaced)" } else { "" },
                res.lock.len()
            );
            ExitCode::from(0)
        }
        Err(e) => fail(e),
    }
}

/// `toto update [pkg ...]`: full re-resolve, or one subtree each.
pub fn update(root: &Path, manifest: Manifest, cache: &Cache, pkgs: &[String]) -> ExitCode {
    if manifest.dependencies.is_empty() {
        // No deps: named targets are unknown (E0609 first — a
        // typo must not report "up to date"); otherwise the lock
        // must not exist — remove a leftover loudly.
        if let Some(first) = pkgs.first() {
            return fail(PkgError::new(
                "E0609",
                format!("package `{first}` is not in the dependency graph"),
            ));
        }
        let lock = root.join("toto.lock");
        if lock.is_file() {
            match std::fs::remove_file(&lock) {
                Ok(()) => println!("toto: removed stale toto.lock (no dependencies)"),
                Err(e) => {
                    return manifest_fail(&format!("toto: cannot remove stale lock: {e}"));
                }
            }
        } else {
            println!("toto: already up to date (no dependencies)");
        }
        return ExitCode::from(0);
    }
    if pkgs.is_empty() {
        let old = read_lockfile(root).unwrap_or(None).unwrap_or_default();
        let res = match resolve(root, &manifest, cache, true) {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
        report_lock_diff(&old, &res.lock);
        if let Err(e) = write_lockfile(root, &res.lock) {
            return fail(e);
        }
        return ExitCode::from(0);
    }
    // Subtree update: pin everything outside the named subtrees.
    let old = match read_lockfile(root) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return manifest_fail("toto: no toto.lock; run `toto update` (full) first");
        }
        Err(e) => return fail(e),
    };
    // Named packages must exist in the graph (manifest or lock).
    let known: HashSet<&str> = manifest
        .dependencies
        .keys()
        .chain(old.iter().map(|p| &p.name))
        .map(String::as_str)
        .collect();
    for p in pkgs {
        if !known.contains(p.as_str()) {
            return fail(PkgError::new(
                "E0609",
                format!("package `{p}` is not in the dependency graph"),
            ));
        }
    }
    // Pinned resolve: full resolve first for materialized dirs
    // (fetch=true: update may fetch), then walk the live subtree
    // from the named roots through their own manifests, then
    // re-resolve with everything outside the subtree pinned.
    let mut pins: BTreeMap<String, (Version, String)> = BTreeMap::new();
    for p in &old {
        pins.insert(p.name.clone(), (p.version, locked_source_key(&p.source)));
    }
    let first = match resolve(root, &manifest, cache, true) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let mut subtree: HashSet<String> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut walk: Vec<String> = pkgs.to_vec();
    while let Some(cur) = walk.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        subtree.insert(cur.clone());
        if let Some(dir) = first.dirs.get(&cur) {
            let mtoml = dir.join("toto.toml");
            if let Ok(m) = super::manifest::read_manifest(&mtoml) {
                for sub in m.dependencies.keys() {
                    walk.push(sub.clone());
                }
            }
        }
    }
    let res =
        match super::packages::resolve_with_pins(root, &manifest, cache, true, &pins, &subtree) {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
    report_lock_diff(&old, &res.lock);
    if let Err(e) = write_lockfile(root, &res.lock) {
        return fail(e);
    }
    ExitCode::from(0)
}

fn report_lock_diff(old: &[LockedPkg], new: &[LockedPkg]) {
    let oldm: BTreeMap<&str, &LockedPkg> = old.iter().map(|p| (p.name.as_str(), p)).collect();
    let newm: BTreeMap<&str, &LockedPkg> = new.iter().map(|p| (p.name.as_str(), p)).collect();
    let mut changed = false;
    for (name, np) in &newm {
        match oldm.get(name) {
            None => {
                println!("toto: added {name} {}", np.version);
                changed = true;
            }
            Some(op) if **op != **np => {
                if op.version == np.version && op.source == np.source {
                    println!("toto: updated {name} content ({})", np.version);
                } else {
                    println!("toto: changed {name} {} -> {}", op.version, np.version);
                }
                changed = true;
            }
            _ => {} // unchanged
        }
    }
    for name in oldm.keys() {
        if !newm.contains_key(name) {
            println!("toto: removed {name}");
            changed = true;
        }
    }
    if !changed {
        println!("toto: already up to date");
    }
}

/// `toto publish`: pack the project into `<name>-<version>.gzv-pkg`
/// in the project root. Local only: no upload, no registry, no auth.
pub fn publish(root: &Path, manifest: Manifest) -> ExitCode {
    match pack_pkg(root) {
        Ok((fname, bytes)) => {
            let dest = root.join(&fname);
            match std::fs::write(&dest, &bytes) {
                Ok(()) => {
                    println!(
                        "toto: published {} ({} bytes, local only)",
                        fname,
                        bytes.len()
                    );
                    let _ = manifest;
                    ExitCode::from(0)
                }
                Err(e) => manifest_fail(&format!("toto: cannot write `{fname}`: {e}")),
            }
        }
        Err(e) => fail(e),
    }
}
