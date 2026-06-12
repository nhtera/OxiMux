//! Link detection for the terminal grid: bare URLs and `path:line:col`
//! references in plain output (compiler errors, grep, stack traces). OSC 8
//! explicit hyperlinks are resolved separately from snapshot link spans; this
//! module is the plain-text fallback and is a pure function over a row's
//! characters so it is unit-testable without the grid.
//!
//! Coordinates are character columns within a single visible row. Line/col
//! parsed from `path:line:col` are kept 1-based (as the text shows them); the
//! open path converts to the editor's 0-based `Position`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Cached answer for "does this resolved path exist on disk?". `Pending`
/// while the background stat is in flight — no underline yet, so hover and
/// paint stay free of filesystem IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existence {
    Pending,
    Exists,
    Missing,
}

/// Entries expire after this long. For resolved answers that means a file
/// created (or deleted) after the first hover is re-checked instead of
/// pinned to a stale answer; for `Pending` it means a check whose task was
/// torn down before landing (entity drop races, shutdown) self-heals on
/// the next hover instead of suppressing the underline forever. A stat
/// slower than the TTL can double-spawn; the results are idempotent.
pub const EXISTENCE_TTL: Duration = Duration::from_secs(5);

/// Wholesale-clear threshold. Entries are one cheap stat away from being
/// rebuilt, so a dumb clear beats LRU bookkeeping at this size.
const EXISTENCE_CAP: usize = 512;

/// Existence cache keyed by RESOLVED absolute path — the session cwd is
/// baked in at resolution time, so one key covers the (cwd, relative-path)
/// pair the spec calls for.
pub struct ExistenceCache {
    entries: HashMap<PathBuf, (Existence, Instant)>,
}

impl ExistenceCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Current answer, or `None` when the path is unknown or its entry
    /// expired — the caller should record `Pending` and start a check.
    pub fn lookup(&self, path: &Path, now: Instant) -> Option<Existence> {
        self.entries.get(path).and_then(|(state, at)| {
            (now.saturating_duration_since(*at) < EXISTENCE_TTL).then_some(*state)
        })
    }

    pub fn record(&mut self, path: PathBuf, state: Existence, now: Instant) {
        if self.entries.len() >= EXISTENCE_CAP && !self.entries.contains_key(&path) {
            self.entries.clear();
        }
        self.entries.insert(path, (state, now));
    }
}

impl Default for ExistenceCache {
    fn default() -> Self {
        Self::new()
    }
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

    #[test]
    fn existence_cache_expires_every_entry_kind() {
        let t0 = Instant::now();
        let mut cache = ExistenceCache::new();
        let p = PathBuf::from("/tmp/x.rs");

        assert_eq!(cache.lookup(&p, t0), None, "unknown path");

        cache.record(p.clone(), Existence::Pending, t0);
        assert_eq!(cache.lookup(&p, t0), Some(Existence::Pending));
        // An abandoned Pending (check task torn down) self-heals: the
        // entry expires and the next hover re-arms a fresh check.
        assert_eq!(cache.lookup(&p, t0 + EXISTENCE_TTL), None);

        cache.record(p.clone(), Existence::Missing, t0);
        assert_eq!(cache.lookup(&p, t0), Some(Existence::Missing));
        // A resolved answer expires after the TTL → caller re-checks.
        assert_eq!(cache.lookup(&p, t0 + EXISTENCE_TTL), None);
    }
}
