//! Driver version parsing and ordering.
//!
//! Deliberately not a semver dependency: the only thing needed is "is the
//! installed driver at least the one this integration was built against", and
//! the driver reports a plain `major.minor.patch`.

use std::fmt;

/// A `major.minor.patch` driver version. Field order gives the derived `Ord`
/// the right precedence for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Pull a version out of `cua-driver --version` output, e.g.
    /// `cua-driver 0.12.6`. Scans tokens rather than assuming a position, so a
    /// future banner change ("cua-driver v0.13.0 (build 42)") still parses.
    pub fn parse_from_output(text: &str) -> Option<Self> {
        text.split_whitespace()
            .find_map(|token| Self::parse(token.trim_start_matches('v')))
    }

    /// Parse a bare `1.2.3`. Trailing pre-release/build metadata is dropped —
    /// the gate is a floor, and `0.13.0-rc1` should not read as older than
    /// `0.13.0` in a way that blocks a tester.
    pub fn parse(token: &str) -> Option<Self> {
        let core = token
            .split(['-', '+'])
            .next()
            .filter(|s| !s.is_empty())?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        // A fourth component means this is not the shape we think it is.
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_real_version_banner() {
        assert_eq!(
            Version::parse_from_output("cua-driver 0.12.6\n"),
            Some(Version::new(0, 12, 6))
        );
    }

    #[test]
    fn tolerates_a_v_prefix_and_trailing_words() {
        assert_eq!(
            Version::parse_from_output("cua-driver v0.13.0 (build 42)"),
            Some(Version::new(0, 13, 0))
        );
    }

    #[test]
    fn drops_prerelease_metadata() {
        assert_eq!(Version::parse("0.13.0-rc1"), Some(Version::new(0, 13, 0)));
        assert_eq!(Version::parse("0.13.0+abc"), Some(Version::new(0, 13, 0)));
    }

    #[test]
    fn rejects_shapes_that_are_not_a_version() {
        assert_eq!(Version::parse("0.12"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("not-a-version"), None);
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse_from_output("command not found"), None);
    }

    #[test]
    fn orders_by_component_precedence() {
        assert!(Version::new(0, 12, 6) < Version::new(0, 12, 7));
        assert!(Version::new(0, 12, 6) < Version::new(0, 13, 0));
        assert!(Version::new(0, 13, 0) < Version::new(1, 0, 0));
        // The floor comparison the gate actually makes.
        assert!(Version::new(0, 12, 5) < Version::new(0, 12, 6));
        assert!(Version::new(0, 12, 6) >= Version::new(0, 12, 6));
    }

    #[test]
    fn minor_is_compared_numerically_not_lexically() {
        // "0.9.0" vs "0.10.0" is the classic string-compare bug.
        assert!(Version::new(0, 9, 0) < Version::new(0, 10, 0));
    }

    #[test]
    fn displays_round_trip() {
        assert_eq!(Version::new(0, 12, 6).to_string(), "0.12.6");
        assert_eq!(
            Version::parse(&Version::new(1, 2, 3).to_string()),
            Some(Version::new(1, 2, 3))
        );
    }
}
