//! Context chips: user-attached external context (a sibling terminal's output, a
//! working-tree diff, the clipboard) staged in the chat composer and serialized
//! into the outgoing message as tagged text blocks.
//!
//! Pure data + a serializer — no gpui, no process access. The app layer captures
//! the content (see `agent_chat::context_providers`) and hands a ready
//! [`ContextChip`] here; this module only models it and renders it onto the wire.
//! Transport-agnostic: plain `<context>` text blocks work on the current
//! stream-json backend and map cleanly to ACP content blocks later.

use serde::{Deserialize, Serialize};

/// Where a chip's content came from. Drives both the chip's label in the composer
/// and the `name="…"` attribute on the wire block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextKind {
    /// Output (or the live selection) of a sibling terminal tab.
    Terminal,
    /// The chat cwd's working-tree git diff (staged + unstaged).
    Diff,
    /// The current text clipboard.
    Clipboard,
}

impl ContextKind {
    /// The stable wire name emitted as `<context name="…">`. Kept lowercase and
    /// word-only so the model reads it as a source tag, not prose.
    pub fn wire_name(self) -> &'static str {
        match self {
            ContextKind::Terminal => "terminal",
            ContextKind::Diff => "diff",
            ContextKind::Clipboard => "clipboard",
        }
    }
}

/// One staged piece of attached context. `content` is captured at chip-creation
/// time (what the user saw is what's sent) and is already capped by the provider;
/// `truncated` records whether that cap clipped it, so the wire block and the UI
/// can both show a visible marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextChip {
    pub kind: ContextKind,
    /// Human detail for the source — a terminal tab's title. `None` for diff /
    /// clipboard, which have no sub-identity.
    pub source: Option<String>,
    pub content: String,
    pub truncated: bool,
}

impl ContextChip {
    pub fn new(
        kind: ContextKind,
        source: Option<String>,
        content: String,
        truncated: bool,
    ) -> Self {
        Self {
            kind,
            source,
            content,
            truncated,
        }
    }

    /// Lines in the captured content — surfaced in the chip label so the user sees
    /// the prompt cost of what they attached.
    pub fn line_count(&self) -> usize {
        if self.content.is_empty() {
            0
        } else {
            self.content.lines().count()
        }
    }

    /// The composer chip's label: `@diff · 128 lines`, `@terminal build · 42
    /// lines`, `@clipboard · 3 lines`. A truncated capture marks the count with a
    /// `+` so the user knows more was clipped.
    pub fn label(&self) -> String {
        let mut base = format!("@{}", self.kind.wire_name());
        if let Some(src) = self.source.as_deref().filter(|s| !s.is_empty()) {
            base.push(' ');
            base.push_str(src);
        }
        let n = self.line_count();
        let trunc = if self.truncated { "+" } else { "" };
        let plural = if n == 1 && !self.truncated { "" } else { "s" };
        format!("{base} · {n}{trunc} line{plural}")
    }

    /// Serialize this chip as one `<context>` block. A trailing newline separates
    /// content from the closing tag; a truncated capture adds a visible marker so
    /// the model knows the content was clipped.
    fn write_block(&self, out: &mut String) {
        out.push_str("<context name=\"");
        out.push_str(self.kind.wire_name());
        out.push('"');
        if let Some(src) = self.source.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(" source=\"");
            // Keep the attribute value tag-safe: swap embedded double-quotes for
            // single so the opening tag can't be broken by a terminal title.
            out.push_str(&src.replace('"', "'"));
            out.push('"');
        }
        out.push_str(">\n");
        out.push_str(&self.content);
        if !self.content.ends_with('\n') {
            out.push('\n');
        }
        if self.truncated {
            out.push_str("[content truncated to fit]\n");
        }
        out.push_str("</context>");
    }
}

/// Prepend the staged chips to the user's message as tagged `<context>` blocks,
/// each named by its source and separated from the message by a blank line.
/// Returns `message` unchanged when there are no chips. Chips are emitted in
/// staging order, so what the user sees above the composer is the order the model
/// reads.
pub fn prepend_context(chips: &[ContextChip], message: &str) -> String {
    if chips.is_empty() {
        return message.to_string();
    }
    let mut out = String::new();
    for chip in chips {
        chip.write_block(&mut out);
        out.push_str("\n\n");
    }
    out.push_str(message);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(kind: ContextKind, source: Option<&str>, content: &str, truncated: bool) -> ContextChip {
        ContextChip::new(kind, source.map(str::to_string), content.to_string(), truncated)
    }

    #[test]
    fn no_chips_returns_message_unchanged() {
        assert_eq!(prepend_context(&[], "hello"), "hello");
    }

    #[test]
    fn single_diff_block_wraps_content() {
        let c = chip(ContextKind::Diff, None, "diff --git a b\n+line", false);
        let out = prepend_context(std::slice::from_ref(&c), "what changed?");
        assert_eq!(
            out,
            "<context name=\"diff\">\ndiff --git a b\n+line\n</context>\n\nwhat changed?"
        );
    }

    #[test]
    fn terminal_block_carries_source_attribute() {
        let c = chip(ContextKind::Terminal, Some("build"), "error: boom", false);
        let out = prepend_context(std::slice::from_ref(&c), "");
        assert!(out.starts_with("<context name=\"terminal\" source=\"build\">\n"));
        assert!(out.contains("error: boom"));
    }

    #[test]
    fn truncated_capture_emits_marker() {
        let c = chip(ContextKind::Clipboard, None, "a\nb", true);
        let out = prepend_context(std::slice::from_ref(&c), "x");
        assert!(out.contains("[content truncated to fit]"));
    }

    #[test]
    fn non_truncated_capture_has_no_marker() {
        let c = chip(ContextKind::Clipboard, None, "a\nb", false);
        let out = prepend_context(std::slice::from_ref(&c), "x");
        assert!(!out.contains("[content truncated to fit]"));
    }

    #[test]
    fn multiple_chips_preserve_staging_order() {
        let chips = [
            chip(ContextKind::Diff, None, "D", false),
            chip(ContextKind::Clipboard, None, "C", false),
        ];
        let out = prepend_context(&chips, "go");
        let diff_at = out.find("name=\"diff\"").unwrap();
        let clip_at = out.find("name=\"clipboard\"").unwrap();
        assert!(diff_at < clip_at, "diff must serialize before clipboard");
        assert!(out.ends_with("go"));
    }

    #[test]
    fn source_quotes_are_sanitized() {
        let c = chip(ContextKind::Terminal, Some("say \"hi\""), "x", false);
        let out = prepend_context(std::slice::from_ref(&c), "");
        assert!(out.contains("source=\"say 'hi'\""));
        // The only double-quotes left belong to the attribute delimiters.
        assert_eq!(out.matches('"').count(), 4);
    }

    #[test]
    fn content_without_trailing_newline_gets_one_before_close() {
        let c = chip(ContextKind::Diff, None, "no newline", false);
        let out = prepend_context(std::slice::from_ref(&c), "");
        assert!(out.contains("no newline\n</context>"));
    }

    #[test]
    fn content_with_trailing_newline_is_not_doubled() {
        let c = chip(ContextKind::Diff, None, "has newline\n", false);
        let out = prepend_context(std::slice::from_ref(&c), "");
        assert!(out.contains("has newline\n</context>"));
        assert!(!out.contains("has newline\n\n</context>"));
    }

    #[test]
    fn label_reports_line_count_and_source() {
        let c = chip(ContextKind::Terminal, Some("build"), "a\nb\nc", false);
        assert_eq!(c.label(), "@terminal build · 3 lines");
    }

    #[test]
    fn label_singular_line() {
        let c = chip(ContextKind::Diff, None, "only one", false);
        assert_eq!(c.label(), "@diff · 1 line");
    }

    #[test]
    fn label_truncated_marks_count() {
        let c = chip(ContextKind::Clipboard, None, "a\nb", true);
        assert_eq!(c.label(), "@clipboard · 2+ lines");
    }

    #[test]
    fn label_empty_content_is_zero_lines() {
        let c = chip(ContextKind::Clipboard, None, "", false);
        assert_eq!(c.label(), "@clipboard · 0 lines");
    }
}
