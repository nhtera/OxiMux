//! Prompt assembly + agent-output cleaning for AI commit-message generation.
//!
//! Two related concerns live here:
//!
//! - **Prompt build**: assemble the text we hand to the agent CLI from
//!   the staged-diff snapshot + branch name + an optional user-defined
//!   custom suffix. The prompt wording instructs the agent on output
//!   format (imperative subject, <=72 chars, blank line, body) and
//!   delivers the staged patch inside a fenced code block.
//!
//! - **Output clean**: strip the noise agent CLIs commonly emit
//!   around the actual response — leading "Generating…" / "Thinking…"
//!   preambles, a single full-content fenced code block enclosing the
//!   message, leading bullet/numbered-list markers, ANSI control
//!   sequences in stderr-derived error extracts.
//!
//! Everything in this module is synchronous, side-effect-free, and
//! unit-tested directly. The async spawn + tokio plumbing live in
//! [`super::spawn`].

use std::path::PathBuf;

/// Byte budget for the staged patch inside the prompt. Bigger budgets
/// hit agent CLI context limits and degrade latency; smaller ones lose
/// signal for non-trivial commits. 200 KB lands in the sweet spot for
/// most real-world commit sizes.
pub const STAGED_DIFF_BYTE_BUDGET: usize = 200_000;

/// Byte budget for the human-readable staged summary inside the prompt.
/// `git diff --cached --name-status` is compact (one line per file), so
/// this only trips on truly enormous changes.
pub const STAGED_SUMMARY_BYTE_BUDGET: usize = 6_000;

/// Byte budget for the user-supplied custom prompt suffix. Keeps a
/// runaway user prompt from squeezing out the actual diff content.
pub const CUSTOM_PROMPT_BYTE_BUDGET: usize = 4_000;

/// Snapshot of the worktree state the prompt is built from. Mirrors
/// the shape of `oximux_git::staged_context::StagedContext` but is
/// duplicated locally so this crate doesn't take a git-crate
/// dependency — callers convert between the two at the boundary.
#[derive(Debug, Clone)]
pub struct StagedContext {
    pub branch: Option<String>,
    pub summary: String,
    pub patch: String,
    /// Absolute path to the workdir — used by the spawner as `cwd` for
    /// the agent CLI, not consumed by the prompt itself. Threaded
    /// through this struct so callers only pass one bundle.
    pub workdir: PathBuf,
}

/// Build the final prompt sent to the agent. Custom suffix is appended
/// verbatim (truncated) when non-empty so the user can override style
/// (Conventional Commits, gitmoji, internal copy rules, ...).
pub fn build_prompt(context: &StagedContext, custom_suffix: &str) -> String {
    let summary = limit_section(&context.summary, STAGED_SUMMARY_BYTE_BUDGET);
    let patch = truncate_diff(&context.patch, STAGED_DIFF_BYTE_BUDGET);
    let branch_label = context.branch.as_deref().unwrap_or("(detached)");

    let base = format!(
        "You are generating a single git commit message.\n\
         Return only the commit message text. Do not include a preamble, quotes, or code fences.\n\
         \n\
         Rules:\n\
         - First line: imperative mood, <= 72 chars, no trailing period.\n\
         - Optional body: blank line, then short wrapped bullet points or prose explaining WHY.\n\
         - Capture the primary user-visible or developer-visible change.\n\
         - Use only the staged changes below as context.\n\
         - Do not include \"Co-authored-by\" or other git trailers.\n\
         \n\
         Branch: {branch_label}\n\
         \n\
         Staged files:\n\
         {summary}\n\
         \n\
         Staged patch:\n\
         ```diff\n\
         {patch}\n\
         ```\n"
    );

    let trimmed_suffix = custom_suffix.trim();
    if trimmed_suffix.is_empty() {
        return base;
    }
    let suffix_section = limit_section(trimmed_suffix, CUSTOM_PROMPT_BYTE_BUDGET);
    format!("{base}\nAdditional user prompt:\n{suffix_section}\n")
}

/// Hard byte-cap a section with a "[truncated: N bytes omitted]" marker
/// when the cap is exceeded. Used by both the summary and the custom
/// prompt; the diff has its own dedicated [`truncate_diff`] helper
/// with diff-specific wording.
pub fn limit_section(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let safe_cut = safe_utf8_cut(value, max_bytes);
    let omitted = value.len() - safe_cut;
    format!("{}\n\n[truncated: {} bytes omitted]", &value[..safe_cut], omitted)
}

/// Cap a diff at `budget` bytes, appending a marker so the agent knows
/// the input was clipped. The trailing marker uses byte counts (not
/// line counts) so it's unambiguous about what was dropped.
pub fn truncate_diff(diff: &str, budget: usize) -> String {
    if diff.len() <= budget {
        return diff.to_string();
    }
    let safe_cut = safe_utf8_cut(diff, budget);
    let omitted = diff.len() - safe_cut;
    format!(
        "{}\n...(diff truncated, {} bytes omitted)",
        &diff[..safe_cut],
        omitted
    )
}

/// Walk back from `max_bytes` to the nearest char boundary so slicing
/// doesn't panic on multi-byte UTF-8. The clamp budgets are byte-based
/// — the goal is "don't exceed the agent's input budget", not "exact
/// char count".
fn safe_utf8_cut(s: &str, max_bytes: usize) -> usize {
    let mut cut = max_bytes.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// Strips noise around the agent's stdout: surrounding whitespace, a
/// leading "Generating..."/"Thinking..."/ellipsis preamble line, a
/// single full-content fenced code block enclosing the whole body,
/// and a leading bullet or numbered-list marker. Agent CLIs commonly
/// emit one or more of these — stripping consistently means the
/// textarea gets the actual commit message, not the CLI's chrome.
pub fn clean_message(raw: &str) -> String {
    let mut text = raw.replace("\r\n", "\n").trim().to_string();

    // Drop a single preamble line that some CLIs emit before the real
    // response ("Generating...", "Thinking...", "...", "…").
    if let Some(first_nl) = text.find('\n') {
        let first_line = text[..first_nl].trim();
        let is_preamble = first_line
            .to_ascii_lowercase()
            .starts_with("generating")
            || first_line.to_ascii_lowercase().starts_with("thinking")
            || first_line.chars().all(|c| c == '.' || c == '…');
        if is_preamble {
            text = text[first_nl + 1..].trim().to_string();
        }
    }

    // Unwrap a single ```lang\n...\n``` fence enclosing the whole body.
    if let Some(inner) = strip_outer_fence(&text) {
        text = inner.trim().to_string();
    }

    // Strip a leading bullet/numbered-list marker.
    text = strip_leading_list_marker(&text);

    text.trim().to_string()
}

fn strip_outer_fence(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return None;
    }
    // Match opening fence: ```[lang]\n
    let after_open = trimmed.strip_prefix("```")?;
    let nl = after_open.find('\n')?;
    let lang = &after_open[..nl];
    // Only accept simple language tags (alnum + - + _) — otherwise the
    // first ``` is likely a code-sample inside the message body.
    if !lang
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let body = &after_open[nl + 1..];
    // Body must end with ``` on its own line (or trailing whitespace
    // after).
    let body = body.trim_end();
    let inner = body.strip_suffix("```")?;
    Some(inner.trim_end().to_string())
}

fn strip_leading_list_marker(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut prefix_len = 0;
    // Optional leading whitespace
    while let Some(c) = chars.peek() {
        if c.is_whitespace() && *c != '\n' {
            prefix_len += c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    let rest: String = chars.clone().collect();
    // - or * or • or ●  then space
    if let Some(stripped) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("• "))
        .or_else(|| rest.strip_prefix("● "))
    {
        return format!("{}{}", &text[..prefix_len], stripped);
    }
    // 1. or 1) etc.
    let mut digits = 0usize;
    let bytes = rest.as_bytes();
    while digits < bytes.len() && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits > 0
        && digits + 1 < bytes.len()
        && (bytes[digits] == b'.' || bytes[digits] == b')')
        && bytes[digits + 1] == b' '
    {
        let after = &rest[digits + 2..];
        return format!("{}{}", &text[..prefix_len], after);
    }
    text.to_string()
}

/// Subject + optional body, split from a cleaned message string. The
/// subject is clamped to 72 chars and stripped of trailing periods so
/// it survives commit-msg lints. Empty subject falls back to a safe
/// default — defensive against agents that produce empty output on
/// the success path (rare but observed in the wild).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitMessage {
    pub subject: String,
    pub body: String,
}

pub fn split_message(cleaned: &str) -> SplitMessage {
    let normalized = clean_message(cleaned);
    let (subject_line, body_part) = match normalized.find('\n') {
        Some(idx) => (&normalized[..idx], normalized[idx + 1..].trim()),
        None => (normalized.as_str(), ""),
    };
    let trimmed = subject_line
        .trim()
        .trim_end_matches('.')
        .trim_end()
        .to_string();
    let subject = clamp_chars(&trimmed, 72);
    let final_subject = if subject.is_empty() {
        "Update project files".to_string()
    } else {
        subject
    };
    SplitMessage {
        subject: final_subject,
        body: body_part.to_string(),
    }
}

/// Format a split message back into a single text payload for the
/// textarea — subject, blank line, then body when present.
pub fn format_message(split: &SplitMessage) -> String {
    if split.body.is_empty() {
        split.subject.clone()
    } else {
        format!("{}\n\n{}", split.subject, split.body)
    }
}

fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>().trim_end().to_string()
}

/// Best-effort extraction of an agent CLI's real error message from
/// stdout + stderr. Walks backwards for an `ERROR:` / `Error:` line —
/// JSON payloads get unwrapped to `.error.message` or `.message` when
/// present, otherwise the raw payload text is returned. The walk is
/// backwards because the actionable error usually comes after a long
/// run of config / lifecycle / progress lines from the agent CLI.
pub fn extract_agent_error(stdout: &str, stderr: &str) -> Option<String> {
    let combined = strip_ansi(&format!("{stdout}\n{stderr}"));
    let lines: Vec<&str> = combined.split('\n').collect();

    for line in lines.iter().rev() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_ascii_lowercase();
        let payload = if let Some(rest) = trimmed.strip_prefix("ERROR:") {
            Some(rest.trim().to_string())
        } else if let Some(rest) = trimmed.strip_prefix("Error:") {
            Some(rest.trim().to_string())
        } else if lower.starts_with("error during ") {
            // "Error during X: <payload>"
            trimmed
                .find(':')
                .map(|colon| trimmed[colon + 1..].trim().to_string())
        } else {
            None
        };
        if let Some(p) = payload {
            if p.starts_with('{') {
                // Cheap parse: regex the "message" string field. Real
                // JSON parsing would pull in serde_json for one field —
                // not worth it for an error-extraction best-effort.
                if let Some(extracted) = extract_message_field(&p) {
                    return Some(extracted);
                }
            }
            if !p.is_empty() {
                return Some(p);
            }
        }
    }
    None
}

fn extract_message_field(payload: &str) -> Option<String> {
    // Look for "message": "..." or "message":"..." — handles single
    // and double quotes around the value.
    let key_idx = payload.find("\"message\"")?;
    let after = &payload[key_idx + "\"message\"".len()..];
    let after = after.trim_start_matches(|c: char| c.is_whitespace() || c == ':');
    let (quote, after_quote) = if let Some(rest) = after.strip_prefix('"') {
        ('"', rest)
    } else if let Some(rest) = after.strip_prefix('\'') {
        ('\'', rest)
    } else {
        return None;
    };
    let mut end = 0;
    let bytes = after_quote.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'\\' && end + 1 < bytes.len() {
            end += 2;
            continue;
        }
        if bytes[end] as char == quote {
            break;
        }
        end += 1;
    }
    if end == 0 || end > bytes.len() {
        return None;
    }
    let raw = &after_quote[..end];
    let unescaped = raw.replace("\\n", "\n").replace("\\\"", "\"");
    let trimmed = unescaped.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn strip_ansi(s: &str) -> String {
    // ESC[ ... letter — simple CSI sequence. Adequate for stderr noise.
    let esc = 0x1b as char;
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == esc && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(branch: Option<&str>, summary: &str, patch: &str) -> StagedContext {
        StagedContext {
            branch: branch.map(str::to_string),
            summary: summary.to_string(),
            patch: patch.to_string(),
            workdir: PathBuf::from("/tmp/repo"),
        }
    }

    #[test]
    fn build_prompt_includes_branch_summary_and_patch() {
        let c = ctx(Some("main"), "M\ta.txt", "diff --git a/a.txt b/a.txt\n+x");
        let p = build_prompt(&c, "");
        assert!(p.contains("Branch: main"));
        assert!(p.contains("M\ta.txt"));
        assert!(p.contains("+x"));
        assert!(p.contains("```diff"));
    }

    #[test]
    fn build_prompt_marks_detached_head() {
        let c = ctx(None, "A\tnew.txt", "diff body");
        let p = build_prompt(&c, "");
        assert!(p.contains("Branch: (detached)"));
    }

    #[test]
    fn build_prompt_appends_custom_suffix_when_nonempty() {
        let c = ctx(Some("main"), "M\ta", "d");
        let p = build_prompt(&c, "Use Conventional Commits style.");
        assert!(p.contains("Additional user prompt:"));
        assert!(p.contains("Conventional Commits"));
    }

    #[test]
    fn build_prompt_omits_custom_section_when_only_whitespace() {
        let c = ctx(Some("main"), "M\ta", "d");
        let p = build_prompt(&c, "   \n  \t  ");
        assert!(!p.contains("Additional user prompt"));
    }

    #[test]
    fn truncate_diff_appends_marker_when_over_budget() {
        let big = "X".repeat(STAGED_DIFF_BYTE_BUDGET + 100);
        let out = truncate_diff(&big, STAGED_DIFF_BYTE_BUDGET);
        assert!(out.contains("(diff truncated"));
        assert!(out.contains("100 bytes omitted"));
        assert!(out.len() < big.len() + 100);
    }

    #[test]
    fn truncate_diff_passthrough_under_budget() {
        let small = "small diff body";
        assert_eq!(truncate_diff(small, STAGED_DIFF_BYTE_BUDGET), small);
    }

    #[test]
    fn truncate_diff_respects_utf8_boundaries() {
        // Multi-byte char straddling the cut point shouldn't panic.
        let mut s = "ä".repeat(100_000); // each 'ä' is 2 bytes
        s.push_str(&"X".repeat(100));
        let out = truncate_diff(&s, 100_001); // odd cut, mid-codepoint
        assert!(out.contains("(diff truncated"));
    }

    #[test]
    fn limit_section_marker_when_over() {
        let big = "Y".repeat(STAGED_SUMMARY_BYTE_BUDGET + 50);
        let out = limit_section(&big, STAGED_SUMMARY_BYTE_BUDGET);
        assert!(out.contains("[truncated:"));
        assert!(out.contains("50 bytes omitted"));
    }

    #[test]
    fn clean_message_strips_generating_preamble() {
        let raw = "Generating commit message...\nfeat: real message\n\nbody";
        let cleaned = clean_message(raw);
        assert!(cleaned.starts_with("feat: real message"));
        assert!(cleaned.contains("body"));
    }

    #[test]
    fn clean_message_strips_ellipsis_preamble() {
        let raw = "...\nfeat: message";
        assert!(clean_message(raw).starts_with("feat: message"));
    }

    #[test]
    fn clean_message_unwraps_full_content_fence() {
        let raw = "```diff\nfeat: x\n\nbody line\n```";
        let cleaned = clean_message(raw);
        assert_eq!(cleaned, "feat: x\n\nbody line");
    }

    #[test]
    fn clean_message_does_not_unwrap_inline_fence() {
        // Embedded code sample in body — must NOT unwrap.
        let raw = "feat: x\n\nFor example:\n```rust\nlet a = 1;\n```\n\nmore body";
        let cleaned = clean_message(raw);
        // Body should remain present including the fence
        assert!(cleaned.contains("```rust"));
        assert!(cleaned.contains("more body"));
    }

    #[test]
    fn clean_message_strips_leading_bullet() {
        assert_eq!(clean_message("- feat: x"), "feat: x");
        assert_eq!(clean_message("  * feat: x"), "feat: x");
        assert_eq!(clean_message("• feat: x"), "feat: x");
        assert_eq!(clean_message("1. feat: x"), "feat: x");
        assert_eq!(clean_message("2) feat: x"), "feat: x");
    }

    #[test]
    fn split_message_clamps_subject_to_72_chars() {
        let long = "x".repeat(100);
        let split = split_message(&long);
        assert!(split.subject.chars().count() <= 72);
        assert!(split.body.is_empty());
    }

    #[test]
    fn split_message_strips_trailing_periods() {
        let split = split_message("feat: x...\n\nbody");
        assert_eq!(split.subject, "feat: x");
        assert_eq!(split.body, "body");
    }

    #[test]
    fn split_message_empty_subject_falls_back_to_default() {
        let split = split_message("");
        assert_eq!(split.subject, "Update project files");
    }

    #[test]
    fn format_message_blank_line_between_subject_and_body() {
        let split = SplitMessage {
            subject: "feat: x".into(),
            body: "why".into(),
        };
        assert_eq!(format_message(&split), "feat: x\n\nwhy");
    }

    #[test]
    fn format_message_subject_only_no_trailing_blank() {
        let split = SplitMessage {
            subject: "feat: x".into(),
            body: "".into(),
        };
        assert_eq!(format_message(&split), "feat: x");
    }

    #[test]
    fn extract_agent_error_walks_backwards_for_error_line() {
        let stderr = "first line\nERROR: actual failure\nNote: more output";
        assert_eq!(
            extract_agent_error("", stderr).as_deref(),
            Some("actual failure")
        );
    }

    #[test]
    fn extract_agent_error_unwraps_message_field() {
        let stderr = "ERROR: {\"error\":{\"message\":\"Network down\"}}";
        assert_eq!(
            extract_agent_error("", stderr).as_deref(),
            Some("Network down")
        );
    }

    #[test]
    fn extract_agent_error_returns_none_when_no_error_line() {
        assert_eq!(extract_agent_error("just stdout", "just stderr"), None);
    }

    #[test]
    fn extract_agent_error_strips_ansi() {
        let stderr = format!("{}[31mERROR: red error{}[0m", 0x1b as char, 0x1b as char);
        assert_eq!(
            extract_agent_error("", &stderr).as_deref(),
            Some("red error")
        );
    }
}
