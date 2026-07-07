//! Per-session Agent Chat transcript persistence.
//!
//! A restored chat tab needs two things the pane-layout blob can't cheaply
//! carry: the full message history (to paint instantly, before the resumed
//! process says anything) and the `session_id` (to `claude --resume`). Both are
//! stored here as a compact JSON blob under a dedicated settings key, one per
//! chat session — `agent_chat:<session_id>`.
//!
//! Why a separate key rather than embedding in the `terminal_tabs:*` layout
//! blob: that blob has a 64 KiB cap sized for tab topology, and a long
//! conversation would blow it. Keying per session sidesteps the cap and lets a
//! transcript grow independently. The layout blob only stores the tab's
//! `session_id` pointer (in `PersistedTabKind::AgentChat`); this module owns the
//! body. Save/load run from `project_panes_factory`, which already holds the
//! `SettingsRepo` — the pane snapshot carries transcripts in-memory
//! (`PersistedTabs::chat_transcripts`, `#[serde(skip)]`) so the factory never
//! needs the repo threaded any deeper.

use serde::{Deserialize, Serialize};

use oximux_agents::thread::{ThreadEntry, Transport};
use oximux_storage::SettingsRepo;

use crate::shell::agent_chat::ThinkingLevel;

const KEY_PREFIX: &str = "agent_chat:";

/// Settings key for one chat session's transcript. Format:
/// `agent_chat:<session_id>`. Session ids are Claude-minted UUIDs, so no
/// per-project/window scoping is needed — the id is globally unique.
pub fn chat_settings_key(session_id: &str) -> String {
    format!("{KEY_PREFIX}{session_id}")
}

/// A persisted chat transcript: the `session_id` used to `--resume`, the model
/// the session ran under, and the ordered message history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedChatTranscript {
    pub session_id: String,
    #[serde(default)]
    pub model: Option<String>,
    pub entries: Vec<ThreadEntry>,
    /// The command names the session last advertised (from `system/init`), cached
    /// so a restored chat can offer its slash-command palette immediately.
    /// `--resume` stays silent until the first message, so without this cache the
    /// palette would be empty until then. `#[serde(default)]` keeps blobs written
    /// before this field loadable (they restore with an empty list, repopulated
    /// by the next init).
    #[serde(default)]
    pub slash_commands: Vec<String>,
    /// The chat-wide thinking display level. `#[serde(default)]` (→ `Auto`)
    /// keeps blobs written before this field loadable.
    #[serde(default)]
    pub thinking_level: ThinkingLevel,
    /// Which backend transport minted this session, so a restored tab routes
    /// back through the connection factory's matching arm. `#[serde(default)]`
    /// (→ `StreamJson`, i.e. Claude) keeps blobs written before this field
    /// loadable and identical.
    #[serde(default)]
    pub provider: Transport,
}

/// Write one transcript blob. A serialize failure is logged and skipped rather
/// than aborting the surrounding layout save (the tab still restores its shell;
/// only its history is lost).
pub fn save_chat_transcript(repo: &SettingsRepo, transcript: &PersistedChatTranscript) {
    let key = chat_settings_key(&transcript.session_id);
    let json = match serde_json::to_string(transcript) {
        Ok(j) => j,
        Err(err) => {
            tracing::warn!(?err, session_id = %transcript.session_id, "save_chat_transcript: serialize failed");
            return;
        }
    };
    if let Err(err) = repo.set(&key, &json) {
        tracing::warn!(?err, session_id = %transcript.session_id, "save_chat_transcript: settings.set failed");
    }
}

/// Load one transcript blob by session id. `None` when absent or corrupt — a
/// corrupt blob degrades to a fresh (empty) chat rather than failing restore.
pub fn load_chat_transcript(repo: &SettingsRepo, session_id: &str) -> Option<PersistedChatTranscript> {
    let key = chat_settings_key(session_id);
    let raw = match repo.get(&key) {
        Ok(v) => v?,
        Err(err) => {
            tracing::warn!(?err, session_id, "load_chat_transcript: settings.get failed");
            return None;
        }
    };
    match serde_json::from_str::<PersistedChatTranscript>(&raw) {
        Ok(t) => Some(t),
        Err(err) => {
            tracing::warn!(?err, session_id, "load_chat_transcript: parse failed; dropping");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::AssistantMessage;
    use oximux_storage::open_memory;

    fn repo() -> SettingsRepo {
        SettingsRepo::new(open_memory().expect("in-memory db"))
    }

    #[test]
    fn round_trips_a_transcript() {
        let repo = repo();
        let t = PersistedChatTranscript {
            session_id: "sid-1".into(),
            model: Some("opus".into()),
            entries: vec![
                ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None },
                ThreadEntry::Assistant(AssistantMessage { text: "hello".into(), thinking: String::new() }),
            ],
            slash_commands: vec!["compact".into(), "research".into()],
            thinking_level: ThinkingLevel::Expanded,
            provider: Transport::StreamJson,
        };
        save_chat_transcript(&repo, &t);
        assert_eq!(load_chat_transcript(&repo, "sid-1"), Some(t));
    }

    #[test]
    fn old_blob_without_provider_defaults_to_stream_json() {
        // A blob written before the provider field must load as StreamJson (Claude).
        let repo = repo();
        let json = r#"{"session_id":"old","model":null,"entries":[],"slash_commands":[]}"#;
        repo.set(&chat_settings_key("old"), json).expect("seed old blob");
        let loaded = load_chat_transcript(&repo, "old").expect("loads");
        assert_eq!(loaded.provider, Transport::StreamJson);
    }

    #[test]
    fn present_provider_round_trips() {
        // A non-default provider is genuinely carried through save/load (not just
        // filled by the serde default).
        let repo = repo();
        let t = PersistedChatTranscript {
            session_id: "acp-sess".into(),
            model: None,
            entries: vec![],
            slash_commands: vec![],
            thinking_level: Default::default(),
            provider: Transport::Acp,
        };
        save_chat_transcript(&repo, &t);
        assert_eq!(
            load_chat_transcript(&repo, "acp-sess").unwrap().provider,
            Transport::Acp
        );
    }

    #[test]
    fn old_blob_without_thinking_level_defaults_to_auto() {
        // A blob written before the field must load with thinking_level = Auto.
        let repo = repo();
        let json = r#"{"session_id":"old","model":null,"entries":[],"slash_commands":[]}"#;
        repo.set(&chat_settings_key("old"), json).expect("seed old blob");
        let loaded = load_chat_transcript(&repo, "old").expect("loads");
        assert_eq!(loaded.thinking_level, ThinkingLevel::Auto);
    }

    #[test]
    fn missing_and_corrupt_yield_none() {
        let repo = repo();
        assert_eq!(load_chat_transcript(&repo, "nope"), None);
        repo.set(&chat_settings_key("bad"), "{not json").expect("seed corrupt");
        assert_eq!(load_chat_transcript(&repo, "bad"), None);
    }

    #[test]
    fn key_format_is_stable() {
        assert_eq!(chat_settings_key("abc"), "agent_chat:abc");
    }
}
