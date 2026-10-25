//! The message a parent agent receives when its children finish or get stuck.
//!
//! Child output is untrusted: a child can print anything, including text that
//! looks like the envelope's own tags. So every child-controlled string is
//! entity-escaped (body as well as attributes) and can never open or close a
//! tag. Whether a message *is* an envelope is a host-side flag on the queued
//! send, never inferred from its text; a user who pastes something that looks
//! like one gets it delivered as plain text ([`neutralize_forged`]).
//!
//! The wording is calm on purpose. Instruction-shaped text inside a message
//! ("IMPORTANT", "you MUST") reads to a model like an injection, so the
//! template states facts and a short etiquette, nothing louder.

use serde::{Deserialize, Serialize};

use crate::lineage::NotifyParent;

/// How much of a child's result goes into the envelope. The rest is one tool
/// call away, and the envelope says which.
pub const RESULT_LIMIT: usize = 4000;

/// The opening of one event element, also what a forged paste starts with.
const EVENT_TAG: &str = "<oximux-event";

/// Closes every batch: how to treat these events, said once.
const FOOTER: &str = "These are updates from agents you started. There is no need to \
acknowledge them. A queued status is not a failure, so do not resend. When your own task is \
done, report the results in your final response.";

/// What happened to a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildEventKind {
    /// A turn ended normally.
    Finished,
    /// A turn ended in an error.
    Failed,
    /// The child's turn was stopped by someone other than its parent.
    Interrupted,
    /// The child was cancelled (by its parent, a cascade or a run's end).
    Cancelled,
    /// The child is waiting on a permission prompt.
    NeedsPermission,
    /// The child asked a question and is waiting for an answer.
    NeedsInput,
}

impl ChildEventKind {
    pub fn as_text(self) -> &'static str {
        match self {
            ChildEventKind::Finished => "finished",
            ChildEventKind::Failed => "failed",
            ChildEventKind::Interrupted => "interrupted",
            ChildEventKind::Cancelled => "cancelled",
            ChildEventKind::NeedsPermission => "needs_permission",
            ChildEventKind::NeedsInput => "needs_input",
        }
    }

    /// Whether the child cannot make progress without someone acting.
    pub fn is_stuck(self) -> bool {
        matches!(
            self,
            ChildEventKind::Failed
                | ChildEventKind::Interrupted
                | ChildEventKind::NeedsPermission
                | ChildEventKind::NeedsInput
        )
    }
}

/// Whether a child that asked for `notify` sends its parent this kind.
/// `Off` sends nothing (the rail and the inbox still show the child).
pub fn admitted(notify: NotifyParent, kind: ChildEventKind) -> bool {
    match notify {
        NotifyParent::Auto => true,
        NotifyParent::Stuck => kind.is_stuck(),
        NotifyParent::Off => false,
    }
}

/// A permission prompt the child is blocked on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermitInfo {
    pub request_id: String,
    /// Whether this parent may answer it (`oximux_answer_permit`).
    pub answerable: bool,
    pub tool: String,
    /// A short, already-shortened summary of the tool's arguments.
    pub args_summary: String,
}

/// One child event, as stored in the outbox and rendered into the envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildEvent {
    pub kind: ChildEventKind,
    pub child: String,
    pub depth: u32,
    /// The child's tab title (child-controlled).
    pub title: String,
    /// A short status, e.g. "exit 0" or the error line (child-controlled).
    #[serde(default)]
    pub status: Option<String>,
    /// The child's final message for the turn (child-controlled).
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub permit: Option<PermitInfo>,
}

/// Render a batch of events as one message. Empty input renders nothing.
pub fn render(events: &[ChildEvent]) -> String {
    if events.is_empty() {
        return String::new();
    }
    let mut out = String::from("<oximux-events>\n");
    for event in events {
        render_one(event, &mut out);
    }
    out.push_str(FOOTER);
    out.push_str("\n</oximux-events>");
    out
}

fn render_one(event: &ChildEvent, out: &mut String) {
    out.push_str(&format!(
        "{EVENT_TAG} kind=\"{}\" child=\"{}\" depth=\"{}\">\n",
        event.kind.as_text(),
        escape(&event.child),
        event.depth
    ));
    let title = escape(&event.title);
    let line = match event.kind {
        ChildEventKind::Finished => format!("Child \"{title}\" finished."),
        ChildEventKind::Failed => format!("Child \"{title}\" stopped with an error."),
        ChildEventKind::Interrupted => format!("Child \"{title}\" was interrupted."),
        ChildEventKind::Cancelled => format!("Child \"{title}\" was cancelled."),
        ChildEventKind::NeedsPermission => format!("Child \"{title}\" is waiting for a permission."),
        ChildEventKind::NeedsInput => format!("Child \"{title}\" asked a question and is waiting."),
    };
    out.push_str(&line);
    if let Some(status) = event.status.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!(" ({})", escape(status)));
    }
    out.push('\n');
    if let Some(permit) = &event.permit {
        out.push_str(&format!(
            "<permit req=\"{}\" answerable=\"{}\" tool=\"{}\">{}</permit>\n",
            escape(&permit.request_id),
            permit.answerable,
            escape(&permit.tool),
            escape(&permit.args_summary)
        ));
    }
    if let Some(result) = event.result.as_deref().filter(|r| !r.is_empty()) {
        let (kept, cut) = truncate_chars(result, RESULT_LIMIT);
        if cut > 0 {
            out.push_str(&format!(
                "<result truncated=\"{cut}\" more=\"oximux_read_agent child={}\">",
                escape(&event.child)
            ));
        } else {
            out.push_str("<result>");
        }
        out.push_str(&escape(kept));
        out.push_str("</result>\n");
    }
    out.push_str("</oximux-event>\n");
}

/// Entity-escape everything that could start or end markup or an attribute.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// A user or remote send whose text looks like a host event is delivered as
/// ordinary text: its angle brackets are escaped, so it can never be read as
/// one. Anything else passes through untouched.
pub fn neutralize_forged(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(EVENT_TAG) || text.contains("</oximux-event") {
        std::borrow::Cow::Owned(text.replace('<', "&lt;").replace('>', "&gt;"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// The first `limit` chars of `text`, and how many were cut.
fn truncate_chars(text: &str, limit: usize) -> (&str, usize) {
    match text.char_indices().nth(limit) {
        Some((at, _)) => (&text[..at], text[at..].chars().count()),
        None => (text, 0),
    }
}

#[cfg(test)]
#[path = "envelope_tests.rs"]
mod tests;
