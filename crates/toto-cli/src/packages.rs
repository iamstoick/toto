//! Package resolution, lockfile, sources, and cache (Phase 7, Stage 3).
//!
//! Approved design + amendments (see the Phase 7 report):
//! `[dependencies]` in `toto.toml`; SemVer `^`/`~`/`=`/bare;
//! single-version rule (highest satisfying, deterministic);
//! `toto.lock` (exact versions, checked in, ONLY source of
//! versions at build time); path + git + `.gzv-pkg` sources, no
//! registry; first-segment resolution with the Amendment 1
//! ambiguity error; local-only `publish`; E06xx codes.
//!
//! Network rule (pinned): fetching happens at add/update time
//! (and lock creation) ONLY. A locked git dependency resolves
//! from the LOCAL CACHE at build time; a missing locked rev
//! fails loudly naming the rev — never a silent build-time
//! fetch. Path dependencies read live (always fresh).
//!
//! Integrity hashes are FNV-1a/64 (`fnv1a64:…`): change
//! detection, explicitly NOT cryptographic.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::index::Indexes;
use super::manifest::{DepReq, DepSource, Manifest};
use super::semver::{Req, Version};

/// A package failure: loud CLI error, exit 3, E06xx code carried
/// in the message (manifest-shaped errors keep their existing
/// line-numbered form; resolution failures have no `.gzv` span).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkgError {
    pub code: &'static str,
    pub message: String,
}

impl PkgError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        PkgError {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for PkgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "toto: {}: {}", self.code, self.message)
    }
}

/// One locked package: exact version + source + content identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedPkg {
    pub name: String,
    pub version: Version,
    pub source: LockedSource,
    /// `fnv1a64:` hex over the package's `.gzv` files (sorted).
    pub files_hash: String,
}

/// Pinned source (lockfile spelling).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockedSource {
    /// Directory as spelled in the manifest (root-relative or abs).
    Path(String),
    /// Immutable revision hash (tags resolved at add/update time).
    Git { url: String, rev: String },
    /// `.gzv-pkg` file as spelled + its content hash.
    Pkg { file: String, hash: String },
}

/// Resolution result: dep name → absolute package directory, plus
/// the lockfile contents to write (sorted by name).
#[derive(Debug)]
pub struct Resolution {
    pub dirs: BTreeMap<String, PathBuf>,
    pub lock: Vec<LockedPkg>,
}

/// Local package cache: git checkouts + unpacked `.gzv-pkg` files.
/// `$TOTO_PKG_CACHE` overrides (tests use scratch dirs); else
/// `$XDG_CACHE_HOME/toto/pkgs`, else `~/.cache/toto/pkgs`.
#[derive(Clone, Debug)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn from_env() -> Self {
        if let Ok(dir) = std::env::var("TOTO_PKG_CACHE")
            && !dir.is_empty()
        {
            return Cache {
                dir: PathBuf::from(dir),
            };
        }
        let base = std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".cache")
            });
        Cache {
            dir: base.join("toto").join("pkgs"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn git_dir(&self, url: &str, rev: &str) -> PathBuf {
        self.dir.join(format!(
            "git-{:016x}-{}",
            fnv1a64(url.as_bytes()),
            short_rev(rev)
        ))
    }

    fn pkg_dir(&self, file_hash: &str) -> PathBuf {
        self.dir.join(format!("pkg-{file_hash}"))
    }
}

fn short_rev(rev: &str) -> String {
    rev.chars().take(16).collect()
}

/// FNV-1a/64: stable, dependency-free integrity hash (NOT crypto).
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hex16(h: u64) -> String {
    format!("{h:016x}")
}

/// Hash every `.gzv` file under `dir` (sorted), skipping `tests/`,
/// `target/`, and `toto.lock` (shipped code only, not the
/// package's own tests or build outputs).
fn hash_gzv_files(dir: &Path) -> Result<String, PkgError> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut kids: Vec<PathBuf> = Vec::new();
        let rd = std::fs::read_dir(&d).map_err(|e| {
            PkgError::new(
                "E0608",
                format!("cannot read package dir `{}`: {e}", d.display()),
            )
        })?;
        for entry in rd.flatten() {
            kids.push(entry.path());
        }
        kids.sort();
        for kid in kids {
            let rel = kid
                .strip_prefix(dir)
                .unwrap_or(&kid)
                .to_string_lossy()
                .into_owned();
            if kid.is_dir() {
                let top = rel.split('/').next().unwrap_or("");
                if top == "tests" || top == "target" {
                    continue;
                }
                stack.push(kid);
            } else if kid.extension().is_some_and(|e| e == "gzv")
                && kid.file_name().is_some_and(|n| n != "toto.lock")
            {
                let bytes = std::fs::read(&kid).map_err(|e| {
                    PkgError::new(
                        "E0608",
                        format!("cannot read package file `{}`: {e}", kid.display()),
                    )
                })?;
                entries.push((rel, bytes));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h: u64 = 0xcbf29ce484222325;
    for (rel, bytes) in &entries {
        for b in rel.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("fnv1a64:{}", hex16(h)))
}

/// One discovered candidate: exactly one version from one source.
#[derive(Debug)]
struct Candidate {
    version: Version,
    /// Manifest spelling (for messages + lockfile).
    source: CandidateSource,
    /// Absolute package directory (materialized).
    dir: PathBuf,
    /// Content hash (lockfile).
    files_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CandidateSource {
    Path(String),
    Git { url: String, rev: String },
    Pkg { file: String, hash: String },
}

/// A requirement with provenance (who asked, in what words).
#[derive(Clone, Debug)]
struct Requirement {
    requirer: String,
    text: String,
    req: Option<Req>,
}

pub(crate) fn git_cmd() -> Result<Command, PkgError> {
    let probe = Command::new("git").arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => Ok(Command::new("git")),
        _ => Err(PkgError::new(
            "E0608",
            "git not found on PATH (needed for git dependencies)",
        )),
    }
}

/// Clone (if missing) and check out `rev` (or resolve `tag`) in
/// the cache. Network ONLY when `fetch` is true; with
/// `fetch=false` the cache must already satisfy the request.
fn materialize_git(
    cache: &Cache,
    url: &str,
    rev: &Option<String>,
    tag: &Option<String>,
    fetch: bool,
    requirer: &str,
) -> Result<(PathBuf, String), PkgError> {
    let bad_pin = |msg: String| PkgError::new("E0605", msg);
    if rev.is_none() && tag.is_none() {
        return Err(bad_pin(format!(
            "unpinned git source `{url}` (required by {requirer}): pin `rev` or `tag`; branches and HEAD are never resolved"
        )));
    }
    if rev.is_some() && tag.is_some() {
        return Err(bad_pin(format!(
            "git source `{url}` pins both `rev` and `tag` (required by {requirer}): pin one revision"
        )));
    }
    git_cmd()?;
    // Resolve the wanted rev first (tags need the network or a
    // warm clone); the checkout dir is keyed by the resolved hash.
    let mut probe_dir: Option<PathBuf> = None;
    if tag.is_some() && !fetch {
        // Best-effort: reuse any cached checkout for this URL whose
        // tag resolves locally; else report missing (update fetches).
        let prefix = format!("git-{:016x}-", fnv1a64(url.as_bytes()));
        if let Ok(rd) = std::fs::read_dir(cache.dir()) {
            let mut cands: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                })
                .collect();
            cands.sort();
            for dir in cands {
                let tag_name = tag.clone().unwrap_or_default();
                let out = Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .arg("rev-parse")
                    .arg(format!("{tag_name}^{{commit}}"))
                    .output();
                if let Ok(out) = out
                    && out.status.success()
                {
                    probe_dir = Some(dir);
                    break;
                }
            }
        }
    }
    let (work, resolved) = if let Some(dir) = probe_dir {
        let rev = dir
            .file_name()
            .and_then(|n| {
                n.to_string_lossy()
                    .split('-')
                    .next_back()
                    .map(str::to_string)
            })
            .unwrap_or_default();
        (dir, rev)
    } else {
        // Clone (fetch=true) or reuse the exact-rev dir (fetch=false).
        let wanted = if let Some(r) = rev {
            r.clone()
        } else {
            // tag → hash needs a clone to resolve against.
            let tag_name = tag.clone().unwrap_or_default();
            if !fetch {
                return Err(PkgError::new(
                    "E0604",
                    format!(
                        "git source `{url}` tag `{tag_name}` (required by {requirer}) is not in the local package cache; run `toto update` with network access"
                    ),
                ));
            }
            let scratch = cache
                .dir
                .join(format!("git-{:016x}-tmpclone", fnv1a64(url.as_bytes())));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(cache.dir())
                .map_err(|e| PkgError::new("E0608", format!("cannot create package cache: {e}")))?;
            let st = Command::new("git")
                .arg("clone")
                .arg("--quiet")
                .arg(url)
                .arg(&scratch)
                .status()
                .map_err(|e| {
                    PkgError::new("E0608", format!("cannot clone git source `{url}`: {e}"))
                })?;
            if !st.success() {
                return Err(PkgError::new(
                    "E0608",
                    format!("cannot clone git source `{url}` (exit {st})"),
                ));
            }
            let out = Command::new("git")
                .arg("-C")
                .arg(&scratch)
                .arg("rev-parse")
                .arg(format!("{tag_name}^{{commit}}"))
                .output()
                .map_err(|e| {
                    PkgError::new("E0608", format!("cannot resolve tag `{tag_name}`: {e}"))
                })?;
            if !out.status.success() {
                return Err(PkgError::new(
                    "E0608",
                    format!("git source `{url}` has no tag `{tag_name}`"),
                ));
            }
            let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let _ = std::fs::remove_dir_all(&scratch);
            hash
        };
        let dir = cache.git_dir(url, &wanted);
        if !dir.join(".git").is_dir() {
            if !fetch {
                return Err(PkgError::new(
                    "E0604",
                    format!(
                        "locked git rev `{wanted}` of `{url}` (required by {requirer}) is not in the local package cache; run `toto update` with network access — builds never fetch"
                    ),
                ));
            }
            std::fs::create_dir_all(cache.dir())
                .map_err(|e| PkgError::new("E0608", format!("cannot create package cache: {e}")))?;
            let st = Command::new("git")
                .arg("clone")
                .arg("--quiet")
                .arg(url)
                .arg(&dir)
                .status()
                .map_err(|e| {
                    PkgError::new("E0608", format!("cannot clone git source `{url}`: {e}"))
                })?;
            if !st.success() {
                return Err(PkgError::new(
                    "E0608",
                    format!("cannot clone git source `{url}` (exit {st})"),
                ));
            }
        } else if fetch {
            let st = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .arg("fetch")
                .arg("--quiet")
                .arg("origin")
                .status()
                .map_err(|e| {
                    PkgError::new("E0608", format!("cannot fetch git source `{url}`: {e}"))
                })?;
            if !st.success() {
                return Err(PkgError::new(
                    "E0608",
                    format!("cannot fetch git source `{url}` (exit {st})"),
                ));
            }
        }
        let st = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("checkout")
            .arg("--quiet")
            .arg(&wanted)
            .status()
            .map_err(|e| {
                PkgError::new(
                    "E0608",
                    format!("cannot check out rev `{wanted}` of `{url}`: {e}"),
                )
            })?;
        if !st.success() {
            return Err(PkgError::new(
                "E0604",
                format!(
                    "locked git rev `{wanted}` of `{url}` is not available (exit {st}); run `toto update` with network access"
                ),
            ));
        }
        (dir, wanted)
    };
    Ok((work, resolved))
}

/// Unpack a `.gzv-pkg` file into the content-addressed cache dir
/// (refill when missing; the FILE is the authority — a changed
/// file hashes differently and fails verification at load).
fn materialize_pkg(
    cache: &Cache,
    root: &Path,
    file: &str,
    requirer: &str,
) -> Result<(PathBuf, String), PkgError> {
    let path = root.join(file);
    let bytes = std::fs::read(&path).map_err(|e| {
        PkgError::new(
            "E0608",
            format!("cannot read package file `{file}` (required by {requirer}): {e}"),
        )
    })?;
    let hash = hex16(fnv1a64(&bytes));
    let dir = cache.pkg_dir(&hash);
    if !dir.join("toto.toml").is_file() {
        unpack_pkg(&bytes, &dir).map_err(|e| {
            PkgError::new("E0608", format!("cannot unpack package file `{file}`: {e}"))
        })?;
    }
    Ok((dir, hash))
}

/// Read one package's own manifest (name/version must parse; the
/// declared name must equal the dependency name).
fn read_pkg_manifest(dir: &Path, dep_name: &str) -> Result<Manifest, PkgError> {
    let path = dir.join("toto.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        PkgError::new(
            "E0608",
            format!(
                "package `{dep_name}` has no readable toto.toml in `{}`: {e}",
                dir.display()
            ),
        )
    })?;
    let m = super::manifest::parse_manifest(&path, &text).map_err(|e| {
        PkgError::new(
            "E0608",
            format!("package `{dep_name}` manifest invalid: {}", e.message),
        )
    })?;
    if m.name != dep_name {
        return Err(PkgError::new(
            "E0608",
            format!(
                "package directory `{}` declares name `{}` but is required as `{dep_name}`",
                dir.display(),
                m.name
            ),
        ));
    }
    super::semver::Version::parse(&m.version).map_err(|e| {
        PkgError::new(
            "E0608",
            format!("package `{dep_name}` version invalid: {e}"),
        )
    })?;
    Ok(m)
}

/// Canonical source identity (pin comparison + tie-breaks).
pub fn locked_source_key(s: &LockedSource) -> String {
    match s {
        LockedSource::Path(p) => format!("path:{p}"),
        LockedSource::Git { url, rev } => format!("git:{url}@{rev}"),
        LockedSource::Pkg { file, hash } => format!("pkg:{file}@{hash}"),
    }
}

fn candidate_source_key(s: &CandidateSource) -> String {
    match s {
        CandidateSource::Path(p) => format!("path:{p}"),
        CandidateSource::Git { url, rev } => format!("git:{url}@{rev}"),
        CandidateSource::Pkg { file, hash } => format!("pkg:{file}@{hash}"),
    }
}

/// Read the package name from packed `.gzv-pkg` bytes (no unpack).
pub fn pkg_name_of(bytes: &[u8]) -> Result<String, String> {
    let mut pos = 0usize;
    let magic = get_bytes(bytes, &mut pos, PKG_MAGIC.len())?;
    if magic != PKG_MAGIC {
        return Err("not a .gzv-pkg file (bad magic)".to_string());
    }
    let mlen = get_u32(bytes, &mut pos)? as usize;
    let manifest = get_bytes(bytes, &mut pos, mlen)?;
    let text = std::str::from_utf8(manifest).map_err(|_| "non-UTF-8 manifest".to_string())?;
    let path = PathBuf::from("package.gzv-pkg(toto.toml)");
    let m = super::manifest::parse_manifest(&path, text)
        .map_err(|e| format!("packed manifest invalid: {}", e.message))?;
    Ok(m.name)
}

/// Resolve every requirement (root + transitive, Amendment 2a)
/// and materialize each selected package. `fetch=true` allows
/// network (add/update/lock creation); `fetch=false` is the
/// offline build path.
pub fn resolve(
    root: &Path,
    manifest: &Manifest,
    cache: &Cache,
    fetch: bool,
) -> Result<Resolution, PkgError> {
    let empty_pins = BTreeMap::new();
    let all_free = HashSet::new();
    resolve_inner(root, manifest, cache, fetch, &empty_pins, &all_free, false)
}

/// Pinned resolve for `toto update <pkg>`: packages outside
/// `free` keep exactly their locked (version, source); the
/// `free` set (named roots + their live subtree) re-resolves
/// normally. A pin with no matching candidate is a loud error
/// (the source moved; run a full `toto update`).
pub fn resolve_with_pins(
    root: &Path,
    manifest: &Manifest,
    cache: &Cache,
    fetch: bool,
    pins: &BTreeMap<String, (Version, String)>,
    free: &HashSet<String>,
) -> Result<Resolution, PkgError> {
    resolve_inner(root, manifest, cache, fetch, pins, free, true)
}

#[allow(clippy::too_many_arguments)]
fn resolve_inner(
    root: &Path,
    manifest: &Manifest,
    cache: &Cache,
    fetch: bool,
    pins: &BTreeMap<String, (Version, String)>,
    free: &std::collections::HashSet<String>,
    use_pins: bool,
) -> Result<Resolution, PkgError> {
    if manifest.name.is_empty() {
        return Err(PkgError::new("E0608", "project manifest has no name"));
    }
    // DFS over sources (sorted for determinism), collecting
    // candidates + requirements with provenance. `stack` is the
    // current dependency chain (cycle detection, E0607).
    let mut candidates: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let mut reqs: BTreeMap<String, Vec<Requirement>> = BTreeMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    // Package index (./toto-index.toml, then ~/.toto/index.toml),
    // consulted for bare requirements only.
    let index = Indexes::load(root)?;
    // Seed with the root requirements (sorted by name: BTreeMap).
    let mut seed: Vec<(String, DepReq)> = manifest
        .dependencies
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    seed.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, dep) in seed {
        visit_source(
            root,
            &manifest.name,
            cache,
            &index,
            fetch,
            &name,
            &dep,
            "<root>",
            &mut candidates,
            &mut reqs,
            &mut visited,
            &mut stack,
        )?;
    }
    // Select: highest version satisfying EVERY requirement.
    // With pins (`toto update <pkg>`), non-free packages keep
    // exactly their locked (version, source) WHEN SUCH A CANDIDATE
    // EXISTS. A pin preserves what exists, not what is gone: when
    // the source moved or changed version (no candidate matches
    // the pin), the package rejoins free selection — every
    // requirement still enforced, every resulting move reported
    // by the lock diff. `build` (never `update`) keeps erroring
    // on any mismatch, so this fallback can never serve stale
    // content silently.
    let mut lock: Vec<LockedPkg> = Vec::new();
    let mut dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (name, cands) in candidates.iter() {
        let rs = reqs.get(name).cloned().unwrap_or_default();
        let pinned = if use_pins && !free.contains(name) {
            pins.get(name)
        } else {
            None
        };
        let satisfies = |c: &&Candidate| {
            rs.iter().all(|r| match &r.req {
                None => true,
                Some(q) => q.matches(c.version),
            })
        };
        let mut ok: Vec<&Candidate> = cands.iter().filter(satisfies).collect();
        if let Some((pv, pk)) = pinned {
            let pinned_ok: Vec<&Candidate> = ok
                .iter()
                .filter(|c| c.version == *pv && candidate_source_key(&c.source) == *pk)
                .copied()
                .collect();
            if !pinned_ok.is_empty() {
                ok = pinned_ok;
            }
            // Else: the pinned candidate is gone — fall back to
            // free selection (still enforcing every requirement).
        }
        if ok.is_empty() {
            let mut msg = format!("no version of package `{name}` satisfies all requirements:");
            let mut sorted: Vec<&Requirement> = rs.iter().collect();
            sorted.sort_by(|a, b| a.requirer.cmp(&b.requirer));
            for r in sorted {
                msg.push_str(&format!("\n  - {} (required by {})", r.text, r.requirer));
            }
            let mut vs: Vec<String> = cands.iter().map(|c| c.version.to_string()).collect();
            vs.sort();
            vs.dedup();
            msg.push_str(&format!("\navailable: {}", vs.join(", ")));
            return Err(PkgError::new("E0602", msg));
        }
        // Highest version; ties broken by source display (stable).
        ok.sort_by(|a, b| {
            b.version
                .cmp(&a.version)
                .then_with(|| source_display(&a.source).cmp(&source_display(&b.source)))
        });
        let sel = ok[0];
        let source = match &sel.source {
            CandidateSource::Path(p) => LockedSource::Path(p.clone()),
            CandidateSource::Git { url, rev } => LockedSource::Git {
                url: url.clone(),
                rev: rev.clone(),
            },
            CandidateSource::Pkg { file, hash } => LockedSource::Pkg {
                file: file.clone(),
                hash: hash.clone(),
            },
        };
        lock.push(LockedPkg {
            name: name.clone(),
            version: sel.version,
            source,
            files_hash: sel.files_hash.clone(),
        });
        dirs.insert(name.clone(), sel.dir.clone());
    }
    lock.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Resolution { dirs, lock })
}

fn source_display(s: &CandidateSource) -> String {
    match s {
        CandidateSource::Path(p) => format!("path {p}"),
        CandidateSource::Git { url, rev } => format!("git {url} {rev}"),
        CandidateSource::Pkg { file, hash } => format!("pkg {file} {hash}"),
    }
}

/// List `(tag, commit)` for a git URL (Wave 7: index resolution).
/// With `fetch`, `git ls-remote --tags` (no clone needed);
/// otherwise the union of tags across warm cached checkouts.
/// Peeled `^{}` lines win (annotated tags resolve to commits).
fn enumerate_git_tags(
    url: &str,
    cache: &Cache,
    fetch: bool,
    requirer: &str,
) -> Result<Vec<(String, String)>, PkgError> {
    if fetch {
        let mut cmd = git_cmd()?;
        let out = cmd
            .arg("ls-remote")
            .arg("--tags")
            .arg(url)
            .output()
            .map_err(|e| {
                PkgError::new(
                    "E0608",
                    format!("cannot list tags of `{url}` (required by {requirer}): {e}"),
                )
            })?;
        if !out.status.success() {
            return Err(PkgError::new(
                "E0608",
                format!(
                    "cannot list tags of `{url}` (required by {requirer}): {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        let mut tags: BTreeMap<String, String> = BTreeMap::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut parts = line.split_whitespace();
            let (Some(hash), Some(rref)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some(name) = rref.strip_prefix("refs/tags/") else {
                continue;
            };
            if let Some(peeled) = name.strip_suffix("^{}") {
                tags.insert(peeled.to_string(), hash.to_string());
            } else {
                tags.entry(name.to_string())
                    .or_insert_with(|| hash.to_string());
            }
        }
        return Ok(tags.into_iter().collect());
    }
    // Offline: union of tags across warm cached checkouts.
    let prefix = format!("git-{:016x}-", fnv1a64(url.as_bytes()));
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(cache.dir()) {
        let mut found: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .collect();
        found.sort();
        dirs.append(&mut found);
    }
    if dirs.is_empty() {
        return Err(PkgError::new(
            "E0604",
            format!(
                "git source `{url}` (required by {requirer}) is not in the local package cache; run `toto update` with network access"
            ),
        ));
    }
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    for dir in &dirs {
        let mut cmd = git_cmd()?;
        let out = match cmd.arg("-C").arg(dir).arg("tag").arg("--list").output() {
            Ok(o) if o.status.success() => o,
            _ => continue,
        };
        for name in String::from_utf8_lossy(&out.stdout).lines() {
            if tags.contains_key(name) {
                continue;
            }
            let mut rc = git_cmd()?;
            let ro = rc
                .arg("-C")
                .arg(dir)
                .arg("rev-list")
                .arg("-n")
                .arg("1")
                .arg(name)
                .output();
            if let Ok(ro) = ro
                && ro.status.success()
            {
                let hash = String::from_utf8_lossy(&ro.stdout).trim().to_string();
                if !hash.is_empty() {
                    tags.insert(name.to_string(), hash);
                }
            }
        }
    }
    Ok(tags.into_iter().collect())
}

/// Strip one leading `v`/`V` and parse strict MAJOR.MINOR.PATCH.
/// Anything else is not a release tag (documented convention:
/// release tags look like versions, e.g. `v1.2.0`).
fn tag_version(tag: &str) -> Option<Version> {
    let stripped = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    Version::parse(stripped).ok()
}

/// Check out every version-shaped tag and collect one candidate
/// per tag with a readable package manifest. Tags without a
/// `toto.toml` are skipped (not releases); a semver tag whose
/// manifest cannot be read is a loud error (broken release).
/// Zero usable tags is E0602: the requirement genuinely has
/// no candidates.
#[allow(clippy::too_many_arguments)]
fn collect_tagged_checkouts(
    url: &str,
    cache: &Cache,
    fetch: bool,
    name: &str,
    requirer: &str,
    visited: &mut HashSet<String>,
    checkouts: &mut Vec<(PathBuf, CandidateSource)>,
) -> Result<(), PkgError> {
    let tags = enumerate_git_tags(url, cache, fetch, requirer)?;
    let mut skipped_manifest = 0usize;
    let mut skipped_name = 0usize;
    for (tag, _rev) in tags {
        if tag_version(&tag).is_none() {
            skipped_name += 1;
            continue;
        }
        let key = format!("git:{url}#{tag}");
        if !visited.insert(key) {
            continue;
        }
        let no_rev: Option<String> = None;
        let only_tag = Some(tag);
        let (dir, _resolved) = materialize_git(cache, url, &no_rev, &only_tag, fetch, requirer)?;
        if !dir.join("toto.toml").is_file() {
            skipped_manifest += 1;
            continue;
        }
        checkouts.push((
            dir,
            CandidateSource::Git {
                url: url.to_string(),
                rev: _resolved,
            },
        ));
    }
    if checkouts.is_empty() {
        return Err(PkgError::new(
            "E0602",
            format!(
                "no usable release tags of `{url}` for `{name}` (required by {requirer}): skipped {skipped_name} non-version tags and {skipped_manifest} tags without a package manifest; tag releases like `v1.2.3`"
            ),
        ));
    }
    Ok(())
}

/// Visit one source: materialize it, record its version as a
/// candidate, file its requirement with provenance, recurse into
/// its own dependencies (Amendment 2a).
#[allow(clippy::too_many_arguments)]
fn visit_source(
    root: &Path,
    root_name: &str,
    cache: &Cache,
    index: &Indexes,
    fetch: bool,
    name: &str,
    dep: &DepReq,
    requirer: &str,
    candidates: &mut BTreeMap<String, Vec<Candidate>>,
    reqs: &mut BTreeMap<String, Vec<Requirement>>,
    visited: &mut HashSet<String>,
    stack: &mut Vec<String>,
) -> Result<(), PkgError> {
    if name == root_name {
        return Err(PkgError::new(
            "E0608",
            format!("package `{name}` has the same name as the project (required by {requirer})"),
        ));
    }
    if stack.contains(&name.to_string()) {
        let mut chain = stack.clone();
        chain.push(name.to_string());
        return Err(PkgError::new(
            "E0607",
            format!("dependency cycle: {}", chain.join(" -> ")),
        ));
    }
    let req_text = match (&dep.source, &dep.version) {
        (DepSource::Bare, v) => v.map(|q| q.to_string()).unwrap_or_default(),
        (DepSource::Path(p), v) => match v {
            Some(q) => format!("{{ path = \"{p}\", version = \"{q}\" }}"),
            None => format!("{{ path = \"{p}\" }}"),
        },
        (DepSource::Git { url, rev, tag }, v) => {
            let pin = rev
                .as_ref()
                .map(|r| format!("rev = \"{r}\""))
                .or_else(|| tag.as_ref().map(|t| format!("tag = \"{t}\"")))
                .unwrap_or_default();
            match v {
                Some(q) => format!("{{ git = \"{url}\", {pin}, version = \"{q}\" }}"),
                None => format!("{{ git = \"{url}\", {pin} }}"),
            }
        }
    };
    reqs.entry(name.to_string()).or_default().push(Requirement {
        requirer: requirer.to_string(),
        text: req_text,
        req: dep.version,
    });
    // Materialize the source into checkouts. Single-checkout
    // sources (path, pinned git) keep the existing
    // once-per-source visit; an index-mapped name or an
    // unpinned git URL with a version requirement enumerates
    // release tags into one checkout per tag instead.
    let mut checkouts: Vec<(PathBuf, CandidateSource)> = Vec::new();
    match &dep.source {
        DepSource::Bare => match index.lookup(name) {
            Some(url) => {
                collect_tagged_checkouts(
                    url,
                    cache,
                    fetch,
                    name,
                    requirer,
                    visited,
                    &mut checkouts,
                )?;
            }
            None => {
                return Err(PkgError::new(
                    "E0606",
                    format!(
                        "requirement `{name} = \"{}\"` names no source (required by {requirer}): use `{{ path = … }}` or `{{ git = …, rev|tag = … }}`, or map the name to a git URL in ./toto-index.toml or ~/.toto/index.toml",
                        dep.version.map(|q| q.to_string()).unwrap_or_default()
                    ),
                ));
            }
        },
        DepSource::Path(p) => {
            let source_id = format!("path:{p}");
            if !visited.insert(source_id) {
                return Ok(());
            }
            if p.ends_with(".gzv-pkg") {
                let (dir, hash) = materialize_pkg(cache, root, p, requirer)?;
                checkouts.push((
                    dir,
                    CandidateSource::Pkg {
                        file: p.clone(),
                        hash,
                    },
                ));
            } else {
                let dir = root.join(p);
                if !dir.join("toto.toml").is_file() {
                    return Err(PkgError::new(
                        "E0608",
                        format!(
                            "path dependency `{name}` = `{p}` (required by {requirer}) has no toto.toml"
                        ),
                    ));
                }
                checkouts.push((dir, CandidateSource::Path(p.clone())));
            }
        }
        DepSource::Git {
            url,
            rev: None,
            tag: None,
        } if dep.version.is_some() => {
            collect_tagged_checkouts(url, cache, fetch, name, requirer, visited, &mut checkouts)?;
        }
        DepSource::Git { url, rev, tag } => {
            let source_id = format!(
                "git:{url}:{}",
                rev.clone().or(tag.clone()).unwrap_or_default()
            );
            if !visited.insert(source_id) {
                return Ok(());
            }
            let (dir, resolved) = materialize_git(cache, url, rev, tag, fetch, requirer)?;
            checkouts.push((
                dir,
                CandidateSource::Git {
                    url: url.clone(),
                    rev: resolved,
                },
            ));
        }
    };
    for (dir, candsource) in checkouts {
        let pkg_manifest = read_pkg_manifest(&dir, name)?;
        let version =
            super::semver::Version::parse(&pkg_manifest.version).expect("validated above");
        let files_hash = hash_gzv_files(&dir)?;
        candidates
            .entry(name.to_string())
            .or_default()
            .push(Candidate {
                version,
                source: candsource,
                dir: dir.clone(),
                files_hash,
            });
        // Recurse into the package's own dependencies (sorted).
        stack.push(name.to_string());
        let mut nested: Vec<(String, DepReq)> = pkg_manifest
            .dependencies
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        nested.sort_by(|a, b| a.0.cmp(&b.0));
        let who = format!("{name} {version}");
        for (sub, subdep) in nested {
            visit_source(
                root, root_name, cache, index, fetch, &sub, &subdep, &who, candidates, reqs,
                visited, stack,
            )?;
        }
        stack.pop();
    }
    Ok(())
}

const LOCKFILE_NAME: &str = "toto.lock";

/// Read `toto.lock`: `None` when absent; malformed content is
/// E0603 (never silently ignored).
pub fn read_lockfile(root: &Path) -> Result<Option<Vec<LockedPkg>>, PkgError> {
    let path = root.join(LOCKFILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(PkgError::new(
                "E0603",
                format!("cannot read `{}`: {e}", path.display()),
            ));
        }
    };
    parse_lockfile(&text)
        .map(Some)
        .map_err(|e| PkgError::new("E0603", format!("cannot parse `{}`: {e}", path.display())))
}

fn parse_lockfile(text: &str) -> Result<Vec<LockedPkg>, String> {
    let mut out: Vec<LockedPkg> = Vec::new();
    let mut cur: Option<LockedPkg> = None;
    let mut seen_kind: HashSet<String> = HashSet::new();
    let flush = |cur: &mut Option<LockedPkg>, out: &mut Vec<LockedPkg>| -> Result<(), String> {
        if let Some(p) = cur.take() {
            if p.name.is_empty() || p.version.to_string().is_empty() || p.files_hash.is_empty() {
                return Err("incomplete [[package]] block".to_string());
            }
            out.push(p);
        }
        Ok(())
    };
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[package]]" {
            flush(&mut cur, &mut out)?;
            cur = Some(LockedPkg {
                name: String::new(),
                version: Version {
                    major: 0,
                    minor: 0,
                    patch: 0,
                },
                source: LockedSource::Path(String::new()),
                files_hash: String::new(),
            });
            seen_kind.clear();
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected `key = \"value\"`"))?;
        let val = unquote_lock(val.trim())
            .ok_or_else(|| format!("line {line_no}: expected plain double-quoted string"))?;
        let pkg = cur
            .as_mut()
            .ok_or_else(|| format!("line {line_no}: keys must live under `[[package]]`"))?;
        if !seen_kind.insert(key.trim().to_string()) {
            return Err(format!("line {line_no}: duplicate key `{}`", key.trim()));
        }
        match key.trim() {
            "name" => pkg.name = val,
            "version" => {
                pkg.version = Version::parse(&val).map_err(|e| format!("line {line_no}: {e}"))?;
            }
            "source-kind" => match val.as_str() {
                "path" => pkg.source = LockedSource::Path(String::new()),
                "git" => {
                    pkg.source = LockedSource::Git {
                        url: String::new(),
                        rev: String::new(),
                    }
                }
                "pkg" => {
                    pkg.source = LockedSource::Pkg {
                        file: String::new(),
                        hash: String::new(),
                    }
                }
                _ => return Err(format!("line {line_no}: unknown source-kind `{val}`")),
            },
            "source" => match &mut pkg.source {
                LockedSource::Path(p) => *p = val,
                LockedSource::Git { url, .. } => *url = val,
                LockedSource::Pkg { file, .. } => *file = val,
            },
            "rev" => match &mut pkg.source {
                LockedSource::Git { rev, .. } => *rev = val,
                _ => return Err(format!("line {line_no}: `rev` needs source-kind `git`")),
            },
            "hash" => match &mut pkg.source {
                LockedSource::Pkg { hash, .. } => *hash = val,
                _ => return Err(format!("line {line_no}: `hash` needs source-kind `pkg`")),
            },
            "files" => pkg.files_hash = val,
            other => return Err(format!("line {line_no}: unknown lockfile key `{other}`")),
        }
    }
    flush(&mut cur, &mut out)?;
    // Completeness: every entry needs its source fields filled.
    for p in &out {
        let complete = match &p.source {
            LockedSource::Path(s) => !s.is_empty(),
            LockedSource::Git { url, rev } => !url.is_empty() && !rev.is_empty(),
            LockedSource::Pkg { file, hash } => !file.is_empty() && !hash.is_empty(),
        };
        if !complete {
            return Err(format!("package `{}` has an incomplete source", p.name));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn unquote_lock(s: &str) -> Option<String> {
    let body = s.strip_prefix('"').and_then(|s| s.strip_suffix('"'))?;
    if body.contains('"') || body.contains('\\') {
        return None;
    }
    Some(body.to_string())
}

/// Write `toto.lock` (sorted by name; byte-deterministic for
/// identical input — see the determinism test).
pub fn write_lockfile(root: &Path, lock: &[LockedPkg]) -> Result<(), PkgError> {
    let mut sorted: Vec<&LockedPkg> = lock.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut text = String::from(
        "# toto.lock — generated by `toto update` (or `toto build` on first use); do not edit.\n# Regenerate: `toto update`. Any hand edit fails verification at load.\n",
    );
    for p in sorted {
        text.push_str("\n[[package]]\n");
        text.push_str(&format!("name = \"{}\"\n", p.name));
        text.push_str(&format!("version = \"{}\"\n", p.version));
        match &p.source {
            LockedSource::Path(s) => {
                text.push_str("source-kind = \"path\"\n");
                text.push_str(&format!("source = \"{s}\"\n"));
            }
            LockedSource::Git { url, rev } => {
                text.push_str("source-kind = \"git\"\n");
                text.push_str(&format!("source = \"{url}\"\n"));
                text.push_str(&format!("rev = \"{rev}\"\n"));
            }
            LockedSource::Pkg { file, hash } => {
                text.push_str("source-kind = \"pkg\"\n");
                text.push_str(&format!("source = \"{file}\"\n"));
                text.push_str(&format!("hash = \"{hash}\"\n"));
            }
        }
        text.push_str(&format!("files = \"{}\"\n", p.files_hash));
    }
    std::fs::write(root.join(LOCKFILE_NAME), text)
        .map_err(|e| PkgError::new("E0603", format!("cannot write `{LOCKFILE_NAME}`: {e}")))
}

/// Compare a fresh resolution against the lockfile. Selection
/// (set, versions, sources) differs → E0603 (the manifest
/// changed; run `toto update` — never a silent re-resolve).
/// Selection matches but content hashes differ → E0604 (sources
/// changed on disk or in the cache; `toto update` accepts the
/// change). Since the fresh resolution re-hashes live sources,
/// no stale dependency can be served silently.
fn verify_against_lock(resolved: &[LockedPkg], locked: &[LockedPkg]) -> Result<(), PkgError> {
    if resolved.len() != locked.len() {
        let rn: Vec<&str> = resolved.iter().map(|p| p.name.as_str()).collect();
        let ln: Vec<&str> = locked.iter().map(|p| p.name.as_str()).collect();
        return Err(PkgError::new(
            "E0603",
            format!(
                "toto.lock disagrees with toto.toml: resolved [{}] vs locked [{}]; run `toto update`",
                rn.join(", "),
                ln.join(", ")
            ),
        ));
    }
    for (r, l) in resolved.iter().zip(locked.iter()) {
        if r.name != l.name || r.version != l.version || r.source != l.source {
            return Err(PkgError::new(
                "E0603",
                format!(
                    "toto.lock disagrees with toto.toml on package `{}` (resolved {} {} vs locked {} {}); run `toto update` — never a silent re-resolve",
                    r.name,
                    r.version,
                    lock_source_desc(&r.source),
                    l.version,
                    lock_source_desc(&l.source)
                ),
            ));
        }
        if r.files_hash != l.files_hash {
            return Err(PkgError::new(
                "E0604",
                format!(
                    "package `{}` changed on disk (locked {} vs live {}); run `toto update` to accept the change",
                    r.name, l.files_hash, r.files_hash
                ),
            ));
        }
    }
    Ok(())
}

fn lock_source_desc(s: &LockedSource) -> String {
    match s {
        LockedSource::Path(p) => format!("path {p}"),
        LockedSource::Git { url, rev } => format!("git {url} rev {}", short_rev(rev)),
        LockedSource::Pkg { file, hash } => format!("pkg {file} {}", &hash[..8.min(hash.len())]),
    }
}

/// The build/run/test path: no manifest deps → empty map (today's
/// behavior exactly; a lingering lockfile is ignored). Otherwise
/// read the lock (missing → resolve WITH fetch + write loudly),
/// or verify a present lock by recomputation (no fetch — the
/// offline build path; a missing cached rev fails E0604 here).
/// Returns dep name → absolute package directory.
pub fn ensure(
    root: &Path,
    manifest: &Manifest,
    cache: &Cache,
) -> Result<BTreeMap<String, PathBuf>, PkgError> {
    if manifest.dependencies.is_empty() {
        return Ok(BTreeMap::new());
    }
    match read_lockfile(root)? {
        None => {
            let res = resolve(root, manifest, cache, true)?;
            write_lockfile(root, &res.lock)?;
            eprintln!(
                "toto: wrote {} ({} packages; re-run is offline-clean)",
                LOCKFILE_NAME,
                res.lock.len()
            );
            Ok(res.dirs)
        }
        Some(locked) => {
            let res = resolve(root, manifest, cache, false)?;
            verify_against_lock(&res.lock, &locked)?;
            Ok(res.dirs)
        }
    }
}

/// `.gzv-pkg` file format (v1, dependency-free, deterministic):
/// `GZVPKG1\n`, u32-LE manifest length + manifest bytes
/// (`toto.toml` text), u32-LE file count, then per file: u32-LE
/// path length + relative path bytes (`/` separators) + u64-LE
/// content length + content bytes. Files sorted by path. No
/// compression, no timestamps: identical inputs pack
/// byte-identically. Paths must be relative and `..`-free.
const PKG_MAGIC: &[u8] = b"GZVPKG1\n";

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn get_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err("truncated package file".to_string());
    }
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[*pos..*pos + 4]);
    *pos += 4;
    Ok(u32::from_le_bytes(b))
}

fn get_u64(data: &[u8], pos: &mut usize) -> Result<u64, String> {
    if *pos + 8 > data.len() {
        return Err("truncated package file".to_string());
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[*pos..*pos + 8]);
    *pos += 8;
    Ok(u64::from_le_bytes(b))
}

fn get_bytes<'a>(data: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8], String> {
    if *pos + len > data.len() {
        return Err("truncated package file".to_string());
    }
    let s = &data[*pos..*pos + len];
    *pos += len;
    Ok(s)
}

/// Pack a package directory: `toto.toml` + every `.gzv` file
/// EXCEPT `tests/`, `target/`, and any `toto.lock` (shipped code
/// only). Returns the file bytes (`<name>-<version>.gzv-pkg`
/// naming is the caller's job).
pub fn pack_pkg(dir: &Path) -> Result<(String, Vec<u8>), PkgError> {
    let manifest_text = std::fs::read_to_string(dir.join("toto.toml")).map_err(|e| {
        PkgError::new(
            "E0608",
            format!(
                "cannot pack `{}`: no readable toto.toml: {e}",
                dir.display()
            ),
        )
    })?;
    let m =
        super::manifest::parse_manifest(&dir.join("toto.toml"), &manifest_text).map_err(|e| {
            PkgError::new(
                "E0608",
                format!("cannot pack `{}`: {}", dir.display(), e.message),
            )
        })?;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(&d)
            .map_err(|e| PkgError::new("E0608", format!("cannot pack `{}`: {e}", dir.display())))?
            .flatten()
            .map(|e| e.path())
            .collect();
        kids.sort();
        for kid in kids {
            let rel = kid
                .strip_prefix(dir)
                .unwrap_or(&kid)
                .to_string_lossy()
                .replace('\\', "/");
            if kid.is_dir() {
                let top = rel.split('/').next().unwrap_or("");
                if top == "tests" || top == "target" {
                    continue;
                }
                stack.push(kid);
            } else if kid.extension().is_some_and(|e| e == "gzv") {
                let bytes = std::fs::read(&kid).map_err(|e| {
                    PkgError::new("E0608", format!("cannot read `{}`: {e}", kid.display()))
                })?;
                files.push((rel, bytes));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    out.extend_from_slice(PKG_MAGIC);
    put_u32(&mut out, manifest_text.len() as u32);
    out.extend_from_slice(manifest_text.as_bytes());
    put_u32(&mut out, files.len() as u32);
    for (rel, bytes) in &files {
        put_u32(&mut out, rel.len() as u32);
        out.extend_from_slice(rel.as_bytes());
        put_u64(&mut out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }
    Ok((format!("{}-{}.gzv-pkg", m.name, m.version), out))
}

/// Unpack `.gzv-pkg` bytes into `dir` (created; must not exist
/// with content — the cache fills missing dirs only).
pub fn unpack_pkg(bytes: &[u8], dir: &Path) -> Result<(), String> {
    let mut pos = 0usize;
    let magic = get_bytes(bytes, &mut pos, PKG_MAGIC.len())?;
    if magic != PKG_MAGIC {
        return Err("not a .gzv-pkg file (bad magic)".to_string());
    }
    let mlen = get_u32(bytes, &mut pos)? as usize;
    let manifest = get_bytes(bytes, &mut pos, mlen)?;
    let n = get_u32(bytes, &mut pos)?;
    if n > 100_000 {
        return Err("absurd file count".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create dir: {e}"))?;
    std::fs::write(dir.join("toto.toml"), manifest)
        .map_err(|e| format!("cannot write toto.toml: {e}"))?;
    for _ in 0..n {
        let plen = get_u32(bytes, &mut pos)? as usize;
        if plen > 4096 {
            return Err("absurd path length".to_string());
        }
        let prels = get_bytes(bytes, &mut pos, plen)?;
        let rel = std::str::from_utf8(prels).map_err(|_| "non-UTF-8 path".to_string())?;
        if rel.is_empty()
            || rel.starts_with('/')
            || rel.split('/').any(|c| c == ".." || c.is_empty())
        {
            return Err(format!("unsafe path `{rel}`"));
        }
        let clen = get_u64(bytes, &mut pos)? as usize;
        if clen > 1 << 30 {
            return Err("absurd file length".to_string());
        }
        let content = get_bytes(bytes, &mut pos, clen)?;
        let dest = dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create dir: {e}"))?;
        }
        std::fs::write(&dest, content).map_err(|e| format!("cannot write file: {e}"))?;
    }
    if pos != bytes.len() {
        return Err("trailing bytes".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch dir (parallel-safe: pid + counter).
    fn scratch(tag: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("toto-pkgtest-{}-{}-{}", std::process::id(), n, tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Write a minimal package: toto.toml + one .gzv file.
    /// Returns the package dir.
    fn mkpkg(root: &Path, name: &str, version: &str, deps: &[(&str, &str)], body: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("pkg dir");
        let mut toml =
            format!("[project]\nname = \"{name}\"\nversion = \"{version}\"\nentry = \"lib.gzv\"\n");
        if !deps.is_empty() {
            toml.push_str("[dependencies]\n");
            for (d, spec) in deps {
                toml.push_str(&format!("{d} = {spec}\n"));
            }
        }
        std::fs::write(dir.join("toto.toml"), toml).expect("manifest");
        std::fs::write(
            dir.join("lib.gzv"),
            format!("// {name} {version}\n{body}\n"),
        )
        .expect("lib");
        dir
    }

    fn manifest_of(deps: &[(&str, &str)]) -> Manifest {
        let mut toml =
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nentry = \"main.gzv\"\n[dependencies]\n"
                .to_string();
        for (d, spec) in deps {
            toml.push_str(&format!("{d} = {spec}\n"));
        }
        let path = PathBuf::from("toto.toml");
        super::super::manifest::parse_manifest(&path, &toml).expect("test manifest")
    }

    fn cache_in(root: &Path) -> Cache {
        Cache {
            dir: root.join("cache"),
        }
    }

    #[test]
    fn resolve_path_dep_and_lock_round_trip() {
        let root = scratch("basic");
        mkpkg(
            &root,
            "leftpad",
            "1.2.0",
            &[],
            "fn pad() -> int { return 1; }",
        );
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        let manifest = manifest_of(&[("leftpad", "{ path = \"../leftpad\" }")]);
        let cache = cache_in(&root);
        let res = resolve(&app, &manifest, &cache, true).expect("resolves");
        assert_eq!(res.lock.len(), 1);
        assert_eq!(res.lock[0].name, "leftpad");
        assert_eq!(res.lock[0].version.to_string(), "1.2.0");
        assert!(matches!(res.lock[0].source, LockedSource::Path(_)));
        write_lockfile(&app, &res.lock).expect("write lock");
        let back = read_lockfile(&app).expect("read lock").expect("present");
        assert_eq!(back, res.lock);
        // Determinism: resolve twice → byte-identical lockfile.
        let res2 = resolve(&app, &manifest, &cache, true).expect("resolves again");
        write_lockfile(&app, &res2.lock).expect("rewrite");
        let t1 = std::fs::read_to_string(app.join("toto.lock")).expect("lock text");
        let res3 = resolve(&app, &manifest, &cache, true).expect("third");
        let tmp = root.join("app2");
        std::fs::create_dir_all(&tmp).expect("tmp");
        write_lockfile(&tmp, &res3.lock).expect("rewrite2");
        let t2 = std::fs::read_to_string(tmp.join("toto.lock")).expect("lock text2");
        assert_eq!(t1, t2);
    }

    #[test]
    fn transitive_deps_discovered_and_max_selected() {
        let root = scratch("trans");
        // mid 1.0.0 requires base ^1.0.0; base has 1.0.0 and 1.5.0
        // available via two path sources; highest wins.
        mkpkg(&root, "base1", "1.0.0", &[], "fn b() -> int { return 1; }");
        mkpkg(&root, "base5", "1.5.0", &[], "fn b() -> int { return 5; }");
        // Rename trick: path dirs carry their own names; use two
        // candidate dirs for one package name via manifest rename.
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        // mid depends on base; app depends on mid + base@base1 dir.
        // Give both base dirs the SAME package name via rewrite.
        for d in ["base1", "base5"] {
            let t = std::fs::read_to_string(root.join(d).join("toto.toml")).expect("read");
            std::fs::write(
                root.join(d).join("toto.toml"),
                t.replace(&format!("name = \"{d}\""), "name = \"base\""),
            )
            .expect("rename");
        }
        mkpkg(
            &root,
            "mid",
            "1.0.0",
            &[("base", "{ path = \"../base1\", version = \"^1.0.0\" }")],
            "fn m() -> int { return 2; }",
        );
        let manifest = manifest_of(&[
            ("mid", "{ path = \"../mid\" }"),
            ("base", "{ path = \"../base5\" }"),
        ]);
        let cache = cache_in(&root);
        let res = resolve(&app, &manifest, &cache, true).expect("resolves");
        // base: candidates 1.0.0 + 1.5.0, reqs ^1.0.0 (from mid)
        // and none (from root) → max 1.5.0.
        let base = res
            .lock
            .iter()
            .find(|p| p.name == "base")
            .expect("base locked");
        assert_eq!(base.version.to_string(), "1.5.0");
        let mid = res
            .lock
            .iter()
            .find(|p| p.name == "mid")
            .expect("mid locked");
        assert_eq!(mid.version.to_string(), "1.0.0");
        assert_eq!(res.lock.len(), 2);
    }

    #[test]
    fn version_conflict_names_every_requirement() {
        let root = scratch("conflict");
        mkpkg(&root, "base", "1.0.0", &[], "fn b() -> int { return 1; }");
        mkpkg(
            &root,
            "mid",
            "1.0.0",
            &[("base", "{ path = \"../base\", version = \"^2.0.0\" }")],
            "fn m() -> int { return 2; }",
        );
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        let manifest = manifest_of(&[
            ("mid", "{ path = \"../mid\" }"),
            ("base", "{ path = \"../base\", version = \"=1.0.0\" }"),
        ]);
        let cache = cache_in(&root);
        let err = resolve(&app, &manifest, &cache, true).expect_err("must conflict");
        assert_eq!(err.code, "E0602");
        assert!(
            err.message.contains("mid 1.0.0"),
            "names transitive requirer"
        );
        assert!(err.message.contains("<root>"), "names root requirer");
        assert!(err.message.contains("base"), "names the package");
    }

    #[test]
    fn dependency_cycle_is_loud() {
        let root = scratch("cycle");
        mkpkg(
            &root,
            "aa",
            "1.0.0",
            &[("bb", "{ path = \"../bb\" }")],
            "fn a() -> int { return 1; }",
        );
        mkpkg(
            &root,
            "bb",
            "1.0.0",
            &[("aa", "{ path = \"../aa\" }")],
            "fn b() -> int { return 1; }",
        );
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        let manifest = manifest_of(&[("aa", "{ path = \"../aa\" }")]);
        let cache = cache_in(&root);
        let err = resolve(&app, &manifest, &cache, true).expect_err("must cycle");
        assert_eq!(err.code, "E0607");
        assert!(err.message.contains("aa -> bb -> aa"));
    }

    #[test]
    fn unpinned_sourceless_and_missing_sources_are_loud() {
        let root = scratch("loud");
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        let cache = cache_in(&root);
        // Sourceless.
        let m = manifest_of(&[("foo", "\"1.2.3\"")]);
        let e = resolve(&app, &m, &cache, true).expect_err("sourceless");
        assert_eq!(e.code, "E0606");
        // Unpinned git.
        let m = manifest_of(&[("foo", "{ git = \"https://example.com/f.gzv.git\" }")]);
        let e = resolve(&app, &m, &cache, true).expect_err("unpinned");
        assert_eq!(e.code, "E0605");
        // Path dir without a manifest.
        let m = manifest_of(&[("foo", "{ path = \"../nope\" }")]);
        let e = resolve(&app, &m, &cache, true).expect_err("missing dir");
        assert_eq!(e.code, "E0608");
    }

    #[test]
    fn pkg_file_round_trip_packs_and_resolves() {
        let root = scratch("pkg");
        let dir = mkpkg(&root, "tiny", "0.3.0", &[], "fn t() -> int { return 3; }");
        // Junk that must NOT ship: tests/, target/, toto.lock.
        std::fs::create_dir_all(dir.join("tests")).expect("tests dir");
        std::fs::write(
            dir.join("tests").join("x.gzv"),
            "fn main() -> int { return 9; }",
        )
        .expect("test file");
        let (fname, bytes) = pack_pkg(&dir).expect("packs");
        assert_eq!(fname, "tiny-0.3.0.gzv-pkg");
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        std::fs::write(app.join("tiny-0.3.0.gzv-pkg"), &bytes).expect("stage pkg");
        // Deterministic packing.
        let (_, bytes2) = pack_pkg(&dir).expect("packs again");
        assert_eq!(bytes, bytes2);
        let manifest = manifest_of(&[(
            "tiny",
            "{ path = \"tiny-0.3.0.gzv-pkg\", version = \"^0.3.0\" }",
        )]);
        let cache = cache_in(&root);
        let res = resolve(&app, &manifest, &cache, true).expect("resolves pkg");
        assert_eq!(res.lock.len(), 1);
        assert!(matches!(res.lock[0].source, LockedSource::Pkg { .. }));
        // Unpacked content excludes tests/.
        let unpacked = res.dirs["tiny"].clone();
        assert!(unpacked.join("lib.gzv").is_file());
        assert!(!unpacked.join("tests").exists());
        // Malformed bytes are rejected, never half-unpacked.
        let bad = root.join("bad unpack");
        assert!(unpack_pkg(b"nope", &bad).is_err());
        assert!(unpack_pkg(&bytes[..bytes.len() - 5], &bad).is_err());
    }

    #[test]
    fn edited_dependency_source_fails_verification() {
        let root = scratch("stale");
        mkpkg(&root, "dep", "1.0.0", &[], "fn d() -> int { return 1; }");
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        let manifest = manifest_of(&[("dep", "{ path = \"../dep\" }")]);
        let cache = cache_in(&root);
        let res = resolve(&app, &manifest, &cache, true).expect("resolves");
        write_lockfile(&app, &res.lock).expect("write lock");
        // Edit the dependency source: verification must fail E0604.
        std::fs::write(
            root.join("dep").join("lib.gzv"),
            "fn d() -> int { return 2; }",
        )
        .expect("edit dep");
        let err = ensure(&app, &manifest, &cache).expect_err("must detect change");
        assert_eq!(err.code, "E0604");
        assert!(err.message.contains("dep"));
    }

    #[test]
    fn lockfile_disagreement_is_an_error_not_a_reresolve() {
        let root = scratch("disagree");
        mkpkg(&root, "dep", "1.0.0", &[], "fn d() -> int { return 1; }");
        mkpkg(&root, "dep2", "2.0.0", &[], "fn d() -> int { return 2; }");
        let app = root.join("app");
        std::fs::create_dir_all(&app).expect("app dir");
        // Lock dep@1.0.0 …
        let m1 = manifest_of(&[("dep", "{ path = \"../dep\" }")]);
        let cache = cache_in(&root);
        let res = resolve(&app, &m1, &cache, true).expect("resolves");
        write_lockfile(&app, &res.lock).expect("write lock");
        // … then hand-edit the manifest to a different package:
        // build must ERROR, never silently re-resolve.
        let m2 = manifest_of(&[("dep2", "{ path = \"../dep2\" }")]);
        let err = ensure(&app, &m2, &cache).expect_err("must disagree");
        assert_eq!(err.code, "E0603");
    }
}
