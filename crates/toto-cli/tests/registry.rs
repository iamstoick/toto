//! Registry tests (Wave 9): index curation + release flow.
//!
//! Hermetic: index tests touch only scratch dirs (no network, no
//! git); release tests use scratch git repos and a local bare
//! remote (no network ever). Follows the packages.rs scratch
//! discipline (pid + atomic counter, cleanup on pass).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("toto-reg-{}-{}-{}", std::process::id(), n, tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn toto() -> Command {
    let bin = env!("CARGO_BIN_EXE_toto");
    Command::new(bin)
}

fn run_toto(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = toto()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("toto runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dirs");
    }
    std::fs::write(path, content).expect("write file");
}

fn mkapp(root: &Path, name: &str, version: &str) -> PathBuf {
    let dir = root.join(name);
    let toml = format!(
        "[project]\nname = \"{}\"\nversion = \"{}\"\nentry = \"main.gzv\"\n",
        name, version
    );
    write(&dir.join("toto.toml"), &toml);
    write(
        &dir.join("main.gzv"),
        "fn main() -> int {\n    println(1);\n    return 0;\n}\n",
    );
    dir
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `git` with hermetic config (never touches global state).
/// Returns (success, stdout); identity comes from `-c` flags or
/// repo-local config set by the test.
fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

fn git_repo(dir: &Path) {
    assert!(git_available(), "git must be on PATH for release tests");
    assert!(git(dir, &["init", "-q", "."]).0, "git init");
    assert!(
        git(dir, &["config", "user.email", "t@t"]).0,
        "git config email"
    );
    assert!(git(dir, &["config", "user.name", "t"]).0, "git config name");
    assert!(git(dir, &["add", "-A"]).0, "git add");
    assert!(git(dir, &["commit", "-qm", "init"]).0, "git commit");
}

#[test]
fn index_add_list_remove_round_trip() {
    let root = scratch("index-roundtrip");
    let app = mkapp(&root, "app", "0.1.0");
    let (code, out, err) = run_toto(
        &app,
        &["index", "add", "leftpad", "https://example.com/leftpad"],
    );
    assert_eq!(code, 0, "add failed: {err}");
    assert!(out.contains("indexed leftpad"), "{out}");
    let text = std::fs::read_to_string(app.join("toto-index.toml")).expect("index written");
    assert!(text.contains("[packages]"), "{text}");
    assert!(
        text.contains("leftpad = \"https://example.com/leftpad\""),
        "{text}"
    );
    // Idempotent re-add of the same mapping.
    let (code, out, _) = run_toto(
        &app,
        &["index", "add", "leftpad", "https://example.com/leftpad"],
    );
    assert_eq!(code, 0);
    assert!(out.contains("already indexed"), "{out}");
    // List shows the project entry.
    let (code, out, _) = run_toto(&app, &["index", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("# ./toto-index.toml"), "{out}");
    assert!(
        out.contains("leftpad = \"https://example.com/leftpad\""),
        "{out}"
    );
    // Remove, then remove again (E0611).
    let (code, out, _) = run_toto(&app, &["index", "remove", "leftpad"]);
    assert_eq!(code, 0);
    assert!(out.contains("removed index entry leftpad"), "{out}");
    let (code, _, err) = run_toto(&app, &["index", "remove", "leftpad"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0611"), "{err}");
    cleanup(&root);
}

#[test]
fn index_add_rejects_bad_shapes_and_rebinds() {
    let root = scratch("index-rejects");
    let app = mkapp(&root, "app", "0.1.0");
    // Bad name (space).
    let (code, _, err) = run_toto(
        &app,
        &["index", "add", "has space", "https://example.com/x"],
    );
    assert_eq!(code, 3);
    assert!(err.contains("E0611"), "{err}");
    // Bad URL (not a remote shape).
    let (code, _, err) = run_toto(&app, &["index", "add", "okname", "notaurl"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0611"), "{err}");
    // Duplicate with a different URL: no silent rebind.
    let (code, _, _) = run_toto(&app, &["index", "add", "pkg", "https://example.com/a"]);
    assert_eq!(code, 0);
    let (code, _, err) = run_toto(&app, &["index", "add", "pkg", "https://example.com/b"]);
    assert_eq!(code, 3);
    assert!(err.contains("already maps to"), "{err}");
    // Usage shapes.
    let (code, _, _) = run_toto(&app, &["index"]);
    assert_eq!(code, 3);
    let (code, _, err) = run_toto(&app, &["index", "frobnicate"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0611"), "{err}");
    let (code, _, _) = run_toto(&app, &["index", "add", "onlyname"]);
    assert_eq!(code, 3);
    cleanup(&root);
}

#[test]
fn index_add_and_remove_preserve_file_bytes() {
    let root = scratch("index-bytes");
    let app = mkapp(&root, "app", "0.1.0");
    write(
        &app.join("toto-index.toml"),
        "# registry\n\n[packages]\n# keep me\nhttp = \"https://example.com/http\"\n",
    );
    let (code, _, err) = run_toto(
        &app,
        &["index", "add", "json", "git@example.com:example/json"],
    );
    assert_eq!(code, 0, "add failed: {err}");
    let text = std::fs::read_to_string(app.join("toto-index.toml")).expect("read");
    assert_eq!(
        text,
        "# registry\n\n[packages]\n# keep me\nhttp = \"https://example.com/http\"\njson = \"git@example.com:example/json\"\n",
        "{text}"
    );
    let (code, _, err) = run_toto(&app, &["index", "remove", "http"]);
    assert_eq!(code, 0, "remove failed: {err}");
    let text = std::fs::read_to_string(app.join("toto-index.toml")).expect("read");
    assert_eq!(
        text, "# registry\n\n[packages]\n# keep me\njson = \"git@example.com:example/json\"\n",
        "{text}"
    );
    cleanup(&root);
}

#[test]
fn release_happy_path_and_idempotent() {
    let root = scratch("release-happy");
    let app = mkapp(&root, "libx", "0.1.0");
    git_repo(&app);
    let (code, out, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 0, "release failed: {err}");
    assert!(out.contains("released libx 0.1.0 (tag v0.1.0)"), "{out}");
    assert!(out.contains("packed libx-0.1.0.gzv-pkg"), "{out}");
    assert!(app.join("libx-0.1.0.gzv-pkg").is_file(), "pack written");
    let tags = git(&app, &["tag", "--list"]);
    assert!(tags.0 && tags.1.contains("v0.1.0"), "{tags:?}");
    // Idempotent re-run (the untracked pack is exempt).
    let (code, out, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 0, "re-release failed: {err}");
    assert!(out.contains("already released libx 0.1.0"), "{out}");
    cleanup(&root);
}

#[test]
fn release_refuses_dirty_tree() {
    let root = scratch("release-dirty");
    let app = mkapp(&root, "app", "0.1.0");
    git_repo(&app);
    write(
        &app.join("main.gzv"),
        "fn main() -> int {\n    return 1;\n}\n",
    );
    let (code, _, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0610"), "{err}");
    assert!(err.contains("not clean"), "{err}");
    cleanup(&root);
}

#[test]
fn release_refuses_drifted_tag() {
    let root = scratch("release-drift");
    let app = mkapp(&root, "app", "0.1.0");
    git_repo(&app);
    let (code, _, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 0, "first release failed: {err}");
    // Same version, different sources: the tag must not move.
    write(
        &app.join("main.gzv"),
        "fn main() -> int {\n    return 2;\n}\n",
    );
    assert!(git(&app, &["add", "-A"]).0);
    assert!(git(&app, &["commit", "-qm", "v2"]).0);
    let (code, _, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0610"), "{err}");
    assert!(err.contains("main.gzv"), "{err}");
    cleanup(&root);
}

#[test]
fn release_refuses_non_repo_and_subdir() {
    let root = scratch("release-norepo");
    let app = mkapp(&root, "app", "0.1.0");
    let (code, _, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0610"), "{err}");
    assert!(err.contains("not a git repo"), "{err}");
    // Project nested inside a repo: the tag would cover the
    // wrong tree.
    let outer = root.join("outer");
    std::fs::create_dir_all(&outer).expect("outer");
    assert!(git(&outer, &["init", "-q", "."]).0);
    let sub = outer.join("sub");
    let app2 = mkapp(&sub, "app", "0.1.0");
    let (code, _, err) = run_toto(&app2, &["release"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0610"), "{err}");
    assert!(err.contains("not the git repo root"), "{err}");
    cleanup(&root);
}

#[test]
fn release_pushes_tag_to_local_remote() {
    let root = scratch("release-push");
    let app = mkapp(&root, "libx", "0.1.0");
    git_repo(&app);
    let bare = root.join("remote.git");
    assert!(git(&root, &["init", "-q", "--bare", "remote.git"]).0);
    assert!(git(&app, &["remote", "add", "origin", &bare.to_string_lossy()]).0);
    assert!(git(&app, &["push", "-q", "origin", "HEAD:main"]).0);
    assert!(
        git(&app, &["branch", "--set-upstream-to=origin/main", "-q"]).0
            || git(&app, &["branch", "--set-upstream-to=origin/main"]).0
    );
    let (code, out, err) = run_toto(&app, &["release", "--push"]);
    assert_eq!(code, 0, "push release failed: {err}");
    assert!(out.contains("pushed v0.1.0 to origin"), "{out}");
    let (ok, tags) = git(&root, &["ls-remote", &bare.to_string_lossy()]);
    assert!(ok && tags.contains("refs/tags/v0.1.0"), "{tags}");
    cleanup(&root);
}

#[test]
fn release_rejects_bad_version() {
    let root = scratch("release-badver");
    let app = mkapp(&root, "app", "not-a-version");
    git_repo(&app);
    let (code, _, err) = run_toto(&app, &["release"]);
    assert_eq!(code, 3);
    assert!(err.contains("E0610"), "{err}");
    cleanup(&root);
}
