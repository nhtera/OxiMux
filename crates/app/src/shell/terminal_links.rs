//! Link detection for the terminal grid: bare URLs and `path:line:col`
//! references in plain output (compiler errors, grep, stack traces). OSC 8
//! explicit hyperlinks are resolved separately from snapshot link spans; this
//! module is the plain-text fallback and is a pure function over a row's
//! characters so it is unit-testable without the grid.
//!
//! Coordinates are character columns within a single visible row. Line/col
//! parsed from `path:line:col` are kept 1-based (as the text shows them); the
//! open path converts to the editor's 0-based `Position`.

use std::path::PathBuf;

/// Where a detected link points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// A URL opened by the system handler.
    Url(String),
    /// A filesystem path with optional 1-based line/col, opened in the editor.
    Path {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// A detected link plus the inclusive column span it occupies on the row, so
/// the renderer can underline exactly the link text on hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMatch {
    pub target: LinkTarget,
    pub col_start: usize,
    pub col_end: usize,
}

/// URL schemes we linkify. Kept short and common; `mailto:` has no `//`.
const SCHEMES: &[&str] = &[
    "https://",
    "http://",
    "ftp://",
    "file://",
    "ssh://",
    "git://",
    "mailto:",
];

/// Characters that end a link token. Brackets/quotes are boundaries so a path
/// inside `(...)` or `"..."` (common in tracebacks) is captured cleanly.
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}')
}

/// Detect a link at character column `col` within `row`. Returns the target
/// plus its inclusive `[col_start, col_end]` span, or `None`.
pub fn detect_at(row: &[char], col: usize) -> Option<LinkMatch> {
    if col >= row.len() || is_boundary(row[col]) {
        return None;
    }
    // Expand to the whitespace/bracket-delimited token around `col`.
    let mut start = col;
    while start > 0 && !is_boundary(row[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < row.len() && !is_boundary(row[end + 1]) {
        end += 1;
    }
    let token: String = row[start..=end].iter().collect();

    if let Some(url) = classify_url(&token) {
        // Trailing sentence punctuation isn't part of the URL.
        let trimmed_len = url.trim_end_matches(['.', ',', ';', ':', '!', '?']).len();
        let trimmed = &url[..trimmed_len];
        if trimmed.len() > scheme_len(trimmed) {
            return Some(LinkMatch {
                target: LinkTarget::Url(trimmed.to_string()),
                col_start: start,
                col_end: start + trimmed.chars().count() - 1,
            });
        }
    }

    classify_path(&token).map(|target| LinkMatch {
        target,
        col_start: start,
        col_end: end,
    })
}

fn scheme_len(s: &str) -> usize {
    SCHEMES
        .iter()
        .find(|sc| s.starts_with(**sc))
        .map(|sc| sc.len())
        .unwrap_or(0)
}

fn classify_url(token: &str) -> Option<String> {
    SCHEMES
        .iter()
        .any(|sc| token.starts_with(sc))
        .then(|| token.to_string())
}

/// Parse `path[:line[:col]]`. Returns `None` when the path part doesn't look
/// like a filesystem path (avoids linkifying arbitrary `word:42` text).
fn classify_path(token: &str) -> Option<LinkTarget> {
    let mut parts: Vec<&str> = token.split(':').collect();
    // Peel up to two trailing all-digit segments (col then line).
    let mut nums: Vec<u32> = Vec::new();
    while parts.len() > 1 && nums.len() < 2 {
        let last = *parts.last().unwrap();
        if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
            nums.push(last.parse().ok()?);
            parts.pop();
        } else {
            break;
        }
    }
    let path = parts.join(":");
    if !looks_like_path(&path) {
        return None;
    }
    let (line, col) = match nums.as_slice() {
        [c, l] => (Some(*l), Some(*c)), // popped col first, then line
        [l] => (Some(*l), None),
        _ => (None, None),
    };
    Some(LinkTarget::Path {
        path: PathBuf::from(path),
        line,
        col,
    })
}

/// Heuristic: a path either contains a `/` or has a `name.ext` dot (a `.`
/// followed by a letter), which excludes numeric tokens like `3.14`.
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.contains('/') {
        return true;
    }
    if let Some(dot) = s.rfind('.') {
        return s[dot + 1..].starts_with(|c: char| c.is_ascii_alphabetic());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn target_at(s: &str, col: usize) -> Option<LinkTarget> {
        detect_at(&row(s), col).map(|m| m.target)
    }

    #[test]
    fn detects_https_url_and_span() {
        let r = row("see https://example.com/x now");
        let m = detect_at(&r, 6).unwrap();
        assert_eq!(m.target, LinkTarget::Url("https://example.com/x".into()));
        // span covers exactly the URL.
        assert_eq!(&r[m.col_start..=m.col_end].iter().collect::<String>(), "https://example.com/x");
    }

    #[test]
    fn trims_trailing_punctuation_from_url() {
        // Trailing period (end of sentence) is not part of the URL.
        assert_eq!(
            target_at("visit https://example.com.", 7),
            Some(LinkTarget::Url("https://example.com".into()))
        );
    }

    #[test]
    fn detects_path_with_line_and_col() {
        assert_eq!(
            target_at("error at src/foo.rs:42:7 here", 12),
            Some(LinkTarget::Path {
                path: PathBuf::from("src/foo.rs"),
                line: Some(42),
                col: Some(7),
            })
        );
    }

    #[test]
    fn detects_path_with_line_only() {
        assert_eq!(
            target_at("src/foo.rs:42", 0),
            Some(LinkTarget::Path {
                path: PathBuf::from("src/foo.rs"),
                line: Some(42),
                col: None,
            })
        );
    }

    #[test]
    fn detects_bare_path_without_position() {
        assert_eq!(
            target_at("./Cargo.toml", 2),
            Some(LinkTarget::Path {
                path: PathBuf::from("./Cargo.toml"),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn plain_word_and_number_are_not_links() {
        assert_eq!(target_at("hello world", 2), None);
        // `word:42` without a path-like stem is not a link.
        assert_eq!(target_at("count:42", 0), None);
        // A bare decimal is not a path.
        assert_eq!(target_at("pi is 3.14", 7), None);
    }

    #[test]
    fn boundary_and_out_of_range_yield_none() {
        assert_eq!(target_at("a b", 1), None); // the space
        assert_eq!(target_at("abc", 9), None); // past end
    }
}
