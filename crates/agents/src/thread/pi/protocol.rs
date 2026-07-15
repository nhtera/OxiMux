//! Wire types for `pi --mode rpc` — one `\n`-terminated JSON value per message.
//!
//! Three inbound kinds, distinguished by `type`:
//! - **response** (`type: "response"` + `command` + `success`) → routed to the
//!   pending request carrying that `id`;
//! - **extension_ui_request** (`type` + its own `id`) → an elicitation the client
//!   answers with `extension_ui_response`;
//! - everything else → a session **event**, broadcast to the mapper.
//!
//! Events carry no `id` and are never correlated. Commands mirror pi's
//! `RpcCommand` union; only the ones a phase actually drives are modelled —
//! an unmodelled command is a compile error rather than a silently wrong string.
//!
//! Inbound events stay `serde_json::Value` here: the decode to a typed taxonomy
//! belongs to the mapper, and keeping the transport permissive is what makes an
//! unknown event type non-fatal (pi is pre-1.0 and adds events on minor releases).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// pi's thinking levels, weakest → strongest. The order is pi's own
/// (`EXTENDED_THINKING_LEVELS`) and is load-bearing: pi's clamp walks it to find
/// the nearest supported level, so the picker must offer it in the same order.
pub const THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A command written to pi's stdin. `id` correlates the eventual response;
/// pi echoes it back verbatim. Serialized as `{"id":…,"type":…,…}`.
///
/// The two `rename_all`s are not redundant: pi's command *types* are
/// snake_case (`switch_session`) while its *fields* are camelCase
/// (`sessionPath`). Getting the field case wrong is silent — pi ignores the
/// unknown key and the command does nothing — so the serialization is asserted
/// against pi's real field names in this module's tests.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum PiCommand {
    /// Handshake + state read: model, thinking level, session id/file, counts.
    GetState { id: String },
    /// Start a turn.
    Prompt {
        id: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    /// Interrupt the in-flight turn. In-band — pi registers no SIGINT handler,
    /// so a signal is never the right way to stop a turn.
    Abort { id: String },
    /// Redirect the live turn; drains at the next turn boundary.
    Steer { id: String, message: String },
    /// Queue for after the live turn (and after any steering).
    FollowUp { id: String, message: String },
    /// Rehydrate a session from its file. NOTE: succeeds even when the path does
    /// not exist, silently minting a new empty session — the caller must verify
    /// the resulting session id rather than trust `success`.
    SwitchSession { id: String, session_path: String },
    /// The full entry DAG, read from pi's in-memory `fileEntries` (so it is
    /// authoritative even before anything reaches disk).
    ///
    /// `since` MUST be omitted rather than sent as null: pi gates on
    /// `command.since !== undefined`, and JSON `null` passes that check, so an
    /// explicit null makes pi look for an entry whose id is null, find none, and
    /// fail the whole call with "Entry not found: null" — breaking the ordinary
    /// full-transcript read.
    GetEntries {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    },
    GetAvailableModels { id: String },
    GetCommands { id: String },
    SetModel { id: String, provider: String, model_id: String },
    SetThinkingLevel { id: String, level: String },
}

impl PiCommand {
    /// The `id` this command will be answered on.
    pub fn id(&self) -> &str {
        match self {
            PiCommand::GetState { id }
            | PiCommand::Prompt { id, .. }
            | PiCommand::Abort { id }
            | PiCommand::Steer { id, .. }
            | PiCommand::FollowUp { id, .. }
            | PiCommand::SwitchSession { id, .. }
            | PiCommand::GetEntries { id, .. }
            | PiCommand::GetAvailableModels { id }
            | PiCommand::GetCommands { id }
            | PiCommand::SetModel { id, .. }
            | PiCommand::SetThinkingLevel { id, .. } => id,
        }
    }
}

/// How a `prompt` sent mid-turn is queued.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

/// One inbound line, classified.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A correlated answer to a command.
    Response(RpcResponse),
    /// An extension elicitation. Carries its own `id`, answered out-of-band with
    /// `extension_ui_response` — deliberately NOT a `Response` (it is a request
    /// *to* us, not an answer to one of ours).
    ExtensionUiRequest(Value),
    /// A session event (`agent_start`, `message_update`, `tool_execution_*`, …).
    /// Left untyped: the mapper owns the taxonomy.
    Event(Value),
}

/// A correlated response. `success` discriminates: on `false`, `error` carries
/// pi's message (e.g. `"Nothing to compact (session too small)"`).
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl RpcResponse {
    /// The response's payload, or its error as an `Err`.
    pub fn into_data(self) -> anyhow::Result<Value> {
        if self.success {
            Ok(self.data.unwrap_or(Value::Null))
        } else {
            Err(anyhow::anyhow!(
                "pi {} failed: {}",
                self.command,
                self.error.unwrap_or_else(|| "unknown error".into())
            ))
        }
    }
}

/// `get_state`'s payload. Every field pi reports; the ones this round consumes
/// are `model` (picker + context meter), `session_id`/`session_file`
/// (continuity), and `thinking_level`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(default)]
    pub model: Option<Model>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub is_compacting: bool,
    #[serde(default)]
    pub steering_mode: Option<String>,
    #[serde(default)]
    pub follow_up_mode: Option<String>,
    /// pi reports this eagerly — the file may not exist yet. A new session is
    /// held in memory until its first assistant message, so absence of the file
    /// is normal, not corruption.
    #[serde(default)]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub auto_compaction_enabled: bool,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub pending_message_count: u64,
}

/// One model from `get_state` / `get_available_models`. `context_window` is
/// present on every model — the context meter can be seeded at connect, before
/// the first turn settles.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Per-model thinking support. A **present but null** value means the level
    /// is unsupported; an absent key means "no remapping needed". The
    /// distinction is the whole point of the nested `Option`, so this must not
    /// be flattened to `HashMap<String, String>` — see
    /// [`Self::supported_thinking_levels`].
    #[serde(default)]
    pub thinking_level_map: Option<HashMap<String, Option<String>>>,
    /// The content kinds this model accepts (`text`, `image`).
    #[serde(default)]
    pub input: Vec<String>,
}

impl Model {
    /// The label the picker shows — pi's display name, else the wire id.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// `provider/id` — the form every model reference MUST take.
    ///
    /// A bare id is not a model reference in pi, it is a *search pattern*: pi
    /// tries an exact `provider/id` match first and otherwise falls back to a
    /// fuzzy `includes()` scan across **every** provider it knows, then picks a
    /// winner by sorting (`model-resolver.ts:125-155`). Verified live —
    /// `pi --mode rpc --model gpt-5.5` resolves to
    /// `azure-openai-responses/gpt-5.5` (1.05M context, empty baseUrl, no auth),
    /// not the `openai-codex/gpt-5.5` the picker meant (272K). The fallback scans
    /// an unfiltered registry, so it reaches providers the catalog itself omits.
    ///
    /// Nothing about that is an error: the wrong model just silently loads, with
    /// the wrong context window under the meter. Always qualify.
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Whether this model accepts image input. Pi's `prompt` command does carry
    /// an `images` field, but OxiMux does not send one yet, so today this only
    /// describes the model rather than gating a live affordance.
    pub fn accepts_images(&self) -> bool {
        self.input.iter().any(|i| i == "image")
    }

    /// The thinking levels this model actually supports.
    ///
    /// A faithful port of pi's `getSupportedThinkingLevels` (`ai/src/models.ts`),
    /// and it must stay faithful: `set_thinking_level` **silently clamps** an
    /// unsupported level and still answers `success: true`. Verified live —
    /// asking gpt-5.5 (whose map has no `max`) for `max` yields `xhigh`, and an
    /// unknown level yields `off`; in neither case does pi report a problem.
    /// Worse, `thinking_level_changed` only fires when the effective level
    /// *changes*, so a clamp onto the level pi is already at is announced by
    /// nothing at all. Offering a level this returns is therefore not a cosmetic
    /// bug — the UI would claim a reasoning depth the model never used, with no
    /// way to find out.
    pub fn supported_thinking_levels(&self) -> Vec<&'static str> {
        if !self.reasoning {
            return vec!["off"];
        }
        THINKING_LEVELS
            .iter()
            .copied()
            .filter(|level| match self.thinking_level_map.as_ref().and_then(|m| m.get(*level)) {
                // Present and null: explicitly unsupported.
                Some(None) => false,
                // Present with a mapping: supported (possibly remapped onto
                // another level — `minimal: "low"` means "ask for minimal, get low").
                Some(Some(_)) => true,
                // Absent: the top two levels are strictly opt-in; the rest are
                // assumed supported by any reasoning model.
                None => !matches!(*level, "xhigh" | "max"),
            })
            .collect()
    }
}

/// `get_available_models`' payload. The list is **wrapped** in a `models` key
/// rather than being a bare array, and it is pre-filtered to providers the user
/// has auth for — unlike the registry `--model` resolution searches.
#[derive(Debug, Clone, Deserialize)]
pub struct AvailableModels {
    #[serde(default)]
    pub models: Vec<Model>,
}

/// One command from `get_commands`, invocable by typing `/<name>`.
///
/// Richer than the bare names Claude advertises: pi gives a `description` and
/// says where the command came from, so the palette can group and attribute rows
/// without OxiMux scanning any config directory itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    /// Without the leading slash. Skills arrive pre-namespaced (`skill:foo`) —
    /// that prefix is pi's own (`rpc-mode.ts:534`), not a convention imposed here.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `extension` | `prompt` | `skill`.
    ///
    /// `prompt` and `skill` both expand into text and start an ordinary turn
    /// (verified live). `extension` does **not**: pi runs the extension's handler
    /// inline and returns without prompting the model
    /// (`agent-session.ts:783-790`), so no `agent_start`/`agent_settled` bracket
    /// the send, and a UI that flips a turn on at submit never sees it flip back
    /// — the user would have to press Stop. Not handled here because it could not
    /// be probed: provoking one needs an installed extension that registers a
    /// command, and none was available. Nor is it a reason to hide such rows —
    /// typing the command by hand does the same thing, so hiding them would cost
    /// discovery and prevent nothing.
    pub source: String,
    #[serde(default)]
    pub source_info: Option<SourceInfo>,
}

impl SlashCommand {
    /// Whether this is a skill-catalog entry rather than a plain command — the
    /// palette groups the two apart.
    pub fn is_skill(&self) -> bool {
        self.source == "skill"
    }
}

/// Where a command came from. Only `scope` is consumed (as the palette's
/// attribution tag); the rest is kept so the shape matches pi's and an added
/// field doesn't read as a decode failure.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// `user` (`~/.agents/skills/…`) or `project` (`<cwd>/.agents/skills/…`).
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub base_dir: Option<String>,
}

/// `get_commands`' payload — **wrapped** in a `commands` key, like
/// [`AvailableModels`], rather than being a bare array.
#[derive(Debug, Clone, Deserialize)]
pub struct AvailableCommands {
    #[serde(default)]
    pub commands: Vec<SlashCommand>,
}

/// Classify one decoded inbound line.
pub fn classify(v: Value) -> Inbound {
    match v.get("type").and_then(Value::as_str) {
        Some("response") => match serde_json::from_value::<RpcResponse>(v.clone()) {
            Ok(r) => Inbound::Response(r),
            // A response we can't parse is more useful as an event than as a
            // dropped line — the mapper logs it rather than the transport
            // silently swallowing a protocol change.
            Err(_) => Inbound::Event(v),
        },
        Some("extension_ui_request") => Inbound::ExtensionUiRequest(v),
        _ => Inbound::Event(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_serialize_with_a_type_tag() {
        let s = serde_json::to_string(&PiCommand::GetState { id: "s1".into() }).unwrap();
        assert_eq!(s, r#"{"type":"get_state","id":"s1"}"#);
        let s = serde_json::to_string(&PiCommand::Prompt {
            id: "p1".into(),
            message: "hi".into(),
            streaming_behavior: None,
        })
        .unwrap();
        // `streaming_behavior` is omitted when None — pi's default applies.
        assert_eq!(s, r#"{"type":"prompt","id":"p1","message":"hi"}"#);
        let s = serde_json::to_string(&PiCommand::Prompt {
            id: "p2".into(),
            message: "hi".into(),
            streaming_behavior: Some(StreamingBehavior::FollowUp),
        })
        .unwrap();
        assert!(s.contains(r#""streamingBehavior":"followUp""#), "got {s}");
    }

    #[test]
    fn get_entries_omits_since_rather_than_sending_null() {
        // Load-bearing: pi gates on `command.since !== undefined`, and JSON null
        // PASSES that check (null !== undefined). An explicit null would make pi
        // hunt for an entry whose id is null, find none, and fail the entire
        // call with "Entry not found: null" — breaking the ordinary
        // full-transcript read, which is the common case.
        let s = serde_json::to_string(&PiCommand::GetEntries { id: "e1".into(), since: None })
            .unwrap();
        assert_eq!(s, r#"{"type":"get_entries","id":"e1"}"#);
        assert!(!s.contains("since"), "a null `since` breaks get_entries entirely: {s}");
        // A real `since` still rides.
        let s = serde_json::to_string(&PiCommand::GetEntries {
            id: "e2".into(),
            since: Some("abc123".into()),
        })
        .unwrap();
        assert!(s.contains(r#""since":"abc123""#), "got {s}");
    }

    #[test]
    fn switch_session_uses_pis_camel_case_field() {
        let s = serde_json::to_string(&PiCommand::SwitchSession {
            id: "sw".into(),
            session_path: "/tmp/x.jsonl".into(),
        })
        .unwrap();
        assert!(s.contains(r#""sessionPath":"/tmp/x.jsonl""#), "got {s}");
    }

    #[test]
    fn responses_route_and_errors_surface_pis_message() {
        let v: Value = serde_json::from_str(
            r#"{"id":"c1","type":"response","command":"compact","success":false,"error":"Nothing to compact (session too small)"}"#,
        )
        .unwrap();
        let Inbound::Response(r) = classify(v) else { panic!("expected a response") };
        assert_eq!(r.id.as_deref(), Some("c1"));
        let err = r.into_data().expect_err("a failed response must be an Err");
        assert!(err.to_string().contains("session too small"), "got {err}");
    }

    #[test]
    fn events_and_elicitations_are_not_responses() {
        let ev: Value = serde_json::from_str(r#"{"type":"agent_settled"}"#).unwrap();
        assert!(matches!(classify(ev), Inbound::Event(_)));
        let ui: Value =
            serde_json::from_str(r#"{"type":"extension_ui_request","id":"u1","method":"confirm"}"#)
                .unwrap();
        assert!(matches!(classify(ui), Inbound::ExtensionUiRequest(_)));
    }

    #[test]
    fn an_unknown_event_type_is_an_event_not_an_error() {
        // pi is pre-1.0 and ships new events on minor releases; an unknown line
        // must flow through, never abort the stream.
        let v: Value = serde_json::from_str(r#"{"type":"some_future_event","x":1}"#).unwrap();
        assert!(matches!(classify(v), Inbound::Event(_)));
    }

    #[test]
    fn session_state_decodes_the_captured_handshake() {
        // Trimmed from real probe bytes (pi 0.80.6).
        let v = serde_json::json!({
            "model": {
                "id": "gpt-5.5", "name": "GPT-5.5", "api": "openai-codex-responses",
                "provider": "openai-codex", "reasoning": true,
                "contextWindow": 272_000, "maxTokens": 128_000
            },
            "thinkingLevel": "medium", "isStreaming": false, "isCompacting": false,
            "steeringMode": "one-at-a-time", "followUpMode": "one-at-a-time",
            "sessionFile": "/tmp/s/2026-07-15_019f650c.jsonl",
            "sessionId": "019f650c-a70a-77c4-8fa4-f81e6e6ad1f3",
            "autoCompactionEnabled": true, "messageCount": 0, "pendingMessageCount": 0
        });
        let st: SessionState = serde_json::from_value(v).expect("decode get_state");
        assert_eq!(st.session_id, "019f650c-a70a-77c4-8fa4-f81e6e6ad1f3");
        let m = st.model.expect("model present");
        assert_eq!(m.display_name(), "GPT-5.5");
        assert_eq!(m.context_window, Some(272_000), "context meter seeds at connect");
    }

    #[test]
    fn a_model_reference_is_always_provider_qualified() {
        // A bare id is a fuzzy search pattern in pi, not a reference: live,
        // `--model gpt-5.5` loads azure-openai-responses/gpt-5.5 (1.05M context)
        // instead of the openai-codex/gpt-5.5 (272K) the picker meant.
        let m = Model {
            id: "gpt-5.5".into(),
            name: Some("GPT-5.5".into()),
            provider: "openai-codex".into(),
            api: None,
            reasoning: true,
            context_window: Some(272_000),
            max_tokens: None,
            thinking_level_map: None,
            input: vec![],
        };
        assert_eq!(m.qualified(), "openai-codex/gpt-5.5");
        // pi splits a reference on its FIRST slash and compares the whole
        // `provider/id` (`model-resolver.ts:77-99`), so an id that itself
        // contains a slash round-trips through the same split.
        let nested = Model { id: "meta/llama-3".into(), provider: "openrouter".into(), ..m.clone() };
        let q = nested.qualified();
        assert_eq!(q, "openrouter/meta/llama-3");
        assert_eq!(q.split_once('/'), Some(("openrouter", "meta/llama-3")));
    }

    #[test]
    fn available_models_is_wrapped_in_a_models_key() {
        // Real bytes: `{"models":[…]}`, not a bare array.
        let v = serde_json::json!({"models":[
            {"id":"gpt-5.5","name":"GPT-5.5","provider":"openai-codex","reasoning":true,
             "thinkingLevelMap":{"xhigh":"xhigh","minimal":"low"},
             "input":["text","image"],"contextWindow":272_000,"maxTokens":128_000}
        ]});
        let a: AvailableModels = serde_json::from_value(v).expect("decode catalog");
        assert_eq!(a.models.len(), 1);
        assert_eq!(a.models[0].qualified(), "openai-codex/gpt-5.5");
        assert!(a.models[0].accepts_images());
    }

    #[test]
    fn available_commands_is_wrapped_and_keeps_pis_own_namespacing() {
        // Real bytes (pi 0.80.6, `get_commands` against this repo): wrapped in a
        // `commands` key, skills already named `skill:<name>` by pi itself, and
        // `scope` distinguishing a repo-local skill from a user-global one.
        let v = serde_json::json!({"commands":[
            {"name":"skill:gpui-action",
             "description":"Action definitions and keyboard shortcuts in GPUI.",
             "source":"skill",
             "sourceInfo":{"path":"/repo/.agents/skills/gpui-action/SKILL.md","source":"auto",
                           "scope":"project","origin":"top-level","baseDir":"/repo/.agents"}},
            {"name":"review","description":"Review a diff","source":"prompt",
             "sourceInfo":{"path":"/home/u/.pi/prompts/review.md","scope":"user"}},
            {"name":"deploy","source":"extension","sourceInfo":{"scope":"user"}}
        ]});
        let a: AvailableCommands = serde_json::from_value(v).expect("decode commands");
        assert_eq!(a.commands.len(), 3);
        assert_eq!(a.commands[0].name, "skill:gpui-action", "pi namespaces skills, not us");
        assert!(a.commands[0].is_skill());
        assert_eq!(
            a.commands[0].source_info.as_ref().and_then(|s| s.scope.as_deref()),
            Some("project")
        );
        // A prompt template is a command, not a skill.
        assert!(!a.commands[1].is_skill());
        // `description` is optional on the wire — an absent one must not fail the
        // whole catalog (it would take the palette down with it).
        assert_eq!(a.commands[2].description, None);
        assert!(!a.commands[2].is_skill());
    }

    #[test]
    fn steering_a_message_uses_pis_own_command() {
        let s = serde_json::to_string(&PiCommand::Steer {
            id: "st1".into(),
            message: "actually, stop".into(),
        })
        .unwrap();
        assert_eq!(s, r#"{"type":"steer","id":"st1","message":"actually, stop"}"#);
    }

    #[test]
    fn thinking_levels_follow_pis_own_support_rules() {
        let model = |reasoning: bool, map: Option<Value>| Model {
            id: "m".into(),
            name: None,
            provider: "p".into(),
            api: None,
            reasoning,
            context_window: None,
            max_tokens: None,
            thinking_level_map: map.map(|v| serde_json::from_value(v).expect("map")),
            input: vec![],
        };

        // Real gpt-5.5 (captured): xhigh is opted in, max is NOT — so offering
        // `max` would silently get `xhigh` with a success response.
        let gpt55 = model(true, Some(serde_json::json!({"xhigh": "xhigh", "minimal": "low"})));
        assert_eq!(
            gpt55.supported_thinking_levels(),
            vec!["off", "minimal", "low", "medium", "high", "xhigh"],
            "max must not be offered: gpt-5.5's map has no `max` key"
        );

        // Real gpt-5.6-luna (captured): opts into both.
        let luna = model(true, Some(serde_json::json!({"xhigh":"xhigh","max":"max","minimal":"low"})));
        assert!(luna.supported_thinking_levels().contains(&"max"));

        // Real azure gpt-5.5 (captured): `off` is present-and-NULL → it cannot be
        // turned off. An absent key and a null key must not be conflated.
        let azure = model(true, Some(serde_json::json!({"off": null, "xhigh": "xhigh"})));
        let levels = azure.supported_thinking_levels();
        assert!(!levels.contains(&"off"), "a null mapping means unsupported, got {levels:?}");
        assert!(levels.contains(&"xhigh"));
        assert!(!levels.contains(&"max"));

        // A reasoning model with no map at all: everything but the opt-in top two.
        assert_eq!(
            model(true, None).supported_thinking_levels(),
            vec!["off", "minimal", "low", "medium", "high"]
        );

        // A non-reasoning model can only be off, whatever its map says.
        assert_eq!(
            model(false, Some(serde_json::json!({"max": "max"}))).supported_thinking_levels(),
            vec!["off"]
        );
    }

    #[test]
    fn a_session_state_without_a_file_still_decodes() {
        // A brand-new session has no file on disk yet; `sessionFile` may be absent.
        let st: SessionState =
            serde_json::from_value(serde_json::json!({"sessionId": "abc"})).expect("decode");
        assert_eq!(st.session_file, None);
        assert!(st.model.is_none());
    }
}
