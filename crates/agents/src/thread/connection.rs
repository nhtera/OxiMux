//! The `AgentConnection` trait + stdin serializers + a test stub.
//!
//! `AgentConnection` is the transport-agnostic seam: the app holds a
//! `Box<dyn AgentConnection>` and a `Receiver<ThreadEvent>`, drains events into
//! a `ChatThread`, and calls back on user actions (send a prompt, answer a
//! permission). The Claude `stream-json` impl lives in `claude_stream_json`; a
//! future ACP impl would satisfy the same trait.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Value};

use super::event::ThreadEvent;
use super::tool_call::PermissionDecision;

/// What a backend can do, so the UI shows/hides controls by capability instead
/// of branching on a hard-coded provider name. Defaults to the most
/// conservative answer (nothing supported); each backend overrides what it can
/// actually do via [`AgentConnection::capabilities`]. Grown here once so a
/// future ACP backend advertises its own shape without a trait change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// Fields become live once the UI gates its usage/reasoning/mode controls on
// `capabilities()`; until a control reads one, the field is intentionally unused.
#[allow(dead_code)]
pub struct AgentCapabilities {
    /// Permission/edit modes can be set at runtime (ACP `session/set_mode`).
    pub supports_modes: bool,
    /// The backend advertises slash commands the UI can offer.
    pub supports_slash: bool,
    /// The backend accepts arbitrary config (e.g. a reasoning-effort control).
    pub supports_config: bool,
    /// Turns carry token/cost usage the UI can meter.
    pub emits_usage: bool,
}

/// The user-facing control surface for one chat session.
///
/// Everything past the first three methods is **default-implemented** so
/// existing impls (and the test stub) compile unchanged; a backend overrides
/// only what it supports. The trait is deliberately grown once, provider-
/// agnostically, so the future ACP backend satisfies the same seam.
pub trait AgentConnection: Send {
    /// Send a user prompt, starting a new turn. (The transport also accepts a
    /// message mid-turn, but the chat UI currently gates sending behind Stop
    /// while a turn is streaming, so a live steer isn't issued from the UI.)
    fn send_user_message(&self, text: &str) -> Result<()>;
    /// Answer a pending permission request by its `request_id`.
    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()>;
    /// Terminate the session and its process.
    fn shutdown(&self);

    /// Interrupt the in-flight turn. Claude: SIGINT the child (which ends the
    /// turn and exits the process — the caller resumes on the next send). ACP:
    /// `session/cancel`. Default is a no-op for backends that can't interrupt.
    fn cancel(&self) -> Result<()> {
        Ok(())
    }

    /// What this backend supports; the UI gates controls on it. Default: none.
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }

    /// Switch the permission/edit mode at runtime (ACP). Unsupported by default.
    fn set_mode(&self, _mode: &str) -> Result<()> {
        anyhow::bail!("this agent does not support changing mode at runtime")
    }

    /// Set a backend config value at runtime (ACP). Unsupported by default.
    fn set_config(&self, _key: &str, _value: Value) -> Result<()> {
        anyhow::bail!("this agent does not support runtime configuration")
    }
}

/// Build the stdin JSON for a user message (stream-json input format).
pub fn user_message_json(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}})
}

/// Build the stdin `control_response` JSON answering a `can_use_tool` request.
///
/// Fail-closed contract (verified against the live CLI):
/// - **allow** MUST echo `updatedInput` — an allow without it is treated as
///   malformed by the CLI and the tool is effectively denied.
/// - **allow + suggestion** additionally echoes the agent's suggestion verbatim
///   under `updatedPermissions` (e.g. `setMode: acceptEdits`), which the CLI
///   applies so it stops prompting for that tool/scope this session. A plain
///   allow (no `updatedPermissions`) re-prompts the next call — the distinction
///   is what makes "Allow always" stick.
/// - **deny** carries a `message` shown to the model.
pub fn control_response_json(request_id: &str, decision: &PermissionDecision) -> Value {
    let response = match decision {
        PermissionDecision::Allow { updated_input } => {
            json!({"behavior": "allow", "updatedInput": updated_input})
        }
        PermissionDecision::AllowWithSuggestion { updated_input, suggestion } => {
            json!({"behavior": "allow", "updatedInput": updated_input,
                   "updatedPermissions": [suggestion.raw]})
        }
        PermissionDecision::Deny { message } => {
            json!({"behavior": "deny", "message": message})
        }
    };
    json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": response}})
}

/// A test double: records everything sent to the "agent" and lets a test inject
/// `ThreadEvent`s (via the returned `Sender`) as if the agent produced them.
/// Used to exercise the app-facing loop (drain events → `ChatThread`; user
/// actions → recorded stdin) without spawning a real subprocess.
#[derive(Clone, Default)]
pub struct StubConnection {
    sent: Arc<Mutex<Vec<Value>>>,
}

impl StubConnection {
    /// Returns the stub, the event receiver the app would drain, and the
    /// sender a test uses to inject agent events.
    pub fn new() -> (Self, Receiver<ThreadEvent>, Sender<ThreadEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self::default(), rx, tx)
    }

    /// The JSON payloads that were written to the agent's stdin, in order.
    pub fn sent(&self) -> Vec<Value> {
        self.sent.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn record(&self, v: Value) {
        if let Ok(mut g) = self.sent.lock() {
            g.push(v);
        }
    }
}

impl AgentConnection for StubConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.record(user_message_json(text));
        Ok(())
    }
    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        self.record(control_response_json(request_id, &decision));
        Ok(())
    }
    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::state::ChatThread;
    use crate::thread::tool_call::PermissionSuggestion;

    #[test]
    fn user_message_json_shape() {
        assert_eq!(
            user_message_json("hi"),
            json!({"type":"user","message":{"role":"user","content":"hi"}})
        );
    }

    #[test]
    fn allow_response_carries_updated_input() {
        let d = PermissionDecision::Allow { updated_input: json!({"file_path": "a"}) };
        let v = control_response_json("rid-1", &d);
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "rid-1");
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        // updatedInput is REQUIRED — a bare allow is malformed.
        assert_eq!(v["response"]["response"]["updatedInput"], json!({"file_path": "a"}));
    }

    #[test]
    fn deny_response_carries_message() {
        let d = PermissionDecision::Deny { message: "no".into() };
        let v = control_response_json("rid-2", &d);
        assert_eq!(v["response"]["response"]["behavior"], "deny");
        assert_eq!(v["response"]["response"]["message"], "no");
        assert!(v["response"]["response"].get("updatedInput").is_none());
    }

    #[test]
    fn stub_uses_conservative_default_capabilities_and_cancel() {
        // A backend that doesn't override the grown-once methods gets no-op
        // defaults: cancel succeeds silently, capabilities advertise nothing,
        // and runtime mode/config are refused.
        let stub = StubConnection::default();
        assert!(stub.cancel().is_ok(), "default cancel is a no-op success");
        assert_eq!(stub.capabilities(), super::AgentCapabilities::default());
        assert!(!stub.capabilities().emits_usage);
        assert!(stub.set_mode("acceptEdits").is_err(), "runtime mode refused by default");
        assert!(stub.set_config("reasoning", json!("high")).is_err());
    }

    #[test]
    fn allow_with_suggestion_echoes_updated_permissions() {
        // "Allow always": allow this call AND apply the suggestion verbatim
        // under updatedPermissions, so the CLI stops re-prompting.
        let raw = json!({"type": "setMode", "mode": "acceptEdits", "destination": "session"});
        let d = PermissionDecision::AllowWithSuggestion {
            updated_input: json!({"file_path": "a"}),
            suggestion: PermissionSuggestion {
                kind: "setMode".into(), label: "Always (acceptEdits)".into(), raw: raw.clone(),
            },
        };
        let r = control_response_json("r", &d);
        let inner = &r["response"]["response"];
        assert_eq!(inner["behavior"], "allow");
        assert_eq!(inner["updatedInput"], json!({"file_path": "a"}));
        assert_eq!(inner["updatedPermissions"], json!([raw]));

        // A plain allow must NOT carry updatedPermissions (else every allow
        // would stick as always-allow).
        let plain = control_response_json("r", &PermissionDecision::Allow { updated_input: json!({}) });
        assert!(plain["response"]["response"].get("updatedPermissions").is_none());
    }

    /// The full app-facing loop: inject agent events → they drive a ChatThread;
    /// answering the permission → the stub records the exact allow JSON.
    #[test]
    fn stub_drives_thread_and_records_decision() {
        let (conn, rx, inject) = StubConnection::new();
        let mut thread = ChatThread::new();

        // user sends a prompt
        conn.send_user_message("edit notes").unwrap();
        thread.push_user_message("edit notes");

        // agent streams: tool_use + a permission request
        inject.send(ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Edit".into(), input: json!({"file_path": "notes.txt"}),
        }).unwrap();
        inject.send(ThreadEvent::PermissionRequested {
            request_id: "rid-9".into(), tool_use_id: Some("toolu_1".into()),
            tool_name: "Edit".into(), input: json!({"file_path": "notes.txt"}),
            description: "notes.txt".into(), suggestions: vec![],
        }).unwrap();
        while let Ok(ev) = rx.try_recv() {
            thread.apply(&ev);
        }

        // the UI would now show a pending permission; the user allows it
        let (tool_id, req) = thread.pending_permission().expect("pending");
        assert_eq!(tool_id, "toolu_1");
        conn.resolve_permission(
            &req.request_id.clone(),
            PermissionDecision::Allow { updated_input: json!({"file_path": "notes.txt"}) },
        ).unwrap();

        // stub recorded: [user message, control_response allow]
        let sent = conn.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["message"]["content"], "edit notes");
        assert_eq!(sent[1]["response"]["response"]["behavior"], "allow");
        assert_eq!(sent[1]["response"]["request_id"], "rid-9");
    }
}
