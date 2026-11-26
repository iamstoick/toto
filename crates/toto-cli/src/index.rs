//! Package index: name → git URL mapping (Wave 7).
//!
//! Bare version requirements (`http = "2"`) name no source, so
//! the resolver consults an index: `./toto-index.toml` in the
//! project root first (checked in, wins on conflict), then a
//! per-user `~/.toto/index.toml` (`$XDG_CONFIG_HOME` honored
//! the same way the package cache honors `$XDG_CACHE_HOME`).
//! Either file is optional — a missing file is an empty index,
//! never an error. Malformed files are loud errors with a path
//! and line number, same as `toto.toml` itself.
//!
//! Format is a strict subset, parsed by hand like the manifest
//! (no new dependency):
//!
//! ```toml
//! # comment lines and blanks allowed
//! [packages]
//!//! http = "https://github.com/example/toto-http"
//! ```
//!
//! The index maps names to git URLs ONLY — versions stay in
//! requirements, pins stay in the lockfile. Nothing here is
//! networked: reads are local files, consulted at add/update
//! time; builds never read the index (the lockfile is the
//! only source of versions at build time).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::packages::PkgError;

/// Name → git URL, in first-wins order (project, then user).
#[derive(Default)]
pub struct Index {
    map: BTreeMap<String, String>,
}

impl Index {
    /// Look up a package name. Project entries shadow user ones
    /// because the caller merges project-first.
    pub fn lookup(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(String::as_str)
    }

    fn insert(
        &mut self,
        name: String,
        url: String,
        path: &Path,
        line: usize,
    ) -> Result<(), PkgError> {
        if name.is_empty() || url.is_empty() {
            return Err(PkgError::new(
                "E0608",
                format!("{}:{line}: empty name or url", path.display()),
            ));
        }
        // First wins: project entries are inserted before user
        // ones, so a repeated name keeps the project mapping.
        self.map.entry(name).or_insert(url);
        Ok(())
    }
}

/// Read one index file; a missing file is an empty index.
fn load_file(path: &Path) -> Result<Index, PkgError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
        Err(e) => {
            return Err(PkgError::new(
                "E0608",
                format!("cannot read index `{}`: {e}", path.display()),
            ));
        }
    };
    parse_index(path, &text)
}

/// Parse the strict `[packages]` subset: section header, then
/// `name = "url"` lines; `#` comments and blank lines ignored.
fn parse_index(path: &Path, text: &str) -> Result<Index, PkgError> {
    let mut index = Index::default();
    let mut in_packages = false;
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_packages = line == "[packages]";
            if !in_packages && line.ends_with(']') {
                return Err(PkgError::new(
                    "E0608",
                    format!(
                        "{}:{line_no}: only the `[packages]` section is supported",
                        path.display()
                    ),
                ));
            }
            if !line.ends_with(']') {
                return Err(PkgError::new(
                    "E0608",
                    format!("{}:{line_no}: malformed section header", path.display()),
                ));
            }
            continue;
        }
        if !in_packages {
            return Err(PkgError::new(
                "E0608",
                format!(
                    "{}:{line_no}: entries must live under `[packages]`",
                    path.display()
                ),
            ));
        }
        let (name, url) = split_entry(line).ok_or_else(|| {
            PkgError::new(
                "E0608",
                format!("{}:{line_no}: want `name = \"git-url\"`", path.display()),
            )
        })?;
        index.insert(name, url, path, line_no)?;
    }
    Ok(index)
}

/// Split `name = "url"` (double quotes required, like the
/// manifest reader; no escapes — URLs do not need them).
fn split_entry(line: &str) -> Option<(String, String)> {
    let (name, rest) = line.split_once('=')?;
    let name = name.trim();
    let url = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
    if name.is_empty() || url.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((name.to_string(), url.to_string()))
}

/// `~/.toto/index.toml` (`$XDG_CONFIG_HOME` honored); `None`
/// when no home is known (then there is simply no user index).
fn user_index_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("toto").join("index.toml"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".toto").join("index.toml"))
}

/// Test hook: user index under an explicit home (keeps unit
/// tests hermetic without touching process-global `HOME`).
#[cfg(test)]
pub fn user_index_path_at(home: Option<&str>) -> Option<PathBuf> {
    match home {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h).join(".toto").join("index.toml")),
        _ => None,
    }
}

/// Both indexes together, loaded once per resolve. Lookup
/// checks the project file first (checked in, wins on
/// conflict), then the user file.
pub struct Indexes {
    project: Index,
    user: Index,
}

impl Indexes {
    pub fn load(root: &Path) -> Result<Self, PkgError> {
        let (project, user) = load_index_pair(root)?;
        Ok(Indexes { project, user })
    }

    /// Git URL for a bare package name, if either index maps it.
    pub fn lookup(&self, name: &str) -> Option<&str> {
        self.project.lookup(name).or_else(|| self.user.lookup(name))
    }

    #[cfg(test)]
    pub fn for_tests(project: Index, user: Index) -> Self {
        Indexes { project, user }
    }
}

/// Load both index files: `./toto-index.toml` under the project
/// root, then the user file. Missing files are empty indexes
/// (never an error); malformed files are loud errors.
fn load_index_pair(root: &Path) -> Result<(Index, Index), PkgError> {
    let project = load_file(&root.join("toto-index.toml"))?;
    let user = match user_index_path() {
        Some(p) => load_file(&p)?,
        None => Index::default(),
    };
    Ok((project, user))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, url: &str) -> (String, String) {
        (name.to_string(), url.to_string())
    }

    #[test]
    fn parses_table_ignoring_comments_and_blank_lines() {
        let text = "# registry\n\n[packages]\n# a comment\nhttp = \"https://example.com/http\"\n\njson = \"https://example.com/json\"\n";
        let idx = parse_index(Path::new("toto-index.toml"), text).expect("parses");
        assert_eq!(idx.lookup("http"), Some("https://example.com/http"));
        assert_eq!(idx.lookup("json"), Some("https://example.com/json"));
        assert_eq!(idx.lookup("missing"), None);
        assert!(idx.lookup("http").is_some());
    }

    #[test]
    fn rejects_unknown_sections_and_bad_lines() {
        assert!(parse_index(Path::new("i"), "[other]\nhttp = \"u\"\n").is_err());
        assert!(parse_index(Path::new("i"), "http = no-quotes\n").is_err());
        assert!(parse_index(Path::new("i"), "just-a-name\n").is_err());
        assert!(parse_index(Path::new("i"), "[packages\nhttp = \"u\"\n").is_err());
    }

    #[test]
    fn empty_file_is_empty_index() {
        let idx = parse_index(Path::new("i"), "# nothing here\n\n").expect("parses");
        assert_eq!(idx.lookup("anything"), None);
        assert_eq!(idx.lookup("http"), None);
    }

    #[test]
    fn first_write_wins_within_one_file() {
        let text =
            "[packages]\nhttp = \"https://first.example/x\"\nhttp = \"https://second.example/x\"\n";
        let idx = parse_index(Path::new("i"), text).expect("parses");
        assert_eq!(idx.lookup("http"), Some("https://first.example/x"));
    }

    #[test]
    fn user_lookup_paths() {
        assert_eq!(
            user_index_path_at(Some("/home/u")),
            Some(PathBuf::from("/home/u/.toto/index.toml"))
        );
        assert_eq!(user_index_path_at(None), None);
        assert_eq!(user_index_path_at(Some("")), None);
    }

    #[test]
    fn project_shadows_user() {
        let (mut project, mut user) = (Index::default(), Index::default());
        project
            .map
            .insert(named("a", "project-url").0, named("a", "project-url").1);
        user.map.insert("a".to_string(), "user-url".to_string());
        user.map.insert("b".to_string(), "user-b".to_string());
        let both = Indexes::for_tests(project, user);
        assert_eq!(both.lookup("a"), Some("project-url"));
        assert_eq!(both.lookup("b"), Some("user-b"));
        assert_eq!(both.lookup("c"), None);
    }
}
