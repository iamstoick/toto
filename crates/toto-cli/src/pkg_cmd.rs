//! `toto add` / `toto update` / `toto publish` (Phase 7, Stage 3)
//! plus `toto release` / `toto index` (Wave 9: versioned
//! publishing on git remotes the user owns — no central
//! registry, no upload, no auth).
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
//! - `release` cuts the manifest version as git tag `v<version>`
//!   (E0610 family): author-side publishing — the tag IS the
//!   release for git consumers, the pack serves file consumers.
//! - `index` curates `./toto-index.toml` (E0611 family): the
//!   user-owned name→URL registry the resolver consults.
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
                .map(|r| format!(", rev = \"{r}\""))
                .or_else(|| tag.as_ref().map(|t| format!(", tag = \"{t}\"")))
                .unwrap_or_default();
            format!("{{ git = \"{url}\"{pin}{ver} }}")
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

/// `toto add <name> [--version REQ] [--path DIR | --git URL [--rev R | --tag T] | <file.gzv-pkg>]`
/// (bare names resolve through the package index)
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
        if rev.is_none() && tag.is_none() && version_req.is_none() {
            return fail(PkgError::new(
                "E0605",
                format!(
                    "unpinned git source `{url}`: pin `--rev` or `--tag` (or give `--version` to select a tag); branches and HEAD are never resolved"
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
    } else if version_req.is_none() {
        return fail(PkgError::new(
            "E0606",
            format!(
                "requirement `{name}` names no source: add an index entry (`toto-index.toml`) or use `toto add <name> --version REQ | --path DIR | --git URL [--rev R | --tag T]`"
            ),
        ));
    } else {
        // Bare name with a version requirement: resolved through
        // the package index (./toto-index.toml, then
        // ~/.toto/index.toml). Unmapped names fail at resolve
        // time with E0606 naming the index files.
        DepReq {
            source: DepSource::Bare,
            version: version_req,
        }
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

/// Run git in `root`, returning trimmed stdout. Git's own
/// stderr becomes the error text (loud, exact); a missing git
/// binary is a shape error like the resolver's (E0608 there —
/// here E0610, the release family, since these helpers are
/// release-only).
fn git_out(root: &Path, args: &[&str]) -> Result<String, PkgError> {
    let mut cmd = super::packages::git_cmd()
        .map_err(|_| PkgError::new("E0610", "git not found on PATH (release tags a git repo)"))?;
    let out = cmd
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| PkgError::new("E0610", format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(PkgError::new(
            "E0610",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Hash a tag points at, or `None` when the tag does not exist
/// (`-q` keeps a missing tag quiet; repo-ness is established
/// before this is ever called).
fn git_tag_hash(root: &Path, tag: &str) -> Result<Option<String>, PkgError> {
    let mut cmd = super::packages::git_cmd()
        .map_err(|_| PkgError::new("E0610", "git not found on PATH (release tags a git repo)"))?;
    let out = cmd
        .args(["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")])
        .current_dir(root)
        .output()
        .map_err(|e| PkgError::new("E0610", format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
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

/// `toto release [--push]`: cut the manifest version as git tag
/// `v<version>` (Wave 9, errors E0610).
///
/// Author-side publishing for the decentralized registry: the
/// tag IS the release (consumers resolve tags through the
/// index), the pack serves file consumers (same artifact as
/// `publish`). Order is validate-everything → tag → pack, so a
/// failure never leaves a half-release. Nothing is pushed
/// without `--push` (and then exactly one tag, never `--tags`).
pub fn release(root: &Path, manifest: Manifest, push: bool) -> ExitCode {
    let rel_fail = |msg: String| fail(PkgError::new("E0610", msg));
    let version = match Version::parse(&manifest.version) {
        Ok(v) => v,
        Err(e) => {
            return rel_fail(format!(
                "cannot release `{}`: bad version: {e}",
                manifest.name
            ));
        }
    };
    let tag = format!("v{version}");
    // The release tags a repo: the project root must BE the repo
    // root (a subdirectory release would tag the wrong tree).
    let toplevel = match git_out(root, &["rev-parse", "--show-toplevel"]) {
        Ok(t) => t,
        Err(e) => {
            return rel_fail(format!(
                "cannot release: not a git repo ({}): {}",
                root.display(),
                e.message
            ));
        }
    };
    let same_root = std::fs::canonicalize(root)
        .ok()
        .zip(std::fs::canonicalize(toplevel.as_str()).ok())
        .is_some_and(|(a, b)| a == b);
    if !same_root {
        return rel_fail(format!(
            "cannot release: project root `{}` is not the git repo root (release tags the whole repo)",
            root.display()
        ));
    }
    // Clean tree: a release must match a commit, exactly. The
    // release's own pack artifact (`<name>-<version>.gzv-pkg`,
    // same spelling as `pack_pkg`) is exempt: it is derived
    // output rewritten on every release, never source — without
    // the exemption no release could ever be re-run.
    // (Manifest names are charset-constrained and versions are
    // strict numerics, so the artifact name needs no quoting
    // and collides with nothing a user would commit.)
    let own_artifact = format!("{}-{}.gzv-pkg", manifest.name, version);
    let porcelain = match git_out(root, &["status", "--porcelain"]) {
        Ok(p) => p,
        Err(e) => return rel_fail(e.message),
    };
    let dirty: Vec<&str> = porcelain
        .lines()
        .filter(|line| {
            let t = line.trim();
            let rest = t.split_at_checked(3).map(|(_, p)| p).unwrap_or(t);
            !rest.split(" -> ").any(|side| side.trim() == own_artifact)
        })
        .collect();
    if !dirty.is_empty() {
        let files: Vec<&&str> = dirty.iter().take(10).collect();
        let n = dirty.len();
        let more = if n > files.len() {
            format!("\n... and {} more", n - files.len())
        } else {
            String::new()
        };
        let shown: Vec<&str> = files.into_iter().copied().collect();
        return rel_fail(format!(
            "cannot release: working tree not clean ({n} files); commit or stash first:\n{}{more}",
            shown.join("\n")
        ));
    }
    if git_out(root, &["rev-parse", "HEAD"]).is_err() {
        return rel_fail("cannot release: no commits yet; commit first".to_string());
    }
    match git_tag_hash(root, &tag) {
        Ok(Some(_)) => {
            // The tag exists: released already iff the release
            // content is unchanged since the tag. The pack
            // artifact and the project index are exempt — the
            // former is derived output, the latter maps OTHER
            // packages (neither ships in the release). Any other
            // drift means version `version` was already cut from
            // different sources: bump the version or delete the
            // tag by hand. Tags are never moved.
            let diff = match git_out(root, &["diff", "--name-only", &tag, "HEAD"]) {
                Ok(d) => d,
                Err(e) => return rel_fail(e.message),
            };
            let index_name = "toto-index.toml";
            let drifted: Vec<&str> = diff
                .lines()
                .map(str::trim)
                .filter(|p| !p.is_empty() && *p != own_artifact && *p != index_name)
                .collect();
            if drifted.is_empty() {
                println!(
                    "toto: already released {} {version} (tag {tag})",
                    manifest.name
                );
            } else {
                let shown: Vec<&&str> = drifted.iter().take(10).collect();
                let n = drifted.len();
                let more = if n > shown.len() {
                    format!("\n... and {} more", n - shown.len())
                } else {
                    String::new()
                };
                let listed: Vec<&str> = shown.into_iter().copied().collect();
                return rel_fail(format!(
                    "cannot release: tag `{tag}` already exists but {n} release file(s) changed since (bump the version or delete the tag by hand; tags are never moved):\n{}{more}",
                    listed.join("\n")
                ));
            }
        }
        Ok(None) => {
            if let Err(e) = git_out(
                root,
                &[
                    "tag",
                    "-a",
                    &tag,
                    "-m",
                    &format!("toto release {} {version}", manifest.name),
                ],
            ) {
                return rel_fail(e.message);
            }
            println!("toto: released {} {version} (tag {tag})", manifest.name);
        }
        Err(e) => return rel_fail(e.message),
    }
    // Pack for file consumers (same artifact as `publish`).
    match pack_pkg(root) {
        Ok((fname, bytes)) => {
            let dest = root.join(&fname);
            if let Err(e) = std::fs::write(&dest, &bytes) {
                return rel_fail(format!("cannot write `{fname}`: {e}"));
            }
            println!("toto: packed {fname} ({} bytes)", bytes.len());
        }
        Err(e) => return fail(e),
    }
    // Publish step: push exactly the one tag (never `--tags`).
    // The remote is the current branch's upstream; without one,
    // print the command instead of guessing.
    let upstream = git_out(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok();
    let remote = upstream
        .as_deref()
        .and_then(|u| u.split('/').next())
        .filter(|r| !r.is_empty());
    match (push, remote) {
        (true, Some(r)) => match git_out(root, &["push", r, &tag]) {
            Ok(_) => {
                println!("toto: pushed {tag} to {r}");
                ExitCode::from(0)
            }
            Err(e) => rel_fail(e.message),
        },
        (true, None) => rel_fail(
            "cannot push: current branch has no upstream; set one (`git push -u <remote> <branch>`) or push the tag by hand"
                .to_string(),
        ),
        (false, Some(r)) => {
            println!("toto: to publish, run: git push {r} {tag}");
            ExitCode::from(0)
        }
        (false, None) => {
            println!("toto: to publish, run: git push <remote> {tag}");
            ExitCode::from(0)
        }
    }
}

/// Name charset for index entries (Wave 9): exactly the
/// `[packages]` table rule — alphanumerics, `-`, `_`.
fn valid_index_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// URL shape for index entries (Wave 9): non-empty, no
/// whitespace, and shaped like a git remote (`scheme://`,
/// scp-like `user@host:path`, or a local path). Reachability
/// is NOT checked here (offline curation stays possible);
/// the resolver proves the URL at add/update time, loudly.
fn valid_index_url(url: &str) -> bool {
    !url.is_empty()
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        && (url.contains("://") || url.contains('@') || url.contains('/') || url.starts_with('.'))
}

/// `toto index add <name> <url> | remove <name> | list` (Wave 9,
/// errors E0611). Curates the PROJECT index
/// (`./toto-index.toml` — the checked-in, winning file) only;
/// the user file stays hand-managed. No network in any of
/// them.
pub fn index_cmd(root: &Path, args: &[String]) -> ExitCode {
    let idx_fail = |msg: String| fail(PkgError::new("E0611", msg));
    match args.first().map(String::as_str) {
        Some("add") => {
            if args.len() != 3 {
                return idx_fail(
                    "toto: `index add` needs a name and a URL: toto index add <name> <url>"
                        .to_string(),
                );
            }
            let (name, url) = (&args[1], &args[2]);
            if !valid_index_name(name) {
                return idx_fail(format!(
                    "toto: bad index name `{name}` (want alphanumerics, `-`, `_`)"
                ));
            }
            if !valid_index_url(url) {
                return idx_fail(format!(
                    "toto: bad index URL `{url}` (want a git remote: scheme://, user@host:path, or a local path)"
                ));
            }
            let path = super::index::project_index_path(root);
            let project = match super::index::load_project(root) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            if let Some(existing) = project.lookup(name) {
                if existing == url {
                    println!("toto: already indexed {name} (./toto-index.toml)");
                    return ExitCode::from(0);
                }
                return idx_fail(format!(
                    "toto: index entry `{name}` already maps to `{existing}` (not `{url}`); remove it first — entries are never silently rebound"
                ));
            }
            // Parseable files hold ONLY the `[packages]` section,
            // so appending lands inside it by construction.
            let mut text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return idx_fail(format!("cannot read `{}`: {e}", path.display())),
            };
            if text.is_empty() {
                text.push_str("[packages]\n");
            } else if !text.ends_with('\n') {
                text.push('\n');
            }
            // No section check: the strict reader above already
            // rejected anything but `[packages]` (comments and
            // blanks allowed anywhere), so appending at EOF lands
            // inside `[packages]` by construction.
            text.push_str(&format!("{name} = \"{url}\"\n"));
            if let Err(e) = std::fs::write(&path, text) {
                return idx_fail(format!("cannot write `{}`: {e}", path.display()));
            }
            println!("toto: indexed {name} (./toto-index.toml)");
            ExitCode::from(0)
        }
        Some("remove") => {
            if args.len() != 2 {
                return idx_fail(
                    "toto: `index remove` needs a name: toto index remove <name>".to_string(),
                );
            }
            let name = &args[1];
            let path = super::index::project_index_path(root);
            let project = match super::index::load_project(root) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            if project.lookup(name).is_none() {
                return idx_fail(format!("toto: index entry `{name}` not found"));
            }
            // Line surgery: drop the entry line(s), keep
            // comments, blanks, order, and bytes otherwise.
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => return idx_fail(format!("cannot read `{}`: {e}", path.display())),
            };
            let mut kept = Vec::new();
            let mut dropped = 0;
            for raw in text.lines() {
                let t = raw.trim();
                if !t.is_empty()
                    && !t.starts_with('#')
                    && !t.starts_with('[')
                    && let Some((key, _)) = t.split_once('=')
                    && key.trim() == name
                {
                    dropped += 1;
                    continue;
                }
                kept.push(raw);
            }
            if dropped == 0 {
                // Lookup said present but no line matched (only
                // possible on pathological files); loud, no write.
                return idx_fail(format!(
                    "toto: index entry `{name}` not found as a line; refusing to rewrite"
                ));
            }
            let mut next = kept.join("\n");
            if !next.is_empty() {
                next.push('\n');
            }
            if let Err(e) = std::fs::write(&path, next) {
                return idx_fail(format!("cannot write `{}`: {e}", path.display()));
            }
            println!("toto: removed index entry {name}");
            ExitCode::from(0)
        }
        Some("list") => {
            if args.len() != 1 {
                return idx_fail("toto: `index list` takes no arguments".to_string());
            }
            let (project, user) = match super::index::load_pair(root) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            println!("# ./toto-index.toml");
            for (n, u) in project.entries() {
                println!("{n} = \"{u}\"");
            }
            if let Some(p) = super::index::user_index_path() {
                // Missing user file is an empty index: omit the
                // section so `list` stays stable and scriptable.
                if p.is_file() {
                    println!("# {}", p.display());
                    for (n, u) in user.entries() {
                        println!("{n} = \"{u}\"");
                    }
                }
            }
            ExitCode::from(0)
        }
        _ => idx_fail(
            "toto: `index` needs a subcommand: toto index add <name> <url> | remove <name> | list"
                .to_string(),
        ),
    }
}
