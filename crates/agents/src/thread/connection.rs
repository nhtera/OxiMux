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

use super::entry::ChatImage;
use super::event::ThreadEvent;
use super::question::{updated_input_json, AskQuestion, QuestionAnswers};
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
    /// The backend keeps an on-disk session log the rewind/fork truncate-fork
    /// can read (`~/.claude/projects/*.jsonl`). Claude sets this true; ACP
    /// backends without such a log report false → the UI hides rewind for them.
    pub supports_rewind: bool,
    /// The backend accepts a message **during** a live turn and delivers it at the
    /// next turn boundary, redirecting the agent before its next model call
    /// (pi `steer`). False for backends where a mid-turn message has nowhere to go
    /// — there the composer parks it and sends it when the turn ends, which is the
    /// default for everything else.
    pub supports_steer: bool,
}

/// One selectable model for the model picker. `wire` is passed to the backend
/// as-is (Claude: `--model opus`); `label` is the human-readable name shown in
/// the menu (the toolbar trigger strips any `provider/` namespace off it). For
/// backends without a distinct display name, `label` mirrors `wire`.
///
/// `description` is an optional one-line capability blurb ("Most capable",
/// "Fastest", or an ACP agent's own model description) rendered muted beneath the
/// name in the searchable picker, and also matched by search. `None` renders a
/// single-line row.
// Serializable so the process-wide catalog cache can persist a probed model
// list to disk and seed the picker instantly on the next launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelChoice {
    pub wire: String,
    pub label: String,
    pub description: Option<String>,
}

/// One permission/edit mode for the mode picker: `wire` is the backend value,
/// `label` is what the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeChoice {
    pub wire: String,
    pub label: String,
}

/// What a backend knows about one of its own slash commands.
///
/// Most backends advertise command *names* only, which is why the app recovers
/// descriptions and grouping by scanning the CLI's on-disk definitions. That scan
/// encodes one CLI's directory layout, so it is only correct for the backend it
/// was written against. A backend that can describe its own commands fills this
/// instead (pi's `get_commands` returns all of it in one request), and the app
/// skips the scan rather than enriching one agent's commands from another's
/// config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandInfo {
    /// Without the leading slash, exactly as it must be typed.
    pub name: String,
    pub description: Option<String>,
    /// Whether the palette files this under Skills rather than Built-in.
    pub is_skill: bool,
    /// Attribution shown muted on the right of the row (pi: `user`/`project`).
    pub source_label: Option<String>,
}

/// One reasoning-effort level for the effort picker, `(wire, label)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortChoice {
    pub wire: String,
    pub label: String,
}

/// One selectable option for a `Select`-kind [`FeatureControl`]. `wire` is the
/// value echoed back to the backend on change; `label` is what the user sees;
/// `description` is an optional muted blurb. Mirrors [`ModelChoice`] so the same
/// searchable-row rendering can be reused.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeatureSelectOption {
    pub wire: String,
    pub label: String,
    pub description: Option<String>,
}

/// The shape of a generic composer feature control. `Toggle` renders as an
/// icon button that flips on/off; `Select` renders as a labeled dropdown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FeatureKind {
    Toggle { on: bool },
    Select { options: Vec<FeatureSelectOption>, selected: Option<String> },
}

/// One backend-advertised control in the composer's feature cluster — the
/// generic seam behind fast mode, plan mode, auto-accept, agent-profile, and any
/// other per-provider switch. Backends fill [`AgentConnection::features`] with
/// whatever they support; the view renders whatever comes back, so no
/// per-provider vocabulary lives in the UI.
///
/// `id` is the stable key echoed back on change (for ACP it is the
/// `session/set_config_option` option id). `icon` is a *semantic* glyph hint
/// (e.g. `"zap"`, `"plan"`, `"bot"`, `"settings"`) — kept as a string so this
/// crate carries no UI asset path; the view maps it to an icon.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeatureControl {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub kind: FeatureKind,
}

/// The value a feature control carries back to the backend on change: a boolean
/// for a toggle, a chosen option `wire` for a select. Serializable so a session's
/// picked feature values can be persisted and replayed on restore.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FeatureValue {
    Bool(bool),
    Choice(String),
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

    /// Send a user prompt that carries attached images. Default falls back to
    /// text-only [`Self::send_user_message`] (dropping the images), so backends
    /// that don't support image input compile unchanged; the Claude backend
    /// overrides this to emit a base64 image content block per attachment.
    fn send_user_message_with_images(&self, text: &str, images: &[ChatImage]) -> Result<()> {
        let _ = images;
        self.send_user_message(text)
    }

    /// Send a message into the turn that is already streaming, to redirect it.
    ///
    /// Only meaningful for an [`AgentCapabilities::supports_steer`] backend. The
    /// message is delivered at the next turn boundary — after the running tool
    /// finishes, before the agent's next model call — and arrives as an ordinary
    /// user message, so the caller pushes the user bubble exactly as it would for
    /// a normal send.
    ///
    /// **Not withdrawable.** pi exposes no way to un-queue a steered message over
    /// RPC (`clearQueue` exists on its session object but is not an RPC command),
    /// so this is only ever called when the user explicitly says "send this now"
    /// — never as the automatic destination for a message they might still edit
    /// or cancel. Unsupported by default.
    fn steer(&self, _text: &str) -> Result<()> {
        anyhow::bail!("this agent cannot accept a message mid-turn")
    }

    /// Answer a pending permission request by its `request_id`.
    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()>;

    /// Answer a pending `AskUserQuestion` by its `request_id`, sending the user's
    /// selections back as a control_response. Default: unsupported (so ACP/stub
    /// backends compile unchanged); the Claude backend overrides it.
    fn answer_question(
        &self,
        request_id: &str,
        questions: &[AskQuestion],
        answers: &QuestionAnswers,
    ) -> Result<()> {
        let _ = (request_id, questions, answers);
        anyhow::bail!("this agent does not support answering questions")
    }

    /// Terminate the session and its process.
    fn shutdown(&self);

    /// Interrupt the in-flight turn. Claude: SIGINT the child (which ends the
    /// turn and exits the process — the caller resumes on the next send). ACP:
    /// `session/cancel`. Default is a no-op for backends that can't interrupt.
    fn cancel(&self) -> Result<()> {
        Ok(())
    }

    /// Interrupt AND block until the agent process is confirmed dead (reaped).
    /// Required before any operation that reads the agent's on-disk session
    /// file (rewind's truncate-fork): `cancel()` alone is fire-and-forget and
    /// the process may still be flushing its transcript. BLOCKS the calling
    /// thread — run it off the UI foreground. Default falls back to `cancel`
    /// for backends without a process to reap.
    fn cancel_and_wait(&self) -> Result<()> {
        self.cancel()
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

    /// Authenticate with the backend using the method the user picked from an
    /// [`ThreadEvent::AuthRequired`] card (ACP `authenticate`). The backend then
    /// retries the session open on the SAME connection (no respawn). Unsupported
    /// by default (Claude/Codex handle their own login out of band), so those
    /// backends compile unchanged.
    fn authenticate(&self, _method_id: &str) -> Result<()> {
        anyhow::bail!("this agent does not support in-app authentication")
    }

    /// Begin a browser-based OAuth sign-in ([`AuthMethodKind::BrowserOauth`]).
    /// Fire-and-forget so a UI click never blocks on the RPC: the backend runs
    /// `account/login/start` on its worker and emits the resulting
    /// [`ThreadEvent::AuthUrl`] for the app to open in a browser, then a later
    /// [`ThreadEvent::AuthOutcome`] when the flow resolves. Unsupported by
    /// default (Claude/ACP use `authenticate` or out-of-band login).
    fn begin_browser_login(&self) -> Result<()> {
        anyhow::bail!("this agent does not support browser sign-in")
    }

    /// Switch the model at runtime. ACP has no first-class model concept, so an
    /// ACP backend maps this to its `Model`-category select config option
    /// (`session/set_config_option`) — the switch stays in-session. Unsupported
    /// by default → the app falls back to a resume-respawn on the new `--model`
    /// (Claude/Codex fix the model at spawn). Returning `Ok` is what tells the
    /// app to skip that respawn.
    fn set_model(&self, _model: &str) -> Result<()> {
        anyhow::bail!("this agent does not support changing model at runtime")
    }

    /// Switch the reasoning effort at runtime. ACP maps this to its
    /// `ThoughtLevel`-category select config option (`session/set_config_option`),
    /// keeping the switch in-session. Unsupported by default → the app falls back
    /// to a resume-respawn on the new `--effort` (Claude fixes effort at spawn).
    /// Returning `Ok` is what tells the app to skip that respawn.
    fn set_effort(&self, _effort: &str) -> Result<()> {
        anyhow::bail!("this agent does not support changing effort at runtime")
    }

    /// The model choices this backend offers in the model picker. Default: none
    /// (the picker then shows only the current model as static text).
    fn models(&self) -> Vec<ModelChoice> {
        Vec::new()
    }

    /// The permission/edit modes this backend offers in the mode picker.
    /// Default: none.
    fn permission_modes(&self) -> Vec<ModeChoice> {
        Vec::new()
    }

    /// What this backend knows about its own slash commands. Default: nothing →
    /// the app falls back to scanning the CLI's on-disk definitions for
    /// descriptions and grouping. A non-empty answer replaces that scan entirely;
    /// see [`SlashCommandInfo`].
    fn slash_commands(&self) -> Vec<SlashCommandInfo> {
        Vec::new()
    }

    /// The reasoning-effort levels this backend offers in the effort picker.
    /// Default: none.
    fn efforts(&self) -> Vec<EffortChoice> {
        Vec::new()
    }

    /// The model shown as current when the user hasn't picked one (the
    /// backend's own default). Default: none.
    fn default_model(&self) -> Option<String> {
        None
    }

    /// The current model's context window, when the backend knows it **without
    /// having run a turn** — the context meter's denominator.
    ///
    /// Default `None`: Claude and Codex report a window only inside a turn's
    /// usage, so their meter stays count-only until the first reply and this
    /// changes nothing for them. Pi reports `contextWindow` per model at the
    /// handshake, so it can seed the meter at connect and move it on a live model
    /// switch (the windows differ by model — 272K vs 128K — so a denominator
    /// pinned at connect would silently measure against the wrong one).
    fn context_window(&self) -> Option<u64> {
        None
    }

    /// The permission mode shown as current when unset. Default: none.
    fn default_mode(&self) -> Option<String> {
        None
    }

    /// The reasoning effort shown as current when unset. Default: none.
    fn default_effort(&self) -> Option<String> {
        None
    }

    /// The generic feature controls this backend offers in the composer's
    /// feature cluster (fast mode, plan mode, auto-accept, agent-profile, …).
    /// Default: none → the cluster stays hidden. A backend fills this with
    /// whatever it advertises; the view renders each without any hardcoded
    /// per-provider knowledge.
    fn features(&self) -> Vec<FeatureControl> {
        Vec::new()
    }

    /// Apply a feature-control change at runtime. ACP maps this to a
    /// `session/set_config_option` write (in-session, no respawn). Unsupported by
    /// default → the app falls back to a resume-respawn that folds the new value
    /// into spawn args. Returning `Ok` tells the app to skip that respawn.
    fn set_feature(&self, _id: &str, _value: FeatureValue) -> Result<()> {
        anyhow::bail!("this agent does not support changing features at runtime")
    }

    /// Whether this backend rewinds the conversation *server-side* on the LIVE
    /// connection (Codex `thread/fork`), rather than the client's kill-then-fork
    /// of an on-disk session log (Claude). The rewind flow branches on this: a
    /// server-side backend forks BEFORE the process is stopped and needs no file
    /// fork; a client-side backend keeps the existing kill → fork-file → respawn
    /// path. Default `false` (Claude / file-based).
    fn rewind_is_server_side(&self) -> bool {
        false
    }

    /// Rewind the conversation to before the user message at `user_ordinal`
    /// (0-based), returning the NEW session id the truncated conversation
    /// continues under. `total_user_messages` is the transcript's full user-turn
    /// count — the backend fails closed if its own turn ledger doesn't account for
    /// all of them (e.g. a restored session whose prior turns weren't replayed, so
    /// ordinals can't be mapped to turn ids), rather than forking at the wrong
    /// point. Only meaningful for a [`Self::rewind_is_server_side`] backend — Codex
    /// forks the thread (`thread/fork`) on the live connection and swaps to the new
    /// thread id; the original thread is left intact. Must be called on the live
    /// connection BEFORE any `cancel_and_wait`. Unsupported by default (Claude uses
    /// the file-fork path instead).
    fn fork_conversation(&self, _user_ordinal: usize, _total_user_messages: usize) -> Result<String> {
        anyhow::bail!("this agent does not support server-side conversation rewind")
    }
}

/// Build the stdin JSON for a user message (stream-json input format).
pub fn user_message_json(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}})
}

/// Build the stdin JSON for a user message that carries images. When `images`
/// is empty this is byte-identical to [`user_message_json`] (a plain string
/// `content`). With images, `content` becomes the Messages-API content-block
/// array — one `text` block (only if non-empty) followed by a `base64` `image`
/// block per attachment. Verified against the live CLI: `claude -p
/// --input-format stream-json` reads and sees such image blocks.
pub fn user_message_json_with_images(text: &str, images: &[ChatImage]) -> Value {
    if images.is_empty() {
        return user_message_json(text);
    }
    let mut content: Vec<Value> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    for img in images {
        content.push(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": img.media_type, "data": img.data},
        }));
    }
    json!({"type": "user", "message": {"role": "user", "content": content}})
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

/// Build the stdin `control_request` JSON that switches the session's permission
/// mode in place (`{subtype:"set_permission_mode", mode}`), the wire the Agent
/// SDK's `setPermissionMode` writes — verified against `@anthropic-ai/
/// claude-agent-sdk`. Fire-and-forget: the CLI acks with a `control_response`
/// (dropped by the decoder), and the new mode applies to subsequent tool calls
/// without a respawn. `mode` is a permission-mode wire string (`default`,
/// `acceptEdits`, `plan`, `bypassPermissions`, …). The `request_id` is minted
/// from a process counter — the reply isn't correlated (nothing to wait on).
pub fn set_permission_mode_json(mode: &str) -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REQ_SEQ: AtomicU64 = AtomicU64::new(1);
    let request_id = format!("oximux-set-mode-{}", REQ_SEQ.fetch_add(1, Ordering::Relaxed));
    json!({"type": "control_request", "request_id": request_id,
           "request": {"subtype": "set_permission_mode", "mode": mode}})
}

/// Build the stdin `control_response` JSON answering an `AskUserQuestion`
/// `can_use_tool` request. Structurally a permission-style `allow`, but the
/// `updatedInput` carries the echoed questions plus the user's `answers` map
/// (keyed by question text) and an optional overall `response` — the shape the
/// tool reads to produce its result (verified against the live CLI).
pub fn question_answer_json(
    request_id: &str,
    questions: &[AskQuestion],
    answers: &QuestionAnswers,
) -> Value {
    json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": {
            "behavior": "allow", "updatedInput": updated_input_json(questions, answers)}}})
}

/// A test double: records everything sent to the "agent" and lets a test inject
/// `ThreadEvent`s (via the returned `Sender`) as if the agent produced them.
/// Used to exercise the app-facing loop (drain events → `ChatThread`; user
/// actions → recorded stdin) without spawning a real subprocess.
#[derive(Clone, Default)]
pub struct StubConnection {
    sent: Arc<Mutex<Vec<Value>>>,
    /// Advertised capabilities; default is the conservative all-false so an
    /// unconfigured stub behaves like the trait default. A test that exercises a
    /// capability-gated affordance (e.g. rewind) sets it via `with_capabilities`.
    caps: AgentCapabilities,
    /// Self-described slash commands, for tests that exercise the palette's
    /// backend-metadata path. Empty by default (like a names-only backend).
    commands: Vec<SlashCommandInfo>,
}

impl StubConnection {
    /// Returns the stub, the event receiver the app would drain, and the
    /// sender a test uses to inject agent events.
    pub fn new() -> (Self, Receiver<ThreadEvent>, Sender<ThreadEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self::default(), rx, tx)
    }

    /// Make the stub advertise `caps` (e.g. a rewind-capable Claude-like backend
    /// for UI tests that gate on `supports_rewind`).
    pub fn with_capabilities(mut self, caps: AgentCapabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Make the stub describe its own slash commands, as a backend with a
    /// command catalog of its own does (pi's `get_commands`).
    pub fn with_slash_commands(mut self, commands: Vec<SlashCommandInfo>) -> Self {
        self.commands = commands;
        self
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
    /// Recorded distinctly from a plain send so a test can tell a message that
    /// went INTO a running turn from one that started a new one. Honours
    /// `supports_steer` so an unconfigured stub still refuses, like the default.
    fn steer(&self, text: &str) -> Result<()> {
        if !self.caps.supports_steer {
            anyhow::bail!("this agent cannot accept a message mid-turn")
        }
        self.record(json!({"type": "steer", "message": text}));
        Ok(())
    }
    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        self.record(control_response_json(request_id, &decision));
        Ok(())
    }
    fn answer_question(
        &self,
        request_id: &str,
        questions: &[AskQuestion],
        answers: &QuestionAnswers,
    ) -> Result<()> {
        self.record(question_answer_json(request_id, questions, answers));
        Ok(())
    }
    fn capabilities(&self) -> AgentCapabilities {
        self.caps
    }
    fn slash_commands(&self) -> Vec<SlashCommandInfo> {
        self.commands.clone()
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
    fn set_permission_mode_json_shape() {
        let v = set_permission_mode_json("acceptEdits");
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request"]["subtype"], "set_permission_mode");
        assert_eq!(v["request"]["mode"], "acceptEdits");
        // A unique request_id is minted (prefix + monotonic counter).
        let id1 = v["request_id"].as_str().unwrap().to_string();
        let id2 = set_permission_mode_json("plan")["request_id"].as_str().unwrap().to_string();
        assert!(id1.starts_with("oximux-set-mode-"));
        assert_ne!(id1, id2, "each request mints a fresh id");
    }

    #[test]
    fn user_message_json_with_no_images_is_plain_string_content() {
        // Empty attachments must be byte-identical to the text-only shape so the
        // common path is unchanged.
        assert_eq!(user_message_json_with_images("hi", &[]), user_message_json("hi"));
    }

    #[test]
    fn user_message_json_with_images_builds_content_blocks() {
        let imgs = vec![ChatImage { media_type: "image/png".into(), data: "QUJD".into() }];
        let v = user_message_json_with_images("look", &imgs);
        let content = &v["message"]["content"];
        assert_eq!(content[0], json!({"type":"text","text":"look"}));
        assert_eq!(
            content[1],
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}})
        );
    }

    #[test]
    fn user_message_json_with_images_omits_empty_text_block() {
        // An image-only prompt (no caption) sends just the image block.
        let imgs = vec![ChatImage { media_type: "image/gif".into(), data: "R0lG".into() }];
        let v = user_message_json_with_images("", &imgs);
        let content = &v["message"]["content"];
        assert_eq!(content.as_array().map(|a| a.len()), Some(1));
        assert_eq!(content[0]["type"], "image");
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
    fn answer_question_json_shape_and_stub_records() {
        use crate::thread::question::{parse_questions, QuestionAnswer, QuestionAnswers};
        let questions = parse_questions(&json!({"questions":[
            {"question":"Tabs or spaces?","header":"Indent",
             "options":[{"label":"Tabs","description":""}],"multiSelect":false}]}));
        let mut answers = QuestionAnswers::default();
        answers.by_question.insert(
            "q-0".into(),
            QuestionAnswer { selected: vec!["Tabs".into()], custom: None },
        );
        let v = question_answer_json("rid-q", &questions, &answers);
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "rid-q");
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        // answers map is keyed by the full question TEXT
        assert_eq!(
            v["response"]["response"]["updatedInput"]["answers"]["Tabs or spaces?"],
            json!("Tabs")
        );
        // the stub records the identical control_response
        let stub = StubConnection::default();
        stub.answer_question("rid-q", &questions, &answers).unwrap();
        assert_eq!(stub.sent()[0], v);
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
        // A backend that only learns its window from a turn's usage reports none
        // here, leaving the meter to be seeded by that usage exactly as before.
        assert_eq!(stub.context_window(), None);
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
            kind: crate::thread::tool_call::PermissionKind::Tool,
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
