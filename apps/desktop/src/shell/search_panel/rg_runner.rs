//! `ripgrep --json` subprocess driver.
//!
//! Spawns `rg` with the user's options, stream-parses NDJSON via Tokio
//! line reader, and assembles a `SearchResults` value. Kills the child as
//! soon as `max_results` is reached so big repos don't keep ripgrep running.
//!
//! Path representation is "text-only" for v1 — non-UTF8 paths are dropped
//! with a trace warning rather than supported via the `bytes` variant.

use serde::Deserialize;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use crate::shell::search_panel::search_state::SearchOptions;
use crate::shell::tool_paths::rg_program;

/// Hard upper bound: even if the caller asks for more, this is the ceiling
/// passed to `--max-count` per-file to keep memory bounded on pathological
/// patterns (e.g. `.` matching every line).
pub const PER_FILE_CAP: usize = 100;

/// Default cap on total matches surfaced to the UI. Beyond this, the child is
/// killed and `truncated = true`.
pub const DEFAULT_MAX_RESULTS: usize = 2000;

/// Wall-clock cap. Without this a runaway regex on a huge repo can pin a CPU.
const HARD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: u32,
    /// 1-based byte column of the start of the matched span.
    pub column: u32,
    /// The full matched line (raw, including trailing newline if rg emitted one).
    pub line_content: String,
    /// Byte length of the matched span.
    pub match_length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFileResult {
    pub file_path: PathBuf,
    pub relative_path: PathBuf,
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResults {
    pub files: Vec<SearchFileResult>,
    pub total_matches: usize,
    pub truncated: bool,
}

impl SearchResults {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RgError {
    #[error("ripgrep not found on PATH")]
    NotFound,
    #[error("ripgrep failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("ripgrep exited with error: {0}")]
    Process(String),
}

/// Detect whether ripgrep (bundled or PATH) is callable. Caller should call
/// once at startup and cache the result — repeating it on every keystroke
/// would be wasteful.
pub async fn detect_rg_available() -> bool {
    match Command::new(rg_program()).arg("--version").output().await {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// Run ripgrep against `cwd` with the user's options. Returns assembled
/// results or `RgError` on failure. Empty/whitespace-only queries return
/// `Ok(SearchResults::empty())` so callers don't need to special-case.
pub async fn run_ripgrep(
    cwd: PathBuf,
    opts: SearchOptions,
    max_results: usize,
) -> Result<SearchResults, RgError> {
    if !opts.has_query() {
        return Ok(SearchResults::empty());
    }

    let args = build_args(&opts);
    let mut cmd = Command::new(rg_program());
    cmd.args(&args)
        .arg("--")
        .arg(&opts.query)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RgError::NotFound),
        Err(e) => return Err(RgError::Io(e)),
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RgError::Process("missing stdout".into()))?;
    let reader = BufReader::new(stdout);

    let parse_fut = parse_stream(reader, &cwd, max_results);
    let collected = match timeout(HARD_TIMEOUT, parse_fut).await {
        Ok(r) => r?,
        Err(_) => {
            // Best-effort kill; child::kill is async and `_` here is fine.
            let _ = child.kill().await;
            return Err(RgError::Process("timeout".into()));
        }
    };

    // We may have hit the cap and stopped reading. Kill the child so it
    // doesn't keep scanning the repo in the background.
    let _ = child.start_kill();
    let _ = child.wait().await;

    Ok(collected)
}

fn build_args(opts: &SearchOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    args.push("--json".into());
    args.push("--max-count".into());
    args.push(PER_FILE_CAP.to_string().into());

    if opts.case_sensitive {
        args.push("--case-sensitive".into());
    } else {
        args.push("--smart-case".into());
    }
    if opts.whole_word {
        args.push("--word-regexp".into());
    }
    if !opts.use_regex {
        args.push("--fixed-strings".into());
    }
    for g in split_globs(&opts.include_glob) {
        args.push("--glob".into());
        args.push(g.into());
    }
    for g in split_globs(&opts.exclude_glob) {
        args.push("--glob".into());
        let mut s = String::from("!");
        s.push_str(&g);
        args.push(s.into());
    }
    args
}

fn split_globs(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

async fn parse_stream<R>(
    reader: BufReader<R>,
    cwd: &std::path::Path,
    max_results: usize,
) -> Result<SearchResults, RgError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = reader.lines();
    let mut results = SearchResults::empty();
    let mut current: Option<SearchFileResult> = None;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let evt: RgEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                tracing::trace!(target: "oximux_app::search_panel", error = %e, "skip malformed rg line");
                continue;
            }
        };
        match evt {
            RgEvent::Begin { data } => {
                let Some(abs) = data.path.text else { continue };
                let abs = PathBuf::from(abs);
                let rel = abs.strip_prefix(cwd).unwrap_or(&abs).to_path_buf();
                if let Some(prev) = current.take() {
                    results.files.push(prev);
                }
                current = Some(SearchFileResult {
                    file_path: abs,
                    relative_path: rel,
                    matches: Vec::new(),
                });
            }
            RgEvent::Match { data } => {
                let Some(group) = current.as_mut() else {
                    continue;
                };
                let Some(line_content) = data.lines.text else {
                    continue;
                };
                let line_number = data.line_number.unwrap_or(0);
                for sm in data.submatches {
                    let mtxt = sm.r#match.text.unwrap_or_default();
                    group.matches.push(SearchMatch {
                        line: line_number,
                        column: sm.start as u32 + 1,
                        line_content: line_content.clone(),
                        match_length: sm.end.saturating_sub(sm.start),
                    });
                    results.total_matches += 1;
                    let _ = mtxt; // submatch text not retained — line_content is enough for highlight.
                    if results.total_matches >= max_results {
                        results.truncated = true;
                        if let Some(prev) = current.take() {
                            results.files.push(prev);
                        }
                        return Ok(results);
                    }
                }
            }
            RgEvent::End => {
                if let Some(prev) = current.take()
                    && !prev.matches.is_empty()
                {
                    results.files.push(prev);
                }
            }
            RgEvent::Summary | RgEvent::Other => {}
        }
    }
    if let Some(prev) = current.take()
        && !prev.matches.is_empty()
    {
        results.files.push(prev);
    }
    Ok(results)
}

// ---- ripgrep NDJSON event types ----

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RgEvent {
    Begin {
        data: BeginData,
    },
    Match {
        data: MatchData,
    },
    End,
    Summary,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct BeginData {
    path: TextPath,
}

#[derive(Debug, Deserialize)]
struct MatchData {
    #[allow(dead_code)]
    path: TextPath,
    lines: TextPath,
    line_number: Option<u32>,
    submatches: Vec<Submatch>,
}

#[derive(Debug, Deserialize)]
struct TextPath {
    /// `None` when ripgrep emitted the non-UTF8 `bytes` variant; we drop those.
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Submatch {
    r#match: TextPath,
    start: usize,
    end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_include_smart_case_by_default() {
        let opts = SearchOptions {
            query: "x".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        let s: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert!(s.iter().any(|a| a == "--smart-case"));
        assert!(s.iter().any(|a| a == "--fixed-strings"));
        assert!(!s.iter().any(|a| a == "--case-sensitive"));
    }

    #[test]
    fn args_case_sensitive_when_toggled() {
        let opts = SearchOptions {
            query: "x".into(),
            case_sensitive: true,
            ..Default::default()
        };
        let args = build_args(&opts);
        let s: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert!(s.iter().any(|a| a == "--case-sensitive"));
        assert!(!s.iter().any(|a| a == "--smart-case"));
    }

    #[test]
    fn args_use_regex_drops_fixed_strings() {
        let opts = SearchOptions {
            query: "x".into(),
            use_regex: true,
            ..Default::default()
        };
        let args = build_args(&opts);
        let s: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert!(!s.iter().any(|a| a == "--fixed-strings"));
    }

    #[test]
    fn args_globs_split_and_excludes_get_bang_prefix() {
        let opts = SearchOptions {
            query: "x".into(),
            include_glob: "*.rs, src/**".into(),
            exclude_glob: "*.lock target/**".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        let s: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        let count = |needle: &str| s.iter().filter(|a| a.as_str() == needle).count();
        assert_eq!(count("*.rs"), 1);
        assert_eq!(count("src/**"), 1);
        assert_eq!(count("!*.lock"), 1);
        assert_eq!(count("!target/**"), 1);
    }
}
