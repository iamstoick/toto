//! Project manifest + `toto init` (Phase 5, Stage 1).
//!
//! A Toto project is a directory containing `toto.toml`:
//!
//! ```toml
//! [project]
//! name = "myapp"
//! version = "0.1.0"
//! entry = "main.gzv"
//! ```
//!
//! That shape is the whole v1 manifest format: a single `[project]`
//! table with exactly the keys `name`, `version`, `entry`, all
//! double-quoted strings, `#` comments and blank lines allowed. The
//! parser below accepts exactly this shape and rejects anything else
//! with a line-numbered CLI error (exit 3) — it is a strict subset
//! reader, not a general TOML parser, so a future full TOML reader
//! still accepts every manifest written today. `toto build` / `toto
//! test` find the project root by walking up from the working
//! directory to the nearest `toto.toml`.

use std::path::{Path, PathBuf};

/// Parsed `toto.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub entry: String,
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
    let mut in_project = false;
    let mut seen_project = false;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut entry: Option<String> = None;
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
                in_project = true;
                continue;
            }
            return Err(ManifestError::at(
                path,
                line_no,
                "only the `[project]` table is supported in v1",
            ));
        }
        if !in_project {
            return Err(ManifestError::at(
                path,
                line_no,
                "keys must live under `[project]`",
            ));
        }
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| ManifestError::at(path, line_no, "expected `key = \"value\"`"))?;
        let key = key.trim();
        let slot = match key {
            "name" => &mut name,
            "version" => &mut version,
            "entry" => &mut entry,
            _ => {
                return Err(ManifestError::at(
                    path,
                    line_no,
                    &format!("unsupported manifest key `{key}` (v1 keys: name, version, entry)"),
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
    })
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
