//! SemVer versions and requirements for packages (Phase 7, Stage 3).
//!
//! Versions are strict `MAJOR.MINOR.PATCH` numerics: no prerelease,
//! no build metadata (both rejected — v1 has no ordering rule for
//! them, so accepting them would be a silent decision). `0.x` gets
//! NO special-casing: the ordinary caret/tilde rules below apply
//! (which are already restrictive for `0.x`, as specified).
//!
//! Requirements: `^1.2.3` (compatible), `~1.2.3` (patch-level),
//! `=1.2.3` (exact); a bare `1.2.3` means caret (manifest
//! shorthand). Only these operators exist in v1.

use std::cmp::Ordering;
use std::fmt;

/// A validated `MAJOR.MINOR.PATCH` version.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("version `{s}` must be MAJOR.MINOR.PATCH"));
        }
        if s.contains('-') || s.contains('+') {
            return Err(format!(
                "version `{s}` must not carry prerelease or build metadata (v1)"
            ));
        }
        let num = |p: &str| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("version `{s}` has a non-numeric part"));
            }
            p.parse::<u64>()
                .map_err(|_| format!("version `{s}` part out of range"))
        };
        Ok(Version {
            major: num(parts[0])?,
            minor: num(parts[1])?,
            patch: num(parts[2])?,
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A version requirement: caret, tilde, or exact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Req {
    Caret(Version),
    Tilde(Version),
    Exact(Version),
}

impl Req {
    /// Parse `^1.2.3`, `~1.2.3`, `=1.2.3`, or bare `1.2.3` (caret).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix('^') {
            return Ok(Req::Caret(Version::parse(rest)?));
        }
        if let Some(rest) = s.strip_prefix('~') {
            return Ok(Req::Tilde(Version::parse(rest)?));
        }
        if let Some(rest) = s.strip_prefix('=') {
            return Ok(Req::Exact(Version::parse(rest)?));
        }
        Ok(Req::Caret(Version::parse(s).map_err(|e| {
            format!("requirement `{s}`: {e} (want ^x.y.z, ~x.y.z, =x.y.z, or x.y.z)")
        })?))
    }

    pub fn matches(&self, v: Version) -> bool {
        match *self {
            Req::Exact(p) => v == p,
            Req::Caret(p) => {
                if v < p {
                    return false;
                }
                if p.major > 0 {
                    v.major == p.major
                } else if p.minor > 0 {
                    v.major == 0 && v.minor == p.minor
                } else {
                    v == p
                }
            }
            Req::Tilde(p) => v >= p && v.major == p.major && v.minor == p.minor,
        }
    }
}

impl fmt::Display for Req {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Req::Caret(v) => write!(f, "^{v}"),
            Req::Tilde(v) => write!(f, "~{v}"),
            Req::Exact(v) => write!(f, "={v}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("valid version")
    }

    #[test]
    fn versions_parse_and_order() {
        assert!(v("1.2.3") < v("1.10.0"));
        assert_eq!(v("0.0.0").to_string(), "0.0.0");
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("1.2.3.4").is_err());
        assert!(Version::parse("1.2.x").is_err());
        assert!(Version::parse("1.2.3-alpha").is_err());
        assert!(Version::parse("1.2.3+build").is_err());
    }

    #[test]
    fn caret_ranges() {
        let r = Req::parse("^1.2.3").unwrap();
        assert!(r.matches(v("1.2.3")) && r.matches(v("1.9.0")) && !r.matches(v("2.0.0")));
        assert!(!r.matches(v("1.2.2")));
        // 0.x: ordinary rule, already restrictive — no special case.
        let r = Req::parse("^0.2.3").unwrap();
        assert!(r.matches(v("0.2.9")) && !r.matches(v("0.3.0")));
        let r = Req::parse("^0.0.3").unwrap();
        assert!(r.matches(v("0.0.3")) && !r.matches(v("0.0.4")));
        // Bare means caret.
        assert_eq!(Req::parse("1.2.3"), Ok(Req::Caret(v("1.2.3"))));
    }

    #[test]
    fn tilde_and_exact() {
        let r = Req::parse("~1.2.3").unwrap();
        assert!(r.matches(v("1.2.9")) && !r.matches(v("1.3.0")) && !r.matches(v("1.2.2")));
        let r = Req::parse("=1.2.3").unwrap();
        assert!(r.matches(v("1.2.3")) && !r.matches(v("1.2.4")));
        assert!(Req::parse(">=1.2.3").is_err());
        assert!(Req::parse("*").is_err());
    }
}
