//! Project manifest + `toto init` (Phase 5, Stage 1).
//!
//! A Toto project is a directory containing `toto.toml`:
//!
//! ```toml
//! [project]
//! name = "myapp"
//! version = "0.1.0"
//! entry = "main.gzv"
//!
//! [dependencies]
//! leftpad = "^1.2.3"
//! http = { path = "../http", version = "~2.0.0" }
//! json = { git = "https://example.com/json.gzv.git", rev = "abc123…" }
//! ```
//!
//! `[project]` (name/version/entry, all double-quoted strings) is
//! required; `[dependencies]` is optional and absent means exactly
//! today's behavior (no packages). `#` comments and blank lines
//! allowed. The parser below accepts exactly this shape and rejects
//! anything else with a line-numbered CLI error (exit 3) — it is a
//! strict subset reader, not a general TOML parser, so a future full
//! TOML reader still accepts every manifest written today.
//! `toto build` / `toto test` find the project root by walking up
//! from the working directory to the nearest `toto.toml`.
//!
//! Dependency value forms: a bare version string (`"1.2.3"`, caret;
//! source-less — resolution rejects it, see `packages`), or an
//! inline table with one source (`path` = directory, or a
//! `.gzv-pkg` file unpacked into the package cache at resolve
//! time; `git` = URL plus pinned `rev` or `tag` — a branch,
//! HEAD, or missing pin is rejected at resolve time) and an
//! optional `version` requirement (`^`/`~`/`=`/bare; absent
//! means any version).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::semver::Req;

/// Parsed `toto.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub entry: String,
    /// Declared dependencies in manifest order (sorted by name).
    /// Empty when the manifest has no `[dependencies]` table.
    pub dependencies: BTreeMap<String, DepReq>,
}

/// One `[dependencies]` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepReq {
    /// Where the package comes from. `Bare` (a version string with
    /// no source) is rejected at resolve time, never silently
    /// defaulted.
    pub source: DepSource,
    /// `None` means any version.
    pub version: Option<Req>,
}

/// Dependency source (manifest spelling).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// Bare version string: no source. Always an error at resolve
    /// time (`E0606`) — there is no registry to fetch it from.
    Bare,
    /// Directory (or `.gzv-pkg` file) holding the package.
    Path(String),
    /// Git URL; exactly one of `rev`/`tag` must pin it (checked at
    /// resolve time so `toto add` can report the file and line).
    Git {
        url: String,
        rev: Option<String>,
        tag: Option<String>,
    },
}

/// Failure to read or parse a manifest (CLI error, exit 3).
#[derive(Clone, Debug)]
pub struct ManifestError {
    pub message: String,
}

impl ManifestError {
    fn at(path: &Path, line: usize, message: &str) -> Self {
        Self {
            message: format!(
                "toto: bad manifest `{}` line {line}: {message}",
                path.display()
            ),
        }
    }
}

/// Strict reader for the v1 manifest shape (see module docs).
pub fn parse_manifest(path: &Path, text: &str) -> Result<Manifest, ManifestError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Table {
        Project,
        Deps,
    }
    let mut table: Option<Table> = None;
    let mut seen_project = false;
    let mut seen_deps = false;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut entry: Option<String> = None;
    let mut dependencies: BTreeMap<String, DepReq> = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // A `#` comment may trail the table header.
            let head = line.split('#').next().unwrap_or("").trim();
            if head == "[project]" {
                if seen_project {
                    return Err(ManifestError::at(
                        path,
                        line_no,
                        "duplicate `[project]` table",
                    ));
                }
                seen_project = true;
                table = Some(Table::Project);
                continue;
            }
            if head == "[dependencies]" {
                if seen_deps {
                    return Err(ManifestError::at(
                        path,
                        line_no,
                        "duplicate `[dependencies]` table",
                    ));
                }
                seen_deps = true;
                table = Some(Table::Deps);
                continue;
            }
            return Err(ManifestError::at(
                path,
                line_no,
                "only the `[project]` and `[dependencies]` tables are supported in v1",
            ));
        }
        let Some(table) = table else {
            return Err(ManifestError::at(
                path,
                line_no,
                "keys must live under `[project]` or `[dependencies]`",
            ));
        };
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| ManifestError::at(path, line_no, "expected `key = \"value\"`"))?;
        let key = key.trim();
        match table {
            Table::Project => {
                let slot = match key {
                    "name" => &mut name,
                    "version" => &mut version,
                    "entry" => &mut entry,
                    _ => {
                        return Err(ManifestError::at(
                            path,
                            line_no,
                            &format!(
                                "unsupported manifest key `{key}` (v1 keys: name, version, entry)"
                            ),
                        ));
                    }
                };
                if slot.is_some() {
                    return Err(ManifestError::at(
                        path,
                        line_no,
                        &format!("duplicate key `{key}`"),
                    ));
                }
                *slot = Some(parse_string_value(path, line_no, value)?);
            }
            Table::Deps => {
                if key.is_empty()
                    || !key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(ManifestError::at(
                        path,
                        line_no,
                        "dependency names must be non-empty ascii alphanumeric/`_`/`-`",
                    ));
                }
                if dependencies.contains_key(key) {
                    return Err(ManifestError::at(
                        path,
                        line_no,
                        &format!("duplicate dependency `{key}`"),
                    ));
                }
                dependencies.insert(key.to_string(), parse_dep_value(path, line_no, key, value)?);
            }
        }
    }
    if !seen_project {
        return Err(ManifestError {
            message: format!(
                "toto: bad manifest `{}`: missing `[project]` table",
                path.display()
            ),
        });
    }
    let missing = [("name", &name), ("version", &version), ("entry", &entry)]
        .iter()
        .find_map(|(k, v)| if v.is_none() { Some(*k) } else { None });
    if let Some(k) = missing {
        return Err(ManifestError {
            message: format!("toto: bad manifest `{}`: missing key `{k}`", path.display()),
        });
    }
    Ok(Manifest {
        name: name.expect("checked"),
        version: version.expect("checked"),
        entry: entry.expect("checked"),
        dependencies,
    })
}

/// One `[dependencies]` value: a bare version string (source-less)
/// or an inline table with one source plus an optional version.
fn parse_dep_value(
    path: &Path,
    line_no: usize,
    name: &str,
    raw_value: &str,
) -> Result<DepReq, ManifestError> {
    let err = |msg: &str| ManifestError::at(path, line_no, msg);
    let v = strip_comment(raw_value.trim()).trim();
    if v.starts_with('"') {
        let req =
            Req::parse(&unquote(v).ok_or_else(|| {
                err("expected `\"1.2.3\"` (plain double-quoted string, no escapes)")
            })?)
            .map_err(|e| err(&format!("dependency `{name}`: {e}")))?;
        return Ok(DepReq {
            source: DepSource::Bare,
            version: Some(req),
        });
    }
    if v.starts_with('{') {
        if !v.ends_with('}') {
            return Err(err("expected `{ … }` (inline table with `path` or `git`)"));
        }
        let mut saw_path: Option<String> = None;
        let mut saw_git: Option<String> = None;
        let mut saw_rev: Option<String> = None;
        let mut saw_tag: Option<String> = None;
        let mut saw_version: Option<String> = None;
        for item in split_top_level(&v[1..v.len() - 1]) {
            let (k, val) = item
                .split_once('=')
                .ok_or_else(|| err("expected `key = \"value\"` inside `{ … }`"))?;
            let val = unquote(val.trim())
                .ok_or_else(|| err("expected `key = \"value\"` inside `{ … }`"))?;
            match k.trim() {
                "path" => {
                    if saw_path.is_some() {
                        return Err(err("duplicate key `path`"));
                    }
                    saw_path = Some(val);
                }
                "git" => {
                    if saw_git.is_some() {
                        return Err(err("duplicate key `git`"));
                    }
                    saw_git = Some(val);
                }
                "rev" => {
                    if saw_rev.is_some() {
                        return Err(err("duplicate key `rev`"));
                    }
                    saw_rev = Some(val);
                }
                "tag" => {
                    if saw_tag.is_some() {
                        return Err(err("duplicate key `tag`"));
                    }
                    saw_tag = Some(val);
                }
                "version" => {
                    if saw_version.is_some() {
                        return Err(err("duplicate key `version`"));
                    }
                    saw_version = Some(val);
                }
                other => {
                    return Err(err(&format!(
                        "unsupported dependency key `{}` (v1 keys: path, git, rev, tag, version)",
                        other.trim()
                    )));
                }
            }
        }
        let version = saw_version
            .map(|s| Req::parse(&s).map_err(|e| err(&format!("dependency `{name}`: {e}"))))
            .transpose()?;
        match (saw_path, saw_git) {
            (Some(p), None) => {
                if saw_rev.is_some() || saw_tag.is_some() {
                    return Err(err("`rev`/`tag` need `git`, not `path`"));
                }
                return Ok(DepReq {
                    source: DepSource::Path(p),
                    version,
                });
            }
            (None, Some(url)) => {
                if saw_rev.is_some() && saw_tag.is_some() {
                    return Err(err("`rev` and `tag` are exclusive (pin one revision)"));
                }
                // Unpinned (neither rev nor tag) parses here and is
                // rejected at resolve time (E0605) so `toto add` can
                // report the manifest file and line.
                return Ok(DepReq {
                    source: DepSource::Git {
                        url,
                        rev: saw_rev,
                        tag: saw_tag,
                    },
                    version,
                });
            }
            (Some(_), Some(_)) => {
                return Err(err(
                    "`path` and `git` are exclusive (one source per dependency)",
                ));
            }
            (None, None) => {
                return Err(err("dependency needs exactly one of `path` or `git`"));
            }
        }
    }
    Err(err(
        "expected `\"1.2.3\"` or `{ path = … }` / `{ git = …, rev|tag = … }`",
    ))
}

/// Strip a trailing `#` comment, but only outside double quotes
/// (v1 strings carry no escapes, so quote tracking is exact).
fn strip_comment(s: &str) -> &str {
    let mut in_str = false;
    for (i, b) in s.bytes().enumerate() {
        if b == b'"' {
            in_str = !in_str;
        } else if b == b'#' && !in_str {
            return s[..i].trim_end();
        }
    }
    s
}

/// Split on top-level commas (commas inside double quotes stay;
/// no nesting in v1 inline tables).
fn split_top_level(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_str = false;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'"' => in_str = !in_str,
            b',' if !in_str => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() || !out.is_empty() {
        out.push(tail);
    }
    out
}

/// One plain double-quoted string, no escapes; `None` otherwise.
fn unquote(s: &str) -> Option<String> {
    let body = s.strip_prefix('"').and_then(|s| s.strip_suffix('"'))?;
    if body.contains('"') || body.contains('\\') {
        return None;
    }
    Some(body.to_string())
}

/// One double-quoted string value; no escapes in v1.
fn parse_string_value(path: &Path, line_no: usize, raw: &str) -> Result<String, ManifestError> {
    // Strip a trailing `#` comment only when it starts outside the string.
    let mut value = raw.trim();
    if let Some(q) = value.find('"') {
        let after_open = &value[q + 1..];
        if let Some(end) = after_open.find('"') {
            let rest = after_open[end + 1..].trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err(ManifestError::at(
                    path,
                    line_no,
                    "expected `key = \"value\"` with nothing after the string",
                ));
            }
            value = value[..q + 1 + end + 1].trim();
        }
    }
    let body = value.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
    match body {
        Some(s) if !s.contains('"') && !s.contains('\\') => Ok(s.to_string()),
        _ => Err(ManifestError::at(
            path,
            line_no,
            "expected `key = \"value\"` (plain double-quoted string, no escapes)",
        )),
    }
}

/// Read + parse the manifest at `path`.
pub fn read_manifest(path: &Path) -> Result<Manifest, ManifestError> {
    let text = std::fs::read_to_string(path).map_err(|e| ManifestError {
        message: format!("toto: couldn't read `{}`: {e}", path.display()),
    })?;
    let m = parse_manifest(path, &text)?;
    validate(&m, path)?;
    Ok(m)
}

/// Walk up from `start` to the nearest `toto.toml`; returns its path.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let candidate = dir.join("toto.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Validate manifest values shared by `init` (pre-write) and readers.
fn validate(m: &Manifest, path: &Path) -> Result<(), ManifestError> {
    let bad = |msg: &str| ManifestError {
        message: format!("toto: bad manifest `{}`: {msg}", path.display()),
    };
    if m.name.is_empty()
        || !m
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(bad("`name` must be non-empty ascii alphanumeric/`_`/`-`"));
    }
    if m.version.is_empty() {
        return Err(bad("`version` must be a non-empty string"));
    }
    if m.entry.is_empty() || !m.entry.ends_with(".gzv") {
        return Err(bad("`entry` must be a non-empty `.gzv` path"));
    }
    Ok(())
}

/// Scaffold a runnable project: manifest, entry point, test layout.
/// Refuses when `dir` already exists.
pub fn init_project(dir: &Path, name: &str) -> Result<PathBuf, ManifestError> {
    let m = Manifest {
        name: name.to_string(),
        version: "0.1.0".to_string(),
        entry: "main.gzv".to_string(),
        dependencies: BTreeMap::new(),
    };
    validate(&m, &dir.join("toto.toml"))?;
    if dir.exists() {
        return Err(ManifestError {
            message: format!("toto: `{}` already exists", dir.display()),
        });
    }
    std::fs::create_dir_all(dir.join("tests")).map_err(|e| ManifestError {
        message: format!("toto: couldn't create `{}`: {e}", dir.display()),
    })?;
    let manifest_text =
        format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"main.gzv\"\n");
    std::fs::write(dir.join("toto.toml"), manifest_text).map_err(|e| ManifestError {
        message: format!("toto: couldn't write manifest: {e}"),
    })?;
    std::fs::write(
        dir.join("main.gzv"),
        "fn main() -> int {\n    println(\"hello from toto\");\n    return 0;\n}\n",
    )
    .map_err(|e| ManifestError {
        message: format!("toto: couldn't write entry point: {e}"),
    })?;
    std::fs::write(
        dir.join("tests").join("example.gzv"),
        "fn main() -> int {\n    println(\"example test\");\n    return 0;\n}\n",
    )
    .map_err(|e| ManifestError {
        message: format!("toto: couldn't write example test: {e}"),
    })?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Manifest, ManifestError> {
        parse_manifest(Path::new("toto.toml"), text)
    }

    #[test]
    fn accepts_the_documented_shape() {
        let m = parse("[project]\nname = \"myapp\"\nversion = \"0.1.0\"\nentry = \"main.gzv\"\n")
            .expect("parses");
        assert_eq!(m.name, "myapp");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.entry, "main.gzv");
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn dependencies_table_parses_all_forms() {
        let m = parse(
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n\
             [dependencies]\n\
             bare = \"1.2.3\"\n\
             hp = { path = \"../http\", version = \"~2.0.0\" }\n\
             js = { git = \"https://example.com/j.gzv.git\", rev = \"abc123\" }\n\
             jt = { git = \"https://example.com/j.gzv.git\", tag = \"v1.0.0\" }\n",
        )
        .expect("parses");
        assert_eq!(m.dependencies.len(), 4);
        let bare = &m.dependencies["bare"];
        assert_eq!(bare.source, DepSource::Bare);
        assert_eq!(bare.version, Some(Req::parse("^1.2.3").unwrap()));
        let hp = &m.dependencies["hp"];
        assert_eq!(hp.source, DepSource::Path("../http".to_string()));
        assert_eq!(hp.version, Some(Req::parse("~2.0.0").unwrap()));
        let js = &m.dependencies["js"];
        assert_eq!(
            js.source,
            DepSource::Git {
                url: "https://example.com/j.gzv.git".to_string(),
                rev: Some("abc123".to_string()),
                tag: None,
            }
        );
        assert_eq!(js.version, None);
        // Unpinned parses (rejected at resolve time, E0605).
        let m = parse(
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n\
             [dependencies]\nup = { git = \"https://example.com/u.gzv.git\" }\n",
        )
        .expect("parses");
        assert!(matches!(
            m.dependencies["up"].source,
            DepSource::Git {
                rev: None,
                tag: None,
                ..
            }
        ));
    }

    #[test]
    fn dependencies_rejections_are_line_numbered() {
        for bad in [
            // Unknown table.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[other]\n",
            // Bad dep name.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nno spaces = \"1.0.0\"\n",
            // Duplicate dep.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = \"1.0.0\"\nd = \"1.0.0\"\n",
            // Duplicate table.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\n[dependencies]\n",
            // path + git.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = { path = \"x\", git = \"y\" }\n",
            // rev + tag.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = { git = \"y\", rev = \"r\", tag = \"t\" }\n",
            // rev without git.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = { path = \"x\", rev = \"r\" }\n",
            // no source.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = { version = \"1.0.0\" }\n",
            // bad version req.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = \">=1.0.0\"\n",
            // unknown dep key.
            "[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[dependencies]\nd = { path = \"x\", branch = \"main\" }\n",
        ] {
            let e = parse(bad).expect_err("must reject");
            assert!(e.message.starts_with("toto: bad manifest"), "{bad}");
        }
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let m = parse("# top\n\n[project] # table\nname = \"a\" # trailing\nversion = \"1\"\nentry = \"m.gzv\"\n")
            .expect("parses");
        assert_eq!(m.name, "a");
    }

    #[test]
    fn rejects_unknown_keys_tables_dupes_and_missing() {
        assert!(
            parse("[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\n[other]\n")
                .is_err()
        );
        assert!(
            parse("[project]\nname = \"a\"\nversion = \"1\"\nentry = \"m.gzv\"\nbogus = \"x\"\n")
                .is_err()
        );
        assert!(
            parse("[project]\nname = \"a\"\nname = \"b\"\nversion = \"1\"\nentry = \"m.gzv\"\n")
                .is_err()
        );
        assert!(parse("[project]\nname = \"a\"\nversion = \"1\"\n").is_err());
        assert!(parse("name = \"a\"\n").is_err());
        assert!(parse("[project]\nname = unquoted\nversion = \"1\"\nentry = \"m.gzv\"\n").is_err());
    }
}
