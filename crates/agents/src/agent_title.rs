//! Ambient agent detection from a terminal's OSC window title.
//!
//! Interactive coding-agent CLIs advertise their live state by rewriting the
//! terminal title as they run: an awaiting marker when idle/waiting on the
//! user, an animated spinner glyph while working, and a distinct glyph or
//! phrase when blocked on a permission prompt. A plain shell title (the cwd,
//! `user@host:path`) carries none of these markers, so a title that does is a
//! strong signal that the foreground program in that PTY is an agent — even
//! one the user launched by hand instead of through the spawn picker.
//!
//! `classify_agent_title` turns such a title into an [`AgentStatus`] so the
//! sidebar can surface a hand-launched agent the same way it surfaces a
//! spawned one. A `None` result means "not recognizably an agent activity"
//! and the caller should fall back to its tracked-session status.

use oximux_core::AgentStatus;

/// Awaiting/idle marker some agents prefix onto the title while waiting on
/// the user (U+2733, eight-spoked asterisk).
const IDLE_MARKER: char = '\u{2733}';

/// Stop-hand glyph (U+270B) some agents show when blocked on an approval
/// prompt. Agent-specific and rare in shell titles, so it stands alone.
const PERMISSION_GLYPH: char = '\u{270B}';

/// Known interactive agent CLIs, as (name token, display label).
///
/// Single source of truth for naming an agent: the title heuristic below
/// matches these tokens, and [`crate::agent_process`] resolves the labels for
/// agents it recognises by process instead. Two detectors that disagreed about
/// what to call the same CLI would render as two differently-styled rows for
/// one agent.
///
/// Tokens are matched whole (see [`token_present`]) so `opencode-helper` does
/// not match `opencode`. Order decides a title naming two agents; the most
/// specific names come first and the most common CLI last.
pub(crate) const AGENT_LABELS: &[(&str, &str)] = &[
    ("gemini", "Gemini CLI"),
    ("codex", "Codex"),
    ("copilot", "GitHub Copilot"),
    ("grok", "Grok"),
    ("opencode", "OpenCode"),
    ("aider", "Aider"),
    ("cursor", "Cursor"),
    ("droid", "Droid"),
    ("omp", "omp"),
    ("pi", "Pi"),
    ("amp", "Amp"),
    ("claude", "Claude Code"),
];

/// True when the title contains a glyph from the Braille Patterns block
/// (U+2800..=U+28FF) — the range every common spinner animation draws from.
/// Its presence means the agent is actively working.
fn has_spinner_glyph(title: &str) -> bool {
    title.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
}

/// A byte that, adjacent to an agent-name token, blocks a whole-token match:
/// word characters plus the path/command separators `.`, `/`, `\`, `-`. This
/// rejects `opencode-helper`, `my.codex`, and `bin/aider` from matching the
/// bare agent name (mirrors a `[\w./\\-]` boundary guard).
fn is_token_flank_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'\\' | b'-')
}

/// Whole-token (boundary-delimited) substring test: `token` matches only when
/// it is not flanked by a token-flank byte, so it never fires mid-word or
/// inside a hyphenated/pathed name. Tokens are ASCII; the haystack may hold
/// multibyte glyphs, but a continuation byte (>=0x80) is not a flank byte, so
/// it correctly reads as a boundary.
fn token_present(haystack: &str, token: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(token) {
        let start = from + rel;
        let end = start + token.len();
        let before_ok = start == 0 || !is_token_flank_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_token_flank_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn contains_agent_token(lower: &str) -> bool {
    AGENT_LABELS
        .iter()
        .any(|(tok, _)| token_present(lower, tok))
}

/// Classify a terminal title into a live agent status, or `None` when the
/// title shows no recognizable agent activity.
///
/// Precedence: a permission prompt outranks a working spinner, which outranks
/// an idle marker — the most attention-worthy state a single title can carry.
/// The keyword fallback fires only when a known agent token is present, so
/// plain-shell titles never register a concrete state.
pub fn classify_agent_title(title: &str) -> Option<AgentStatus> {
    let lower = title.to_ascii_lowercase();

    // Permission / blocked — highest priority.
    if title.contains(PERMISSION_GLYPH) {
        return Some(AgentStatus::NeedsApproval("permission".into()));
    }
    if contains_agent_token(&lower)
        && (lower.contains("action required")
            || lower.contains("permission")
            || lower.contains("waiting for your")
            || lower.contains("needs approval"))
    {
        return Some(AgentStatus::NeedsApproval("permission".into()));
    }

    // Working — an active spinner glyph anywhere in the title.
    if has_spinner_glyph(title) {
        return Some(AgentStatus::Running);
    }

    // Idle — an explicit awaiting marker.
    if title.contains(IDLE_MARKER) {
        return Some(AgentStatus::Idle);
    }

    // Keyword fallback for named agents that spell their state in words.
    if contains_agent_token(&lower) {
        if lower.contains("working") || lower.contains("thinking") || lower.contains("running") {
            return Some(AgentStatus::Running);
        }
        if lower.contains("idle") || lower.contains("ready") || lower.contains("done") {
            return Some(AgentStatus::Idle);
        }
    }

    None
}

/// Resolve the display NAME of the agent a terminal title belongs to, or
/// `None` when the title is not recognizably an agent. Mirrors the precedence
/// of [`classify_agent_title`] but returns a human label for the sidebar
/// (e.g. a hand-launched agent shows up by name, not just by status).
///
/// Token-named agents win over the glyph heuristic (a spinner glyph could
/// belong to several CLIs); a bare spinner / idle marker with no other token
/// falls through to the most common interactive agent.
pub fn agent_label_from_title(title: &str) -> Option<&'static str> {
    let lower = title.to_ascii_lowercase();

    // Gemini's own marks carry no name token, so they are checked ahead of
    // the shared table.
    if title.contains('\u{2726}') || title.contains('\u{23F2}') {
        return Some("Gemini CLI");
    }
    if let Some((_, label)) = AGENT_LABELS
        .iter()
        .find(|(tok, _)| token_present(&lower, tok))
    {
        return Some(label);
    }

    // A bare spinner glyph or idle marker with no other token is most
    // commonly the primary interactive agent.
    if has_spinner_glyph(title) || title.contains(IDLE_MARKER) {
        return Some("Claude Code");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_named_agents_win_over_glyph() {
        assert_eq!(agent_label_from_title("codex — \u{2807} working"), Some("Codex"));
        assert_eq!(agent_label_from_title("aider: ready"), Some("Aider"));
        assert_eq!(agent_label_from_title("\u{2726} gemini"), Some("Gemini CLI"));
    }

    #[test]
    fn label_bare_glyph_is_claude() {
        assert_eq!(agent_label_from_title("\u{2807} editing"), Some("Claude Code"));
        assert_eq!(agent_label_from_title("\u{2733} oximux"), Some("Claude Code"));
        assert_eq!(agent_label_from_title("claude"), Some("Claude Code"));
    }

    #[test]
    fn label_plain_shell_is_none() {
        assert_eq!(agent_label_from_title("nguyen@host: ~/Code"), None);
        assert_eq!(agent_label_from_title("~/opencode-helper"), None);
    }

    #[test]
    fn plain_shell_titles_are_not_agents() {
        assert_eq!(classify_agent_title("nguyen@host: ~/Code/oximux"), None);
        assert_eq!(classify_agent_title("zsh"), None);
        assert_eq!(classify_agent_title("~/Code/projects/graphify-rs"), None);
        assert_eq!(classify_agent_title(""), None);
    }

    #[test]
    fn spinner_glyph_reads_as_working() {
        // Braille spinner frame mid-title → Running.
        assert_eq!(
            classify_agent_title("\u{2807} Editing files"),
            Some(AgentStatus::Running)
        );
    }

    #[test]
    fn idle_marker_reads_as_idle() {
        assert_eq!(
            classify_agent_title("\u{2733} oximux"),
            Some(AgentStatus::Idle)
        );
    }

    #[test]
    fn permission_glyph_reads_as_needs_approval() {
        assert!(matches!(
            classify_agent_title("\u{270B} Allow edit?"),
            Some(AgentStatus::NeedsApproval(_))
        ));
    }

    #[test]
    fn permission_outranks_spinner() {
        // A title carrying both must report the more attention-worthy state.
        assert!(matches!(
            classify_agent_title("\u{270B} \u{2807} blocked"),
            Some(AgentStatus::NeedsApproval(_))
        ));
    }

    #[test]
    fn keyword_fallback_requires_agent_token() {
        // "working" alone (no agent token, no glyph) is just a shell title.
        assert_eq!(classify_agent_title("working on taxes"), None);
        // With a recognized agent token it classifies.
        assert_eq!(
            classify_agent_title("codex — working"),
            Some(AgentStatus::Running)
        );
        assert_eq!(
            classify_agent_title("aider: ready"),
            Some(AgentStatus::Idle)
        );
    }

    #[test]
    fn permission_keyword_needs_agent_token() {
        // Avoid firing on stray "permission denied" output in a shell title.
        assert_eq!(classify_agent_title("permission denied"), None);
        assert!(matches!(
            classify_agent_title("codex action required"),
            Some(AgentStatus::NeedsApproval(_))
        ));
    }

    #[test]
    fn token_match_is_boundary_aware() {
        // A directory named after an agent must not register a state.
        assert_eq!(classify_agent_title("~/opencode-helper/src"), None);
        // Whole-token presence with a state keyword does register.
        assert_eq!(
            classify_agent_title("opencode thinking"),
            Some(AgentStatus::Running)
        );
    }
}
