//! The folded-thread projection the app renders.
//!
//! [`SessionSubscription`](oximux_remote_session::SessionSubscription) already
//! holds a [`ChatThread`] — the *same* `agent-core` fold the desktop renders. This
//! module projects it to JSON so React renders that fold rather than
//! reimplementing one in TypeScript, which would be a second implementation to
//! keep in step with the desktop's every time the event taxonomy grows.
//!
//! The whole read model crosses as one JSON string rather than as a projected
//! uniffi record for the same reason [`RemoteEvent`](crate::ffi_types::RemoteEvent)
//! and the git payloads do: the entry tree is deep and still evolving (nested
//! tool results, permission requests, plan entries, per-file turn diffs), and
//! mirroring every variant into an FFI type would mean re-cutting the bindings
//! on each change.

use oximux_agent_core::thread::{
    ChatThread, PlanEntryLite, ThreadEntry, ToolCallStatus, TurnUsage,
};
use serde::Serialize;

/// The folded thread, pushed to the app after each batch of applied events.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ThreadSnapshot {
    pub session_id: String,
    /// The fold cursor this snapshot reflects — the resume point on reconnect.
    pub seq: u64,
    /// The read model, shaped by [`ThreadJson`].
    pub thread_json: String,
    /// Whether the session is blocked awaiting the user. Lifted out of the JSON
    /// because the session list reads it to badge a row without parsing a
    /// transcript it does not otherwise need.
    pub awaiting_permission: bool,
}

/// The serialized shape of [`ThreadSnapshot::thread_json`].
///
/// A *borrowed* projection of [`ChatThread`], which is not itself `Serialize`:
/// it carries private fold cursors (the in-flight assistant index, the streaming
/// flags) that are fold bookkeeping, not read model, and would be misleading to
/// send. Borrowing also keeps the per-push cost to the serializer alone, with no
/// intermediate clone of the transcript.
#[derive(Serialize)]
struct ThreadJson<'a> {
    entries: &'a [ThreadEntry],
    model: Option<&'a str>,
    permission_mode: Option<&'a str>,
    title: Option<&'a str>,
    /// Drives the composer's send-vs-stop affordance.
    turn_active: bool,
    compacting: bool,
    last_error: Option<&'a str>,
    last_summary: Option<&'a str>,
    plan: Option<&'a [PlanEntryLite]>,
    /// The last settled turn's usage.
    usage: Option<&'a TurnUsage>,
    /// Mid-turn usage, for a live context meter.
    live_usage: Option<&'a TurnUsage>,
    /// The context-window denominator. Carried separately because Claude's live
    /// usage omits the window, so a meter reading only `live_usage` would have a
    /// numerator and no denominator mid-turn.
    context_window: Option<u64>,
    session_cost_usd: f64,
    slash_commands: &'a [String],
}

impl ThreadSnapshot {
    /// Project a folded thread for the app.
    pub(crate) fn build(session_id: &str, seq: u64, thread: &ChatThread) -> Option<Self> {
        let json = ThreadJson {
            entries: &thread.entries,
            model: thread.model.as_deref(),
            permission_mode: thread.permission_mode.as_deref(),
            title: thread.title.as_deref(),
            turn_active: thread.turn_active,
            compacting: thread.compacting,
            last_error: thread.last_error.as_deref(),
            last_summary: thread.last_summary.as_deref(),
            plan: thread.plan.as_deref(),
            usage: thread.usage.as_ref(),
            live_usage: thread.live_usage.as_ref(),
            context_window: thread.last_known_context_window,
            session_cost_usd: thread.session_cost_usd,
            slash_commands: &thread.slash_commands,
        };
        // A thread that will not serialize cannot be rendered, and pushing a
        // half-formed snapshot would be worse than pushing none: the app would
        // replace a good transcript with a broken one. Skipping leaves the last
        // good snapshot in place, and the next event tries again.
        let thread_json = serde_json::to_string(&json).ok()?;
        Some(Self {
            session_id: session_id.to_string(),
            seq,
            thread_json,
            awaiting_permission: awaits_user(&thread.entries),
        })
    }
}

/// Whether any tool call is blocked on the user — either a permission prompt or
/// an unanswered question. Both stop the turn dead, so both badge the session.
fn awaits_user(entries: &[ThreadEntry]) -> bool {
    entries.iter().any(|entry| match entry {
        ThreadEntry::ToolCall(call) => matches!(
            call.status,
            ToolCallStatus::WaitingForConfirmation(_) | ToolCallStatus::AwaitingAnswer(_)
        ),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use oximux_agent_core::thread::event::ThreadEvent;
    use oximux_agent_core::thread::tool_call::{PermissionKind, PermissionRequest};
    use serde_json::Value;

    use super::*;

    fn thread_with(events: &[ThreadEvent]) -> ChatThread {
        let mut thread = ChatThread::new();
        for event in events {
            thread.apply(event);
        }
        thread
    }

    /// The projection must carry the folded entries, not the raw events — this is
    /// the whole point of pushing a snapshot rather than an event stream.
    #[test]
    fn a_snapshot_carries_the_folded_entries() {
        let thread = thread_with(&[
            ThreadEvent::AssistantText("hello".into()),
            ThreadEvent::ToolCallStarted {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "ls" }),
            },
        ]);

        let snap = ThreadSnapshot::build("sess-1", 7, &thread).expect("serializes");
        let json: Value = serde_json::from_str(&snap.thread_json).expect("valid json");

        assert_eq!(snap.seq, 7);
        assert_eq!(snap.session_id, "sess-1");
        let entries = json["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2, "assistant message + tool call");
        assert_eq!(entries[0]["Assistant"]["text"], "hello");
        assert_eq!(entries[1]["ToolCall"]["name"], "Bash");
    }

    /// The badge the session list reads. Asserted against a real fold rather than
    /// a hand-built entry so it stays true if the fold's status handling changes.
    #[test]
    fn awaiting_permission_tracks_a_pending_request() {
        let mut thread = thread_with(&[ThreadEvent::ToolCallStarted {
            id: "t1".into(),
            name: "Bash".into(),
            input: Value::Null,
        }]);
        assert!(
            !ThreadSnapshot::build("s", 1, &thread).unwrap().awaiting_permission,
            "a running tool call is not awaiting the user"
        );

        thread.apply(&ThreadEvent::PermissionRequested {
            request_id: "req-1".into(),
            tool_use_id: Some("t1".into()),
            tool_name: "Bash".into(),
            input: Value::Null,
            description: "run ls".into(),
            suggestions: vec![],
            kind: PermissionKind::Tool,
        });

        let snap = ThreadSnapshot::build("s", 2, &thread).unwrap();
        assert!(snap.awaiting_permission, "a pending permission blocks the session");

        // And the request itself must survive into the JSON, since the card is
        // rendered from the entry rather than from the event that created it.
        let json: Value = serde_json::from_str(&snap.thread_json).unwrap();
        let status = &json["entries"][0]["ToolCall"]["status"];
        assert_eq!(status["WaitingForConfirmation"]["request_id"], "req-1");
        assert_eq!(status["WaitingForConfirmation"]["kind"], "tool", "snake_case on the wire");
    }

    /// A `PermissionRequest` the app must be able to answer: the `request_id` it
    /// echoes back has to round-trip through the JSON unchanged.
    #[test]
    fn a_resolved_request_stops_awaiting() {
        let mut thread = thread_with(&[ThreadEvent::ToolCallStarted {
            id: "t1".into(),
            name: "Edit".into(),
            input: Value::Null,
        }]);
        thread.apply(&ThreadEvent::PermissionRequested {
            request_id: "req-9".into(),
            tool_use_id: Some("t1".into()),
            tool_name: "Edit".into(),
            input: Value::Null,
            description: "edit a file".into(),
            suggestions: vec![],
            kind: PermissionKind::Tool,
        });
        assert!(ThreadSnapshot::build("s", 1, &thread).unwrap().awaiting_permission);

        // The tool running is what clears the prompt.
        thread.apply(&ThreadEvent::ToolResult {
            tool_use_id: "t1".into(),
            content: "done".into(),
            is_error: false,
            structured: None,
        });

        assert!(
            !ThreadSnapshot::build("s", 2, &thread).unwrap().awaiting_permission,
            "a completed tool call no longer blocks"
        );
    }

    /// `PermissionRequest` is a distinct type from the event that carries one;
    /// this pins the field names the app reads off the *entry* model.
    #[test]
    fn the_entry_permission_request_shape_is_stable() {
        let request = PermissionRequest {
            request_id: "r".into(),
            kind: PermissionKind::Plan,
            description: "# Plan".into(),
            suggestions: vec![],
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["request_id"], "r");
        assert_eq!(json["kind"], "plan");
        assert_eq!(json["description"], "# Plan");
    }
}
