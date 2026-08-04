//! The persisted chat blob, as the headless host reads and writes it.
//!
//! Serve shares the desktop's own persistence: one JSON blob per session under
//! the settings key `agent_chat:<session_id>`, so a session created on either
//! host is listable, readable, and resumable from the other. This struct
//! mirrors the fields serve needs from the desktop's `PersistedChatTranscript`
//! by serde shape (same names, same types, defaults for absence); everything
//! this build does not model — the desktop's display preferences, fields a
//! future build adds — rides through `extra` untouched, so a rewrite here
//! never strips what the desktop wrote.

use oximux_agent_core::thread::{SessionMeta, ThreadEntry};
use oximux_agents::thread::{ModeChoice, ModelChoice, Transport};
use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

/// Settings key for one chat session's transcript — the desktop's own scheme.
pub fn chat_settings_key(session_id: &str) -> String {
    format!("agent_chat:{session_id}")
}

/// The key prefix the catalog enumerates.
pub const CHAT_KEY_PREFIX: &str = "agent_chat:";

/// Mirror of the desktop's persisted picker cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BlobChoices {
    #[serde(default)]
    pub models: Vec<ModelChoice>,
    #[serde(default)]
    pub modes: Vec<ModeChoice>,
    #[serde(default)]
    pub current_model: Option<String>,
    #[serde(default)]
    pub current_mode: Option<String>,
    /// Fields a newer build knows and this one does not.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One session's persisted transcript + launch facts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatBlob {
    pub session_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub entries: Vec<ThreadEntry>,
    #[serde(default)]
    pub slash_commands: Vec<String>,
    #[serde(default)]
    pub session_meta: SessionMeta,
    #[serde(default)]
    pub provider: Transport,
    #[serde(default)]
    pub acp_command: Option<String>,
    #[serde(default)]
    pub acp_args: Vec<String>,
    #[serde(default)]
    pub codex_posture: Option<(String, String)>,
    #[serde(default)]
    pub pi_posture: Option<oximux_agents::thread::pi::posture::PiPosture>,
    #[serde(default)]
    pub choices: BlobChoices,
    /// Everything else the writer knew — the desktop's `thinking_level`, any
    /// future field — preserved verbatim across a headless rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ChatBlob {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            model: None,
            entries: Vec::new(),
            slash_commands: Vec::new(),
            session_meta: SessionMeta::default(),
            provider: Transport::default(),
            acp_command: None,
            acp_args: Vec::new(),
            codex_posture: None,
            pi_posture: None,
            choices: BlobChoices::default(),
            extra: serde_json::Map::new(),
        }
    }

    /// A one-line label for a session list row: the first user prompt's first
    /// line. The blob stores no title (the desktop keeps titles in its tab
    /// layout), so this is the best durable approximation.
    pub fn derived_title(&self) -> Option<String> {
        self.entries.iter().find_map(|entry| match entry {
            ThreadEntry::User { text, .. } => {
                let line = text.lines().next().unwrap_or("").trim();
                (!line.is_empty()).then(|| {
                    let mut line = line.to_string();
                    if line.chars().count() > 60 {
                        line = line.chars().take(60).collect::<String>() + "…";
                    }
                    line
                })
            }
            _ => None,
        })
    }
}

/// Load one blob. `None` when absent or unreadable — corrupt history degrades
/// to a fresh session rather than failing the caller.
pub fn load(settings: &SettingsRepo, session_id: &str) -> Option<ChatBlob> {
    let raw = settings.get(&chat_settings_key(session_id)).ok()??;
    match serde_json::from_str(&raw) {
        Ok(blob) => Some(blob),
        Err(err) => {
            tracing::warn!(session_id, %err, "unreadable chat blob");
            None
        }
    }
}

/// Write one blob. Failures are logged, not propagated — history persistence
/// must never take the live session down with it.
pub fn save(settings: &SettingsRepo, blob: &ChatBlob) {
    let json = match serde_json::to_string(blob) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(session_id = %blob.session_id, %err, "chat blob serialize failed");
            return;
        }
    };
    if let Err(err) = settings.set(&chat_settings_key(&blob.session_id), &json) {
        tracing::warn!(session_id = %blob.session_id, %err, "chat blob save failed");
    }
}
