//! Context providers for the chat composer's `@` menu: the app-layer glue that
//! turns "the user picked `@diff`/`@terminal`/`@clipboard`" into a captured
//! [`ContextChip`].
//!
//! Split of concerns:
//! - The pure, transport-agnostic chip model + serializer lives in the agents
//!   crate ([`oximux_agents::thread::context_chip`]).
//! - The *sources* offered in the `@` menu ([`ContextSource`]) and the *capture
//!   requests* they emit ([`ContextRequest`]) live here, along with the pure
//!   cap/combine helpers that shape captured bytes into a chip.
//! - The IO itself (reading the clipboard, shelling out `git diff`, pulling a
//!   terminal's scrollback) is done by the owning [`super::AgentChatView`], which
//!   holds the `cx`, the chat cwd, and the sibling-terminal handles. It calls the
//!   helpers here to build the chip.
//!
//! Keeping the caps + combiners pure makes the truncation semantics unit-testable
//! without a running terminal, git repo, or clipboard.

use oximux_agents::thread::{ContextChip, ContextKind};
use oximux_pty::TerminalSessionId;

/// Last N lines of a terminal's scrollback attached by `@terminal` (a live
/// selection overrides this — see `TerminalView::capture_agent_context`).
pub const TERMINAL_MAX_LINES: usize = 200;
/// Cap on an attached working-tree `@diff`.
pub const DIFF_MAX_LINES: usize = 400;
/// Cap on attached `@clipboard` text — a paste can be arbitrarily large.
pub const CLIPBOARD_MAX_BYTES: usize = 32 * 1024;
/// Cap on a `@browser` element capture. The picker already clamps `outerHTML` to
/// 4 KiB in-page, but a deeply nested element's ancestor + DOM paths sit on top
/// of that, so the assembled block gets its own ceiling.
pub const BROWSER_MAX_BYTES: usize = 16 * 1024;

/// One entry offered in the composer's `@` menu "Context" section. Cheap display
/// data plus the [`ContextRequest`] the composer emits back to the view when the
/// user picks it. Rebuilt by the view whenever the `@` menu opens so the terminal
/// list stays fresh.
#[derive(Debug, Clone)]
pub struct ContextSource {
    /// Shown in the menu row, e.g. `@diff`, `@clipboard`, `@terminal build`.
    pub label: String,
    /// Lowercased keyword the composer ranks the typed `@query` against
    /// (`"diff"`, `"clipboard"`, `"terminal <title>"`).
    pub match_key: String,
    /// What the view should capture when this source is picked.
    pub request: ContextRequest,
}

impl ContextSource {
    pub fn diff() -> Self {
        Self {
            label: "@diff".into(),
            match_key: "diff".into(),
            request: ContextRequest::Diff,
        }
    }

    pub fn clipboard() -> Self {
        Self {
            label: "@clipboard".into(),
            match_key: "clipboard".into(),
            request: ContextRequest::Clipboard,
        }
    }

    pub fn terminal(id: TerminalSessionId, title: &str) -> Self {
        let title = title.trim();
        let label = if title.is_empty() {
            "@terminal".to_string()
        } else {
            format!("@terminal {title}")
        };
        Self {
            label,
            match_key: format!("terminal {}", title.to_lowercase()),
            request: ContextRequest::Terminal { id, title: title.to_string() },
        }
    }
}

/// What [`super::AgentChatView`] should capture when a context source is picked.
/// Plain, `Clone`-able data (a terminal is named by its stable
/// [`TerminalSessionId`], re-resolved to a live view at capture time) so the
/// composer can hold and echo it back without borrowing any entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRequest {
    /// The chat cwd's working-tree diff (staged + unstaged).
    Diff,
    /// The current text clipboard.
    Clipboard,
    /// A sibling terminal tab's scrollback (or its live selection).
    Terminal { id: TerminalSessionId, title: String },
}

/// Trim `text` to at most `max_lines` lines, keeping the FIRST `max_lines`.
/// Returns `(text, truncated)`. Used for the `@diff` cap (a diff reads top-down,
/// so the head is the useful part).
pub fn cap_head_lines(text: &str, max_lines: usize) -> (String, bool) {
    let total = text.lines().count();
    if total <= max_lines {
        return (text.to_string(), false);
    }
    let kept: Vec<&str> = text.lines().take(max_lines).collect();
    (kept.join("\n"), true)
}

/// Trim `text` to at most `max_bytes`, keeping the TAIL and never splitting a
/// char. Returns `(text, truncated)`. Used for `@clipboard` (the recent tail of a
/// long paste is usually what the user means to reference).
pub fn cap_tail_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    // Walk forward from `len - max_bytes` to the next char boundary so the kept
    // suffix is valid UTF-8.
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_string(), true)
}

/// Build the `@clipboard` chip from the clipboard's current text, or `None` when
/// the clipboard holds no text. Caps to [`CLIPBOARD_MAX_BYTES`].
pub fn clipboard_chip(text: Option<String>) -> Option<ContextChip> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    let (content, truncated) = cap_tail_bytes(&text, CLIPBOARD_MAX_BYTES);
    Some(ContextChip::new(ContextKind::Clipboard, None, content, truncated))
}

/// Build the `@diff` chip by concatenating the unstaged then staged patches and
/// capping to [`DIFF_MAX_LINES`]. Always returns a chip: an empty working tree
/// yields a short placeholder so the user gets feedback that there was nothing to
/// attach, rather than a silent no-op.
pub fn diff_chip(unstaged: &str, staged: &str) -> ContextChip {
    let mut combined = String::new();
    let unstaged = unstaged.trim_end();
    let staged = staged.trim_end();
    if !unstaged.is_empty() {
        combined.push_str(unstaged);
    }
    if !staged.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(staged);
    }
    if combined.is_empty() {
        return ContextChip::new(
            ContextKind::Diff,
            None,
            "(no working-tree changes)".to_string(),
            false,
        );
    }
    let (content, truncated) = cap_head_lines(&combined, DIFF_MAX_LINES);
    ContextChip::new(ContextKind::Diff, None, content, truncated)
}

/// Build the `@terminal` chip from a capture already produced by
/// `TerminalView::capture_agent_context` (which honors a selection and applies
/// [`TERMINAL_MAX_LINES`]). `capture_truncated` carries that method's flag.
/// `None` when the terminal had nothing to attach.
pub fn terminal_chip(title: &str, text: String, capture_truncated: bool) -> Option<ContextChip> {
    if text.trim().is_empty() {
        return None;
    }
    let source = {
        let t = title.trim();
        if t.is_empty() { None } else { Some(t.to_string()) }
    };
    Some(ContextChip::new(ContextKind::Terminal, source, text, capture_truncated))
}

/// Build the `@browser` chip from an element picked in the embedded browser.
/// `markdown` is the block already formatted by
/// `browser_view::agent_context::format_pick` (identity, styles, HTML, paths);
/// `selector` becomes the chip's source so the label reads `@browser a#go`.
///
/// Unlike `@terminal`, the cap is applied here rather than at capture: the
/// picker is a page script and cannot be trusted to have bounded what it sent.
/// `None` when the capture was empty.
pub fn browser_chip(selector: &str, markdown: String) -> Option<ContextChip> {
    if markdown.trim().is_empty() {
        return None;
    }
    let source = {
        let s = selector.trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    };
    let (content, truncated) = cap_head_bytes(&markdown, BROWSER_MAX_BYTES);
    Some(ContextChip::new(ContextKind::Browser, source, content, truncated))
}

/// Trim `text` to at most `max_bytes`, keeping the HEAD and never splitting a
/// char. Returns `(text, truncated)`. The mirror of [`cap_tail_bytes`], used
/// where the *front* of the capture is the meaningful part — a picked element
/// leads with its identity and styles, and trails with its DOM path.
pub fn cap_head_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    // Walk back to the previous char boundary so the kept prefix is valid UTF-8.
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_chip_labels_with_the_selector() {
        let c = browser_chip("a#go", "Selected element:\n<a id=\"go\">x</a>".to_string()).unwrap();
        assert_eq!(c.kind, ContextKind::Browser);
        assert_eq!(c.source.as_deref(), Some("a#go"));
        assert!(!c.truncated);
        assert!(c.label().starts_with("@browser a#go · "));
    }

    #[test]
    fn browser_chip_is_none_for_an_empty_capture() {
        assert!(browser_chip("a#go", String::new()).is_none());
        assert!(browser_chip("a#go", "   \n ".to_string()).is_none());
    }

    #[test]
    fn browser_chip_without_a_selector_has_no_source() {
        let c = browser_chip("  ", "body".to_string()).unwrap();
        assert!(c.source.is_none());
        assert!(c.label().starts_with("@browser · "));
    }

    #[test]
    fn browser_chip_caps_an_oversized_capture() {
        let huge = "x".repeat(BROWSER_MAX_BYTES + 10);
        let c = browser_chip("div", huge).unwrap();
        assert!(c.truncated);
        assert_eq!(c.content.len(), BROWSER_MAX_BYTES);
    }

    #[test]
    fn cap_head_bytes_never_splits_a_char() {
        // "aéb" is 4 bytes: a | é(2) | b. A cap of 2 lands inside `é`, so the
        // kept prefix must back off to 1 byte rather than slice mid-char.
        let (out, trunc) = cap_head_bytes("aéb", 2);
        assert!(trunc);
        assert_eq!(out, "a");
        // A cap that already sits on a boundary keeps everything up to it.
        let (out, trunc) = cap_head_bytes("aéb", 3);
        assert!(trunc);
        assert_eq!(out, "aé");
    }

    #[test]
    fn cap_head_lines_keeps_all_when_under_limit() {
        let (out, trunc) = cap_head_lines("a\nb\nc", 10);
        assert_eq!(out, "a\nb\nc");
        assert!(!trunc);
    }

    #[test]
    fn cap_head_lines_clips_and_flags() {
        let (out, trunc) = cap_head_lines("a\nb\nc\nd", 2);
        assert_eq!(out, "a\nb");
        assert!(trunc);
    }

    #[test]
    fn cap_tail_bytes_keeps_all_when_under_limit() {
        let (out, trunc) = cap_tail_bytes("hello", 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
    }

    #[test]
    fn cap_tail_bytes_keeps_suffix() {
        let (out, trunc) = cap_tail_bytes("0123456789", 4);
        assert_eq!(out, "6789");
        assert!(trunc);
    }

    #[test]
    fn cap_tail_bytes_respects_char_boundary() {
        // Each `é` is 2 bytes; a naive byte cut at an odd offset would split it.
        let s = "aééé"; // 1 + 2 + 2 + 2 = 7 bytes
        let (out, trunc) = cap_tail_bytes(s, 5);
        assert!(trunc);
        // Must be valid UTF-8 and a suffix of the original.
        assert!(s.ends_with(&out));
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn clipboard_chip_none_for_empty_or_missing() {
        assert!(clipboard_chip(None).is_none());
        assert!(clipboard_chip(Some(String::new())).is_none());
    }

    #[test]
    fn clipboard_chip_caps_large_paste() {
        let big = "x".repeat(CLIPBOARD_MAX_BYTES + 100);
        let chip = clipboard_chip(Some(big)).unwrap();
        assert!(chip.truncated);
        assert_eq!(chip.content.len(), CLIPBOARD_MAX_BYTES);
        assert_eq!(chip.kind, ContextKind::Clipboard);
    }

    #[test]
    fn diff_chip_combines_unstaged_then_staged() {
        let chip = diff_chip("UNSTAGED", "STAGED");
        assert_eq!(chip.content, "UNSTAGED\nSTAGED");
        assert!(!chip.truncated);
    }

    #[test]
    fn diff_chip_placeholder_when_empty() {
        let chip = diff_chip("", "  \n ");
        assert_eq!(chip.content, "(no working-tree changes)");
        assert!(!chip.truncated);
    }

    #[test]
    fn diff_chip_caps_long_patch() {
        let patch = (0..DIFF_MAX_LINES + 50)
            .map(|i| format!("+line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chip = diff_chip(&patch, "");
        assert!(chip.truncated);
        assert_eq!(chip.content.lines().count(), DIFF_MAX_LINES);
    }

    #[test]
    fn terminal_chip_none_when_blank() {
        assert!(terminal_chip("build", "   \n  ".to_string(), false).is_none());
    }

    #[test]
    fn terminal_chip_carries_title_and_truncation() {
        let chip = terminal_chip("build", "error: boom".to_string(), true).unwrap();
        assert_eq!(chip.source.as_deref(), Some("build"));
        assert!(chip.truncated);
        assert_eq!(chip.kind, ContextKind::Terminal);
    }

    #[test]
    fn terminal_chip_untitled_has_no_source() {
        let chip = terminal_chip("  ", "out".to_string(), false).unwrap();
        assert!(chip.source.is_none());
    }

    #[test]
    fn source_labels_and_keys() {
        assert_eq!(ContextSource::diff().match_key, "diff");
        assert_eq!(ContextSource::clipboard().label, "@clipboard");
        let t = ContextSource::terminal(TerminalSessionId(7), "Build All");
        assert_eq!(t.label, "@terminal Build All");
        assert_eq!(t.match_key, "terminal build all");
        assert_eq!(t.request, ContextRequest::Terminal { id: TerminalSessionId(7), title: "Build All".into() });
    }
}
