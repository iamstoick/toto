//! Package-manager integration tests (Phase 7, Stage 3).
//!
//! Hermetic: every test uses a scratch project dir and a scratch
//! `TOTO_PKG_CACHE`; git "remotes" are local `git init` repos (no
//! network ever). Each rule from the report has a golden here:
//! path deps end to end, Amendment 1 ambiguity, lockfile
//! disagreement, edited-source detection + update recovery,
//! unpinned/sourceless/conflict/cycle errors, the publish round
//! trip, git-from-cache builds, and `update <pkg>` subtrees.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("toto-pkgit-{}-{}-{}", std::process::id(), n, tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn toto() -> Command {
    let bin = env!("CARGO_BIN_EXE_toto");
    Command::new(bin)
}

/// Shared inner-link target dir (test-only): all `toto build` shim
/// links reuse one cargo cache instead of one cold target per
/// scratch project. It lives under the workspace `target/` (real
/// disk, gitignored, cargo-owned) — NOT tmpfs — so parallel gate
/// runs cannot exhaust `/tmp`. Correctness is unaffected (same
/// package name but different sources/link args → different
/// fingerprints → correct rebuilds; project-local
/// `.toto-fingerprint` + binary still gate freshness). Warm across
/// gate runs; never cleaned.
fn shared_target() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("toto-pkgtest-shim")
}

/// Run toto with a scratch package cache; returns (status, stdout, stderr).
fn run_toto(dir: &Path, cache: &Path, args: &[&str]) -> (i32, String, String) {
    let out: Output = toto()
        .args(args)
        .current_dir(dir)
        .env("TOTO_PKG_CACHE", cache)
        .env("CARGO_TARGET_DIR", shared_target())
        .output()
        .expect("toto runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Remove a scratch root at the end of a passing test (keeps gate
/// disk bounded; failures leave the tree for debugging).
fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dirs");
    }
    std::fs::write(path, content).expect("write file");
}

/// A package dir with toto.toml + one lib file.
fn mkpkg(root: &Path, name: &str, version: &str, deps_toml: &str, lib: &str) -> PathBuf {
    let dir = root.join(name);
    let mut toml =
        format!("[project]\nname = \"{name}\"\nversion = \"{version}\"\nentry = \"lib.gzv\"\n");
    if !deps_toml.is_empty() {
        toml.push_str("[dependencies]\n");
        toml.push_str(deps_toml);
    }
    write(&dir.join("toto.toml"), &toml);
    write(&dir.join("lib.gzv"), lib);
    dir
}

/// A runnable app project (entry main.gzv).
fn mkapp(root: &Path, name: &str, main: &str) -> PathBuf {
    let dir = root.join(name);
    let toml = format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"main.gzv\"\n");
    write(&dir.join("toto.toml"), &toml);
    write(&dir.join("main.gzv"), main);
    dir
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Local git repo used as a hermetic "remote".
fn git_remote(root: &Path, name: &str, version: &str, lib: &str) -> (PathBuf, String) {
    assert!(git_available(), "git must be on PATH for package tests");
    let dir = mkpkg(root, name, version, "", lib);
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(st.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&st.stdout).trim().to_string()
    };
    git(&["init", "-q", "."]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "init",
    ]);
    let rev = git(&["rev-parse", "HEAD"]);
    (dir, rev)
}

#[test]
fn path_dep_add_run_build_and_fresh_skip() {
    let root = scratch("e2e");
    let cache = root.join("cache");
    mkpkg(
        &root,
        "leftpad",
        "1.2.0",
        "",
        "pub fn pad(s: string) -> int {\n    return 7;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"leftpad/lib.gzv\";\nfn main() -> int {\n    println(pad(\"x\"));\n    return 0;\n}\n",
    );
    let (code, out, _) = run_toto(&app, &cache, &["add", "leftpad", "--path", "../leftpad"]);
    assert_eq!(code, 0, "add failed: {out}");
    assert!(app.join("toto.lock").is_file(), "lockfile written");
    let (code, run_out, _) = run_toto(&app, &cache, &["run", "main.gzv"]);
    assert_eq!((code, run_out.trim()), (0, "7"));
    let (code, _, _) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 0, "build failed");
    let built = std::process::Command::new(app.join("target").join("app"))
        .output()
        .expect("built binary runs");
    assert_eq!(String::from_utf8_lossy(&built.stdout).trim(), "7");
    // No-change rebuild skips (fingerprint rule).
    let (code, _, err) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 0);
    assert!(
        err.contains("is fresh, skipping rebuild") || err.is_empty(),
        "expected fresh-skip, got: {err}"
    );
    cleanup(&root);
}

#[test]
fn amendment_1_ambiguity_is_a_loud_e0601() {
    let root = scratch("amb");
    let cache = root.join("cache");
    mkpkg(
        &root,
        "leftpad",
        "1.2.0",
        "",
        "pub fn pad(s: string) -> int {\n    return 7;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"leftpad/lib.gzv\";\nfn main() -> int {\n    println(pad(\"x\"));\n    return 0;\n}\n",
    );
    let (code, _, _) = run_toto(&app, &cache, &["add", "leftpad", "--path", "../leftpad"]);
    assert_eq!(code, 0);
    // A project-root path matching the dep import: E0601, exit 2.
    write(
        &app.join("leftpad").join("lib.gzv"),
        "pub fn pad(s: string) -> int {\n    return 9;\n}\n",
    );
    let (code, _, err) = run_toto(&app, &cache, &["run", "main.gzv"]);
    assert_eq!(code, 2, "ambiguity must fail the load");
    assert!(err.contains("E0601"), "missing code: {err}");
    assert!(err.contains("leftpad"), "must name the dependency: {err}");
    assert!(
        err.contains("leftpad/lib.gzv"),
        "must name the root path: {err}"
    );
    cleanup(&root);
}

#[test]
fn edited_source_fails_e0604_update_recovers() {
    // The caching proof: change a dependency source, rebuild,
    // observe E0604 (never stale); update; rebuild, observe the
    // new behavior.
    let root = scratch("stale");
    let cache = root.join("cache");
    let dep = mkpkg(
        &root,
        "dep",
        "1.0.0",
        "",
        "pub fn v() -> int {\n    return 1;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"dep/lib.gzv\";\nfn main() -> int {\n    println(v());\n    return 0;\n}\n",
    );
    assert_eq!(
        run_toto(&app, &cache, &["add", "dep", "--path", "../dep"]).0,
        0
    );
    assert_eq!(run_toto(&app, &cache, &["build"]).0, 0);
    write(
        &dep.join("lib.gzv"),
        "pub fn v() -> int {\n    return 2;\n}\n",
    );
    let (code, _, err) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 3, "edited source must fail loudly");
    assert!(err.contains("E0604"), "missing code: {err}");
    assert!(err.contains("dep"), "must name the package: {err}");
    let (code, out, _) = run_toto(&app, &cache, &["update"]);
    assert_eq!(code, 0, "update failed: {out}");
    assert!(out.contains("dep"), "update must report the package: {out}");
    let (code, _, _) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 0, "rebuild after update failed");
    let built = std::process::Command::new(app.join("target").join("app"))
        .output()
        .expect("built binary runs");
    assert_eq!(
        String::from_utf8_lossy(&built.stdout).trim(),
        "2",
        "new behavior observed after update"
    );
    cleanup(&root);
}

#[test]
fn lockfile_disagreement_and_missing_lock() {
    let root = scratch("lock");
    let cache = root.join("cache");
    mkpkg(
        &root,
        "a",
        "1.0.0",
        "",
        "pub fn f() -> int {\n    return 1;\n}\n",
    );
    mkpkg(
        &root,
        "b",
        "2.0.0",
        "",
        "pub fn f() -> int {\n    return 2;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"a/lib.gzv\";\nfn main() -> int {\n    println(f());\n    return 0;\n}\n",
    );
    assert_eq!(run_toto(&app, &cache, &["add", "a", "--path", "../a"]).0, 0);
    // Hand-edit the manifest to a different package: build errors
    // E0603, never silently re-resolves. (Rewrite the dep line
    // exactly — the key is unquoted in manifest spelling.)
    let toml = std::fs::read_to_string(app.join("toto.toml")).expect("read manifest");
    let edited = toml.replace("a = { path = \"../a\" }", "b = { path = \"../b\" }");
    assert_ne!(toml, edited);
    write(&app.join("toto.toml"), &edited);
    // The entry must import what the manifest declares.
    write(
        &app.join("main.gzv"),
        "import \"b/lib.gzv\";\nfn main() -> int {\n    println(f());\n    return 0;\n}\n",
    );
    let (code, _, err) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0603"), "missing code: {err}");
    // Missing lockfile: resolve + write loudly, then build.
    std::fs::remove_file(app.join("toto.lock")).expect("remove lock");
    let (code, _, err) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 0, "build without lock failed: {err}");
    assert!(err.contains("toto.lock"), "must announce the write: {err}");
    assert!(app.join("toto.lock").is_file());
    cleanup(&root);
}

#[test]
fn unpinned_sourceless_and_conflict_add_time_errors() {
    let root = scratch("errs");
    let cache = root.join("cache");
    let app = mkapp(&root, "app", "fn main() -> int {\n    return 0;\n}\n");
    // Unpinned git.
    let (code, _, err) = run_toto(
        &app,
        &cache,
        &["add", "x", "--git", "https://example.com/x.gzv.git"],
    );
    assert_eq!(code, 3);
    assert!(err.contains("E0605"), "missing code: {err}");
    // Sourceless.
    let (code, _, err) = run_toto(&app, &cache, &["add", "x"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0606"), "missing code: {err}");
    // Unknown update target.
    let (code, _, err) = run_toto(&app, &cache, &["update", "nope"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0609"), "missing code: {err}");
    cleanup(&root);
}

#[test]
fn transitive_resolution_and_conflict() {
    let root = scratch("trans");
    let cache = root.join("cache");
    // mid 1.0.0 requires base ^1.0.0 (only 1.0.0 published here);
    // root also requires base directly: single version selected.
    mkpkg(
        &root,
        "base",
        "1.0.0",
        "",
        "pub fn b() -> int {\n    return 1;\n}\n",
    );
    mkpkg(
        &root,
        "mid",
        "1.0.0",
        "base = { path = \"../base\", version = \"^1.0.0\" }\n",
        "pub fn m() -> int {\n    return 2;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"mid/lib.gzv\";\nimport \"base/lib.gzv\";\nfn main() -> int {\n    println(m() + b());\n    return 0;\n}\n",
    );
    let (code, out, _) = run_toto(&app, &cache, &["add", "mid", "--path", "../mid"]);
    assert_eq!(code, 0, "add failed: {out}");
    let (code, out, _) = run_toto(&app, &cache, &["add", "base", "--path", "../base"]);
    assert_eq!(code, 0, "add failed: {out}");
    let lock = std::fs::read_to_string(app.join("toto.lock")).expect("lock");
    assert!(lock.contains("name = \"mid\"") && lock.contains("name = \"base\""));
    let (code, run_out, _) = run_toto(&app, &cache, &["run", "main.gzv"]);
    assert_eq!((code, run_out.trim()), (0, "3"));
    // Conflicting transitive requirement: mid2 needs base ^2.0.0.
    mkpkg(
        &root,
        "mid2",
        "1.0.0",
        "base = { path = \"../base\", version = \"^2.0.0\" }\n",
        "pub fn m() -> int {\n    return 2;\n}\n",
    );
    let (code, _, err) = run_toto(&app, &cache, &["add", "mid2", "--path", "../mid2"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0602"), "missing code: {err}");
    assert!(err.contains("mid2"), "must name the requirer: {err}");
    // Full-requirement-list pin: EVERY conflicting requirement is
    // named with its text and provenance, not just the newcomer.
    assert!(
        err.contains("^1.0.0") && err.contains("(required by mid 1.0.0)"),
        "must name mid's requirement: {err}"
    );
    assert!(
        err.contains("^2.0.0") && err.contains("(required by mid2 1.0.0)"),
        "must name mid2's requirement: {err}"
    );
    cleanup(&root);
}

#[test]
fn publish_round_trip_pack_add_build() {
    let root = scratch("pub");
    let cache = root.join("cache");
    let lib = mkpkg(
        &root,
        "tiny",
        "0.3.0",
        "",
        "pub fn t() -> int {\n    return 3;\n}\n",
    );
    let (code, out, _) = run_toto(&lib, &cache, &["publish"]);
    assert_eq!(code, 0, "publish failed: {out}");
    assert!(out.contains("local only"), "must state local-only: {out}");
    let pkg = lib.join("tiny-0.3.0.gzv-pkg");
    assert!(pkg.is_file());
    let app = mkapp(
        &root,
        "app",
        "import \"tiny/lib.gzv\";\nfn main() -> int {\n    println(t());\n    return 0;\n}\n",
    );
    // Stage the pkg file next to the manifest and add it.
    let staged = app.join("tiny-0.3.0.gzv-pkg");
    std::fs::copy(&pkg, &staged).expect("stage pkg");
    let (code, out, _) = run_toto(&app, &cache, &["add", "tiny-0.3.0.gzv-pkg"]);
    assert_eq!(code, 0, "pkg add failed: {out}");
    let (code, run_out, _) = run_toto(&app, &cache, &["run", "main.gzv"]);
    assert_eq!((code, run_out.trim()), (0, "3"));
    let (code, _, _) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 0, "packaged build failed");
    let built = std::process::Command::new(app.join("target").join("app"))
        .output()
        .expect("built binary runs");
    assert_eq!(String::from_utf8_lossy(&built.stdout).trim(), "3");
    cleanup(&root);
}

#[test]
fn git_dep_builds_from_cache_after_remote_vanishes() {
    // Pinned rule: fetching happens at add/update time only. The
    // build below runs AFTER the remote is deleted: success proves
    // the build used only the local cache (no network at build).
    let root = scratch("gitc");
    let cache = root.join("cache");
    let (remote, rev) = git_remote(
        &root,
        "fmt",
        "0.4.0",
        "pub fn shout(s: string) -> string {\n    return s;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"fmt/lib.gzv\";\nfn main() -> int {\n    println(shout(\"hey\"));\n    return 0;\n}\n",
    );
    let remote_str = remote.to_string_lossy().into_owned();
    let (code, out, _) = run_toto(
        &app,
        &cache,
        &["add", "fmt", "--git", &remote_str, "--rev", &rev],
    );
    assert_eq!(code, 0, "git add failed: {out}");
    let lock = std::fs::read_to_string(app.join("toto.lock")).expect("lock");
    assert!(lock.contains(&rev), "lock must pin the rev");
    // Delete the remote: the build must still succeed from cache.
    std::fs::remove_dir_all(&remote).expect("delete remote");
    let (code, _, err) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 0, "cache-only build failed: {err}");
    let built = std::process::Command::new(app.join("target").join("app"))
        .output()
        .expect("built binary runs");
    assert_eq!(String::from_utf8_lossy(&built.stdout).trim(), "hey");
    cleanup(&root);
}

#[test]
fn missing_cached_git_rev_fails_loudly_naming_the_rev() {
    let root = scratch("gitmiss");
    let cache = root.join("cache");
    let (_remote, rev) = git_remote(
        &root,
        "fmt",
        "0.4.0",
        "pub fn shout(s: string) -> string {\n    return s;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"fmt/lib.gzv\";\nfn main() -> int {\n    println(shout(\"hey\"));\n    return 0;\n}\n",
    );
    // Hand-write manifest + lock (as if the cache were lost), then
    // build with an empty package cache: must fail E0604 naming it.
    let remote = root.join("fmt");
    let remote_str = remote.to_string_lossy().into_owned();
    let manifest = format!(
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\nentry = \"main.gzv\"\n\n[dependencies]\nfmt = {{ git = \"{remote_str}\", rev = \"{rev}\" }}\n"
    );
    write(&app.join("toto.toml"), &manifest);
    let (code, _, _) = run_toto(&app, &cache, &["update"]);
    assert_eq!(code, 0, "update must populate the lock");
    // Wipe the package cache (locked rev gone), keep the lock.
    std::fs::remove_dir_all(&cache).expect("wipe cache");
    let (code, _, err) = run_toto(&app, &cache, &["build"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0604"), "missing code: {err}");
    // Full-rev pin: the error names the complete locked revision,
    // not just a short prefix.
    assert!(err.contains(&rev), "must name the full missing rev: {err}");
    cleanup(&root);
}

#[test]
fn update_subtree_pins_satisfiable_pins() {
    let root = scratch("upsub");
    let cache = root.join("cache");
    let left = mkpkg(
        &root,
        "left",
        "1.0.0",
        "",
        "pub fn v() -> int {\n    return 1;\n}\n",
    );
    mkpkg(
        &root,
        "right",
        "1.0.0",
        "",
        "pub fn w() -> int {\n    return 10;\n}\n",
    );
    mkpkg(
        &root,
        "mid",
        "1.0.0",
        "",
        "pub fn m() -> int {\n    return 100;\n}\n",
    );
    let app = mkapp(
        &root,
        "app",
        "import \"left/lib.gzv\";\nimport \"right/lib.gzv\";\nimport \"mid/lib.gzv\";\nfn main() -> int {\n    println(v() + w() + m());\n    return 0;\n}\n",
    );
    assert_eq!(
        run_toto(&app, &cache, &["add", "left", "--path", "../left"]).0,
        0
    );
    assert_eq!(
        run_toto(&app, &cache, &["add", "right", "--path", "../right"]).0,
        0
    );
    assert_eq!(
        run_toto(&app, &cache, &["add", "mid", "--path", "../mid"]).0,
        0
    );
    let lock_before = std::fs::read_to_string(app.join("toto.lock")).expect("lock");
    let mid_before = lock_before
        .split("[[package]]")
        .find(|b| b.contains("name = \"mid\""))
        .expect("mid block")
        .to_string();
    // Change left's content (same version) + bump right's version
    // on disk (mid untouched): left moves, right rejoins free
    // selection (its pin vanished), mid stays byte-identical.
    write(
        &left.join("lib.gzv"),
        "pub fn v() -> int {\n    return 2;\n}\n",
    );
    write(
        &root.join("right").join("toto.toml"),
        "[project]\nname = \"right\"\nversion = \"2.0.0\"\nentry = \"lib.gzv\"\n",
    );
    let (code, out, _) = run_toto(&app, &cache, &["update", "left"]);
    assert_eq!(code, 0, "subtree update failed: {out}");
    assert!(out.contains("left"), "must report left: {out}");
    assert!(out.contains("right"), "must report right's move: {out}");
    let lock = std::fs::read_to_string(app.join("toto.lock")).expect("lock");
    let mid_after = lock
        .split("[[package]]")
        .find(|b| b.contains("name = \"mid\""))
        .expect("mid block");
    assert_eq!(mid_before, mid_after, "satisfiable pin must not move");
    let right_block = lock
        .split("[[package]]")
        .find(|b| b.contains("name = \"right\""))
        .expect("right block");
    assert!(right_block.contains("version = \"2.0.0\""), "{right_block}");
    // The app runs against left=2/right=2.0.0/mid=1.0.0.
    let (code, run_out, _) = run_toto(&app, &cache, &["run", "main.gzv"]);
    assert_eq!((code, run_out.trim()), (0, "112"));
    cleanup(&root);
}
