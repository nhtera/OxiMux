//! Three-part version numbers, parsed from release tags and bundle plists.

use std::fmt;

/// `major.minor.patch`. Ordering is derived field-by-field, which is exactly
/// semver precedence for the plain triples this app ships (no pre-release
/// labels — the release workflow only tags `vX.Y.Z`).
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

    /// Parse `0.1.3` or `v0.1.3`. Anything else — including a fourth
    /// component or trailing junk — is `None`: the feed tag and the bundle
    /// plist are both under this repo's control, so a shape mismatch means
    /// something is wrong, not that parsing should be lenient.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().strip_prefix('v').unwrap_or_else(|| raw.trim());
        let mut parts = raw.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
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
    fn parses_plain_and_v_prefixed_triples() {
        assert_eq!(Version::parse("0.1.3"), Some(Version::new(0, 1, 3)));
        assert_eq!(Version::parse("v10.2.0"), Some(Version::new(10, 2, 0)));
        assert_eq!(Version::parse(" 1.2.3 "), Some(Version::new(1, 2, 3)));
    }

    #[test]
    fn rejects_malformed_shapes() {
        for bad in ["1.2", "1.2.3.4", "1.2.x", "", "v", "1.2.3-beta"] {
            assert_eq!(Version::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn ordering_is_numeric_not_lexicographic() {
        // The case string comparison gets wrong: 0.10.0 > 0.9.9.
        assert!(Version::new(0, 10, 0) > Version::new(0, 9, 9));
        assert!(Version::new(1, 0, 0) > Version::new(0, 99, 99));
        assert!(Version::new(0, 1, 3) < Version::new(0, 1, 4));
    }
}
