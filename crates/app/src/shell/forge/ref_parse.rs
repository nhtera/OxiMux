//! Parse a pasted forge reference — issue/PR/MR URL or `#123` — into the
//! pieces the title prefill needs. Pure string work (no regex crate, no
//! IO) so every form is unit-testable.

use oximux_core::ForgeRefKind;

/// A recognized forge reference from the workspace-name field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedForgeRef {
    pub kind: ForgeRefKind,
    pub number: u32,
    /// Repository the URL points at (`owner/repo` for GitHub, the full
    /// `group/.../project` path for GitLab). `None` for `#N` / bare-number
    /// forms — those resolve against the active project's own forge.
    pub repo: Option<String>,
}

/// Longest issue/PR number accepted after `#`. Generous for real trackers
/// while rejecting things like pasted timestamps.
const MAX_BARE_DIGITS: usize = 7;

/// Recognize a forge reference. Returns `None` for anything that doesn't
/// confidently parse — ordinary workspace names must never be mistaken for
/// a reference, so every path here errs toward `None`.
pub fn parse_forge_ref(input: &str) -> Option<ParsedForgeRef> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // URL forms first — most specific.
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return parse_forge_url(trimmed);
    }

    // `#123` — the hash is the explicit "this is a ticket" signal. Bare
    // digits deliberately do NOT parse: short numeric workspace names
    // ("2024", "100") are common, and silently replacing one with an
    // unrelated issue title would be a nasty surprise.
    if let Some(rest) = trimmed.strip_prefix('#') {
        let number = parse_bare_number(rest)?;
        return Some(ParsedForgeRef {
            kind: ForgeRefKind::Issue,
            number,
            repo: None,
        });
    }

    None
}

fn parse_bare_number(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > MAX_BARE_DIGITS || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = s.parse().ok()?;
    (n > 0).then_some(n)
}

/// URL forms:
///   github.com/{owner}/{repo}/issues/{n}            → Issue
///   github.com/{owner}/{repo}/pull/{n}              → Pull
///   {gitlab-host}/{path…}/-/issues/{n}              → Issue
///   {gitlab-host}/{path…}/-/merge_requests/{n}      → Pull
/// GitLab's `/-/` separator cleanly splits the project path from the object
/// route, so nested groups parse without guessing.
fn parse_forge_url(url: &str) -> Option<ParsedForgeRef> {
    let after_scheme = url.split_once("://")?.1;
    let (host, path) = after_scheme.split_once('/')?;
    let path = path.trim_end_matches('/');

    // GitLab routes (`/-/` separator) — host must look like a GitLab to
    // avoid claiming arbitrary URLs; same heuristic as remote detection
    // (self-hosted instances without "gitlab" in the domain stay
    // unrecognized, documented limitation).
    if host.contains("gitlab") {
        for (marker, kind) in [
            ("/-/issues/", ForgeRefKind::Issue),
            ("/-/merge_requests/", ForgeRefKind::Pull),
        ] {
            if let Some((project_path, tail)) = path.split_once(marker) {
                let number = parse_leading_number(tail)?;
                return Some(ParsedForgeRef {
                    kind,
                    number,
                    repo: Some(project_path.to_string()),
                });
            }
        }
        return None;
    }

    if host == "github.com" || host == "www.github.com" {
        let mut segments = path.split('/');
        let owner = segments.next().filter(|s| !s.is_empty())?;
        let repo = segments.next().filter(|s| !s.is_empty())?;
        let route = segments.next()?;
        let kind = match route {
            "issues" => ForgeRefKind::Issue,
            "pull" => ForgeRefKind::Pull,
            _ => return None,
        };
        let number = parse_leading_number(segments.next()?)?;
        return Some(ParsedForgeRef {
            kind,
            number,
            repo: Some(format!("{owner}/{repo}")),
        });
    }

    None
}

/// Parse the number segment, tolerating trailing route noise
/// (`123#issuecomment-…`, `123?query`, `123/files`).
fn parse_leading_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (n > 0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(kind: ForgeRefKind, number: u32, repo: Option<&str>) -> ParsedForgeRef {
        ParsedForgeRef {
            kind,
            number,
            repo: repo.map(str::to_string),
        }
    }

    #[test]
    fn github_issue_and_pull_urls() {
        assert_eq!(
            parse_forge_ref("https://github.com/foo/bar/issues/42"),
            Some(parsed(ForgeRefKind::Issue, 42, Some("foo/bar")))
        );
        assert_eq!(
            parse_forge_ref("https://github.com/foo/bar/pull/7"),
            Some(parsed(ForgeRefKind::Pull, 7, Some("foo/bar")))
        );
    }

    #[test]
    fn github_url_tolerates_trailing_route_noise() {
        assert_eq!(
            parse_forge_ref("https://github.com/foo/bar/pull/7/files"),
            Some(parsed(ForgeRefKind::Pull, 7, Some("foo/bar")))
        );
        assert_eq!(
            parse_forge_ref("https://github.com/foo/bar/issues/42#issuecomment-1"),
            Some(parsed(ForgeRefKind::Issue, 42, Some("foo/bar")))
        );
    }

    #[test]
    fn gitlab_urls_with_nested_groups() {
        assert_eq!(
            parse_forge_ref("https://gitlab.com/group/sub/proj/-/issues/9"),
            Some(parsed(ForgeRefKind::Issue, 9, Some("group/sub/proj")))
        );
        assert_eq!(
            parse_forge_ref("https://gitlab.example.io/team/proj/-/merge_requests/31"),
            Some(parsed(ForgeRefKind::Pull, 31, Some("team/proj")))
        );
    }

    #[test]
    fn hash_numbers_resolve_repoless() {
        assert_eq!(
            parse_forge_ref("#123"),
            Some(parsed(ForgeRefKind::Issue, 123, None))
        );
    }

    #[test]
    fn ordinary_names_and_junk_do_not_parse() {
        for input in [
            "fix-login",
            "fix login 2",
            "v2",
            "0",
            "#0",
            "#12a",
            // Bare digits are legitimate workspace names, never refs.
            "55",
            "2024",
            "123456789012",
            "https://example.com/foo/bar/issues/3",
            "https://github.com/foo/bar/commit/abc123",
            "https://github.com/foo",
            "src/main.rs",
            "",
        ] {
            assert_eq!(parse_forge_ref(input), None, "should not parse: {input:?}");
        }
    }

    #[test]
    fn github_path_with_gitlab_in_name_routes_as_github() {
        // Host check is exact for GitHub, substring for GitLab — a GitHub
        // repo named "gitlab-tools" must not route down the GitLab arm.
        assert_eq!(
            parse_forge_ref("https://github.com/gitlab-tools/x/issues/2"),
            Some(parsed(ForgeRefKind::Issue, 2, Some("gitlab-tools/x")))
        );
    }
}
