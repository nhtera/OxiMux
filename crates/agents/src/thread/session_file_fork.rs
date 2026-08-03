//! Truncate-fork of a Claude CLI session file — the conversation half of
//! rewind/edit-and-resend.
//!
//! The CLI stores each session as `~/.claude/projects/<slug>/<sid>.jsonl`.
//! `--fork-session` itself copies every line verbatim with only `sessionId`
//! rewritten (spike-verified), and a hand-written truncated copy under a fresh
//! uuid resumes cleanly with `--resume <new-uuid>`. This module does exactly
//! that: keep everything BEFORE the target user message, rewrite session ids,
//! write a NEW file next to the original. The original is never modified.
//!
//! Truncation cuts at a REAL human user line (never mid-assistant-turn — a
//! turn's thinking `signature` and shared `message.id` must stay intact), and
//! the cut line's text is verified against the caller's expectation so a
//! drifted ordinal fails safe instead of corrupting.

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("session file for {sid} not found under {root}")]
    SessionFileNotFound { sid: String, root: PathBuf },
    #[error("session has {found} user messages; ordinal {ordinal} out of range")]
    OrdinalOutOfRange { ordinal: usize, found: usize },
    #[error("message text at ordinal {ordinal} doesn't match the transcript — rewind disabled")]
    OrdinalMismatch { ordinal: usize },
    #[error("session file has a torn line before the cut point")]
    TornFile,
    #[error("fork io: {0}")]
    Io(String),
}

impl ForkError {
    fn io(e: impl std::fmt::Display) -> Self {
        Self::Io(e.to_string())
    }
}

/// Default projects root: `~/.claude/projects`.
pub fn default_projects_root() -> Option<PathBuf> {
    // `dirs`, not `$HOME`: Windows sets `USERPROFILE` and leaves `HOME` unset
    // outside git-bash, so reading the variable returned `None` there and the
    // whole projects tree silently went missing.
    Some(dirs::home_dir()?.join(".claude").join("projects"))
}

/// Locate `<root>/*/<sid>.jsonl`. Globbing by sid avoids the lossy
/// cwd→slug encoding entirely (a literal `-` in a path is indistinguishable
/// from a `/` substitution in the slug).
pub fn locate_session_file(root: &Path, sid: &str) -> Option<PathBuf> {
    let name = format!("{sid}.jsonl");
    let entries = std::fs::read_dir(root).ok()?;
    for project_dir in entries.flatten() {
        let candidate = project_dir.path().join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Fork `sid`, keeping only lines BEFORE its `remove_from`-th real human user
/// message (0-based). `expected_text` is the transcript's text for that
/// message — mismatch aborts without writing anything. Returns the new
/// session id; the new file sits next to the original.
pub fn fork_truncated(
    root: &Path,
    sid: &str,
    remove_from: usize,
    expected_text: &str,
) -> Result<String, ForkError> {
    let src = locate_session_file(root, sid).ok_or_else(|| ForkError::SessionFileNotFound {
        sid: sid.to_string(),
        root: root.to_path_buf(),
    })?;

    // The child process should be dead before we get here (cancel_and_wait),
    // but a concurrent writer can still leave a torn last line — retry once.
    let cut = match plan_cut(&src, remove_from, expected_text) {
        Err(ForkError::TornFile) => {
            std::thread::sleep(std::time::Duration::from_millis(150));
            plan_cut(&src, remove_from, expected_text)
        }
        other => other,
    }?;

    let new_sid = uuid::Uuid::new_v4().to_string();
    let dst = src.with_file_name(format!("{new_sid}.jsonl"));
    // create_new: a colliding path is astronomically unlikely — surface it as
    // an error rather than silently overwriting an unrelated session.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dst)
        .map_err(ForkError::io)?;
    let mut out = std::io::BufWriter::new(file);
    for line in cut.kept_lines {
        let rewritten = rewrite_session_id(&line, &new_sid);
        out.write_all(rewritten.as_bytes()).map_err(ForkError::io)?;
        out.write_all(b"\n").map_err(ForkError::io)?;
    }
    let file = out.into_inner().map_err(ForkError::io)?;
    file.sync_all().map_err(ForkError::io)?;
    Ok(new_sid)
}

struct CutPlan {
    kept_lines: Vec<String>,
}

fn plan_cut(src: &Path, remove_from: usize, expected_text: &str) -> Result<CutPlan, ForkError> {
    let raw = std::fs::read_to_string(src).map_err(ForkError::io)?;
    let lines: Vec<&str> = raw.lines().collect();
    let mut kept: Vec<String> = Vec::new();
    let mut user_ordinal = 0usize;

    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // Reaching an unparseable line means the cut point wasn't found
            // yet, so this line would have been KEPT — hard error. (A torn
            // line PAST the cut is never reached: the loop returns at the
            // cut. That's the "torn tail beyond cut is tolerated" property.)
            Err(_) => return Err(ForkError::TornFile),
        };
        if is_real_user_line(&value) {
            if user_ordinal == remove_from {
                let found = extract_user_text(&value);
                if !text_matches(&found, expected_text) {
                    return Err(ForkError::OrdinalMismatch { ordinal: remove_from });
                }
                return Ok(CutPlan { kept_lines: kept });
            }
            user_ordinal += 1;
        }
        kept.push((*line).to_string());
    }

    Err(ForkError::OrdinalOutOfRange {
        ordinal: remove_from,
        found: user_ordinal,
    })
}

/// A REAL human user line: `type=="user"`, not a tool_result carrier, not a
/// compaction summary, not a sidechain (subagent) line.
fn is_real_user_line(v: &serde_json::Value) -> bool {
    if v.get("type").and_then(|t| t.as_str()) != Some("user") {
        return false;
    }
    if v.get("isCompactSummary").and_then(|b| b.as_bool()) == Some(true) {
        return false;
    }
    if v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
        return false;
    }
    match v.pointer("/message/content") {
        Some(serde_json::Value::String(_)) => true,
        Some(serde_json::Value::Array(blocks)) => !blocks.iter().any(|b| {
            b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
        }),
        _ => false,
    }
}

fn extract_user_text(v: &serde_json::Value) -> String {
    match v.pointer("/message/content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .map(|b| b.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whitespace-normalized prefix comparison: the CLI may append attachment
/// context to the stored text, so the transcript's text matching the stored
/// line's HEAD is the invariant.
fn text_matches(stored: &str, expected: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let stored = norm(stored);
    let expected = norm(expected);
    if expected.is_empty() {
        return stored.is_empty();
    }
    stored.starts_with(&expected)
}

fn rewrite_session_id(line: &str, new_sid: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string(); // pre-validated by plan_cut; defensive only
    };
    if let Some(obj) = value.as_object_mut()
        && obj.contains_key("sessionId")
    {
        obj.insert("sessionId".into(), serde_json::Value::String(new_sid.into()));
        return serde_json::to_string(&value).unwrap_or_else(|_| line.to_string());
    }
    line.to_string()
}

#[cfg(test)]
mod tests;
