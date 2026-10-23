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
    /// An OSC 8 hyperlink whose scheme is not one OxiMux will hand to the
    /// system (see [`classify_osc8_target`]). Shown, never opened.
    Blocked { uri: String, scheme: String },
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

/// Sentence punctuation that trails a link in prose rather than belonging to
/// it — "the journal is at plans/x.md." or "see docs/y.md, then…". Trimmed
/// from BOTH branches: a path that keeps the period stats as missing, which
/// silently suppresses the underline and the click, and a trailing `.` also
/// defeats the `path:line:col` digit peel.
const TRAILING_PUNCT: [char; 6] = ['.', ',', ';', ':', '!', '?'];

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
    let raw: String = row[start..=end].iter().collect();
    // Trailing sentence punctuation isn't part of the link, and the span must
    // shrink with it so the underline covers exactly the clickable text.
    let token = raw.trim_end_matches(TRAILING_PUNCT);
    if token.is_empty() {
        return None;
    }
    let end = start + token.chars().count() - 1;

    if let Some(url) = classify_url(token)
        && url.len() > scheme_len(&url)
    {
        return Some(LinkMatch {
            target: LinkTarget::Url(url),
            col_start: start,
            col_end: end,
        });
    }

    classify_path(token).map(|target| LinkMatch {
        target,
        col_start: start,
        col_end: end,
    })
}

/// Classify a whole link token (as captured by a right-click) into a
/// [`LinkTarget`]: a known URL scheme wins, otherwise a `path[:line[:col]]`
/// is attempted. Returns `None` when the token is neither. Used by the
/// terminal context menu's "Open Link" row to re-derive the target from the
/// string carried in the open action (actions can't ferry enums/`PathBuf`).
pub fn classify_link(token: &str) -> Option<LinkTarget> {
    if let Some(url) = classify_url(token) {
        return Some(LinkTarget::Url(url));
    }
    classify_path(token)
}

/// `s` begins with the scheme prefix `sc`, in any case (`HTTPS://…`).
fn has_scheme_prefix(s: &str, sc: &str) -> bool {
    s.get(..sc.len()).is_some_and(|p| p.eq_ignore_ascii_case(sc))
}

fn scheme_len(s: &str) -> usize {
    SCHEMES
        .iter()
        .find(|sc| has_scheme_prefix(s, sc))
        .map(|sc| sc.len())
        .unwrap_or(0)
}

fn classify_url(token: &str) -> Option<String> {
    SCHEMES
        .iter()
        .any(|sc| has_scheme_prefix(token, sc))
        .then(|| token.to_string())
}

/// Parse `path[:line[:col]]`. Returns `None` when the path part doesn't look
/// like a filesystem path (avoids linkifying arbitrary `word:42` text).
fn classify_path(token: &str) -> Option<LinkTarget> {
    let mut parts: Vec<&str> = token.split(':').collect();
    // Peel up to two trailing all-digit segments (col then line).
    let mut nums: Vec<u32> = Vec::new();
    while parts.len() > 1 && nums.len() < 2 {
        // The `len() > 1` guard makes `last()` infallible, but match it
        // defensively rather than unwrap so a future refactor of the loop
        // bounds can't turn this into a panic.
        let Some(&last) = parts.last() else {
            break;
        };
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

/// What an OSC 8 hyperlink may do when clicked. A program chooses the target
/// freely and the text it shows independently, so only schemes that cannot
/// run anything go to the system handler: a `file:` URL handed to it would
/// let LaunchServices execute the file (a `.command`, an app bundle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc8Action {
    /// `http`, `https` or `mailto`: the system handler.
    Open(String),
    /// `file://`: OxiMux's own editor, never the system handler.
    OpenInEditor(PathBuf),
    /// Anything else — `ssh`, `git`, custom app schemes, `javascript:`, a bare
    /// path — carrying the scheme for the "Blocked link" message.
    Blocked(String),
}

/// Classify an OSC 8 target. Shared by Cmd-click and the context menu.
pub fn classify_osc8_target(uri: &str) -> Osc8Action {
    let Some(scheme) = uri_scheme(uri) else {
        return Osc8Action::Blocked("none".into());
    };
    match scheme.as_str() {
        "http" | "https" | "mailto" => Osc8Action::Open(uri.to_string()),
        "file" => match file_url_path(uri) {
            Some(path) => Osc8Action::OpenInEditor(path),
            None => Osc8Action::Blocked(scheme),
        },
        _ => Osc8Action::Blocked(scheme),
    }
}

/// The OSC 8 target as a [`LinkTarget`], so a hyperlink shares every path
/// the plain-text links take (underline, context menu, open).
pub fn osc8_link_target(uri: &str) -> LinkTarget {
    match classify_osc8_target(uri) {
        Osc8Action::Open(url) => LinkTarget::Url(url),
        Osc8Action::OpenInEditor(path) => LinkTarget::Path { path, line: None, col: None },
        Osc8Action::Blocked(scheme) => LinkTarget::Blocked { uri: uri.to_string(), scheme },
    }
}

/// The lowercase scheme of `uri` (`[A-Za-z][A-Za-z0-9+.-]*` before `:`).
fn uri_scheme(uri: &str) -> Option<String> {
    let (scheme, _) = uri.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// Whether `uri` is a `file:` URL, in any case — decided by the scheme alone,
/// so one that fails to parse is still known to be a file URL.
pub fn is_file_url(uri: &str) -> bool {
    uri_scheme(uri).as_deref() == Some("file")
}

/// The local path of a `file:` URL, any case. `None` for anything else, and
/// for a file URL that does not parse (see [`is_file_url`]).
pub fn file_url_path(uri: &str) -> Option<PathBuf> {
    if !is_file_url(uri) {
        return None;
    }
    // `parse_file_uri` wants the canonical lowercase spelling.
    oximux_pty::osc7::parse_file_uri(&format!("file{}", &uri[4..]))
}

/// The host of a URL-looking string (`scheme://host…`, or a bare `host.tld…`),
/// lowercased, for comparing a link's shown text with its real target.
fn url_host(text: &str) -> Option<String> {
    let text = text.trim();
    let rest = match text.split_once("://") {
        Some((_, rest)) => rest,
        None if text.contains('.') && !text.contains(char::is_whitespace) => text,
        None => return None,
    };
    let bare = !text.contains("://");
    let host = rest
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    // Bare text reads as a host only when it ends in a word-like label, so a
    // version (`v1.2.3`) or a `file.c:12` reference is not mistaken for one.
    let tld_like = host
        .rsplit('.')
        .next()
        .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
    let is_host = if bare { host.contains('.') && tld_like } else { host.contains('.') || host == "localhost" };
    is_host.then_some(host)
}

/// Whether an OSC 8 link's visible text claims a different destination than
/// its target: the text reads as a URL or host, and that host is not the
/// target's. A link titled "click here" claims nothing and is not flagged.
pub fn osc8_text_misleads(shown: &str, target: &str) -> bool {
    // A file link (`ls --hyperlink`, compiler output) shows a file name, which
    // can look like a host; it misleads only when it spells out another URL.
    if is_file_url(target) {
        return shown.contains("://") && !is_file_url(shown.trim());
    }
    match (url_host(shown), url_host(target)) {
        (Some(shown), Some(real)) => shown != real,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc8_opens_only_web_and_mail_links_through_the_system() {
        for ok in ["https://example.com/x", "HTTPS://Example.com", "http://a.b", "mailto:me@x.org"] {
            assert!(matches!(classify_osc8_target(ok), Osc8Action::Open(_)), "{ok}");
        }
        for (bad, scheme) in [
            ("ssh://host", "ssh"),
            ("git://h/r", "git"),
            ("javascript:alert(1)", "javascript"),
            ("vscode://file/x", "vscode"),
            ("x-custom-app:do", "x-custom-app"),
            ("ftp://h/f", "ftp"),
            ("/usr/bin/true", "none"),
            ("relative/path", "none"),
        ] {
            assert_eq!(classify_osc8_target(bad), Osc8Action::Blocked(scheme.into()), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_osc8_file_link_goes_to_the_editor_never_the_system() {
        assert_eq!(
            classify_osc8_target("file:///tmp/evil.command"),
            Osc8Action::OpenInEditor(PathBuf::from("/tmp/evil.command"))
        );
        assert_eq!(
            classify_osc8_target("FILE://host/tmp/x.app"),
            Osc8Action::OpenInEditor(PathBuf::from("/tmp/x.app"))
        );
        assert!(matches!(osc8_link_target("file:///tmp/a"), LinkTarget::Path { .. }));
        assert!(matches!(osc8_link_target("ssh://h"), LinkTarget::Blocked { .. }));
        // A plain-text `file://` URL is routed to the editor too.
        assert_eq!(file_url_path("file:///tmp/a.command"), Some(PathBuf::from("/tmp/a.command")));
        assert_eq!(file_url_path("https://x.org"), None);
    }

    #[test]
    fn link_text_that_names_another_host_is_flagged() {
        assert!(osc8_text_misleads("https://bank.com/login", "https://evil.example/bank"));
        assert!(osc8_text_misleads("bank.com", "https://evil.example"));
        assert!(!osc8_text_misleads("https://www.bank.com/a", "https://bank.com/b"));
        assert!(!osc8_text_misleads("click here", "https://anything.example"));
        assert!(!osc8_text_misleads("src/main.rs", "file:///repo/src/main.rs"));
        assert!(!osc8_text_misleads("main.rs", "file:///repo/main.rs"));
        assert!(!osc8_text_misleads("foo.c:12:3", "file:///repo/foo.c"));
        assert!(!osc8_text_misleads("v1.2.3", "https://github.com/o/r/releases/v1.2.3"));
        assert!(osc8_text_misleads("https://bank.com", "file:///tmp/x.command"));
        assert!(osc8_text_misleads("https://docs.rs", "ssh://attacker"));
    }

    #[test]
    fn a_malformed_file_url_is_still_a_file_url() {
        assert!(is_file_url("file:///x%ZZ/y.command"));
        assert!(is_file_url("FILE:x"));
        assert!(file_url_path("file:///x%ZZ/y.command").is_none());
        assert!(!is_file_url("https://x.org"));
        // Plain-text detection is case-insensitive on the scheme.
        assert_eq!(classify_link("HTTPS://Example.com"), Some(LinkTarget::Url("HTTPS://Example.com".into())));
    }

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
    fn trims_trailing_punctuation_from_paths() {
        // The shape agents actually print: a doc path ending a sentence.
        // Keeping the period made the existence stat miss, which silently
        // suppressed both the hover underline and the Cmd-click.
        let r = row("A journal entry is at plans/journals/2026-09-11-x.md.");
        let m = detect_at(&r, 25).unwrap();
        assert_eq!(
            m.target,
            LinkTarget::Path {
                path: PathBuf::from("plans/journals/2026-09-11-x.md"),
                line: None,
                col: None,
            }
        );
        // The span stops before the period so the underline matches the link.
        assert_eq!(
            r[m.col_start..=m.col_end].iter().collect::<String>(),
            "plans/journals/2026-09-11-x.md"
        );
        // A comma mid-sentence is a boundary too.
        assert_eq!(
            target_at("see docs/setup.md, then go", 6),
            Some(LinkTarget::Path {
                path: PathBuf::from("docs/setup.md"),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn trailing_period_does_not_eat_line_and_col() {
        // Without the trim the trailing `.` blocks the digit peel, losing the
        // position AND leaving a path that can never exist on disk.
        assert_eq!(
            target_at("open src/foo.rs:42:7.", 5),
            Some(LinkTarget::Path {
                path: PathBuf::from("src/foo.rs"),
                line: Some(42),
                col: Some(7),
            })
        );
    }

    #[test]
    fn punctuation_only_token_is_not_a_link() {
        // Trimming must not manufacture an empty token (or a bare scheme).
        assert_eq!(target_at("...", 1), None);
        assert_eq!(target_at("mailto:", 0), None);
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
    fn classify_link_handles_urls_and_paths() {
        // The context menu's "Open Link" re-classifies the captured token.
        assert_eq!(
            classify_link("https://example.com/x"),
            Some(LinkTarget::Url("https://example.com/x".into()))
        );
        assert_eq!(
            classify_link("src/foo.rs:42:7"),
            Some(LinkTarget::Path {
                path: PathBuf::from("src/foo.rs"),
                line: Some(42),
                col: Some(7),
            })
        );
        // A bare word is neither.
        assert_eq!(classify_link("hello"), None);
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
