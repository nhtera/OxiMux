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

/// One message this host had queued and never wrote.
///
/// Text only, with the id the line would have been reported by. Images are
/// not carried: the desktop's own restore drops them too ("text only"), and a
/// blob is not the place for attachment bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BlobQueuedSend {
    pub text: String,
    /// Who queued it, so a restored line keeps the standing it had — a child's
    /// event batch is not a person's message.
    #[serde(default)]
    pub source: String,
    /// The send key it carried, kept so a receipt can later say whether the
    /// agent ever saw it.
    #[serde(default)]
    pub send_key: Option<String>,
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
    /// The error the last turn ended in, when nothing has been sent since.
    ///
    /// The fold keeps no durable trace of a failure: `last_error` is
    /// transient and an errored `TurnEnded` leaves no entry behind. So
    /// without this a host restart turns a session that died failing into
    /// one that reads idle — and a queue the failure was holding drains into
    /// it. The desktop does not write this field and drops it on its own
    /// save, which costs that session the same restored failure it has
    /// today; closing that is the desktop's half of F8.
    #[serde(default)]
    pub last_failure: Option<String>,
    /// A turn, a continuation or background work was running when this host
    /// stopped, so the restart cut it short.
    ///
    /// The desktop's own field, by name and by meaning: a session saved by
    /// one host and reopened by the other must owe the agent the same note.
    #[serde(default)]
    pub mid_turn: bool,
    /// What the background tasks running at that moment were doing, for the
    /// note to name them.
    #[serde(default)]
    pub stopped_tasks: Vec<String>,
    /// A note a restart left owed and nothing has sent yet.
    ///
    /// Kept so a second restart before the next message does not lose it —
    /// and so the note the agent eventually reads names the restart that
    /// actually interrupted it rather than the latest one.
    #[serde(default)]
    pub recovery_note: Option<String>,
    /// What this host had queued and had not written when it stopped.
    ///
    /// Without it the queue simply vanished with the process: a message a
    /// phone sent while a turn ran was accepted, held, and then lost. Restored
    /// **held** — a restart is exactly the moment nobody has reviewed what was
    /// queued, so it waits for a person rather than draining into the agent.
    ///
    /// Serve's own field, like [`ChatBlob::last_failure`]: the desktop keeps
    /// its queue in the tab-layout records and drops this on its own save, so
    /// a session the desktop reopens loses what serve was holding. Closing
    /// that is the desktop's half, as it is for the failure.
    #[serde(default)]
    pub queued: Vec<BlobQueuedSend>,
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
            last_failure: None,
            mid_turn: false,
            stopped_tasks: Vec::new(),
            recovery_note: None,
            queued: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// A one-line label for a session list row: the first user prompt's first
    /// line. The blob stores no title (the desktop keeps titles in its tab
    /// layout), so this is the best durable approximation.
    pub fn derived_title(&self) -> Option<String> {
        derived_title(&self.entries)
    }
}

/// The list-row label rule, over whatever holds the entries.
///
/// A free function rather than a method because the live pump must apply the
/// same rule to the *fold* — and applying it in only one of the two places is
/// exactly the bug this shape prevents. `oximux ls` used to show a live session
/// as its own UUID and the same session as "Summarise what a.txt contains"
/// after a host restart, because the persistence path had the fallback and the
/// registry path did not. Restarting the host improved the listing, which is
/// the wrong way round.
pub fn derived_title(entries: &[ThreadEntry]) -> Option<String> {
    entries.iter().find_map(|entry| match entry {
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

/// Write one blob, reporting whether it landed.
///
/// Failures are logged, never propagated — history persistence must not take
/// the live session down with it. The caller is still told, because a caller
/// that remembers "this is saved" after a write that did not happen will skip
/// the next one too, and the thing it skipped can be a queued message.
#[must_use = "a save that did not land must not be remembered as one that did"]
pub fn save(settings: &SettingsRepo, blob: &ChatBlob) -> bool {
    let json = match serde_json::to_string(blob) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(session_id = %blob.session_id, %err, "chat blob serialize failed");
            return false;
        }
    };
    if let Err(err) = settings.set(&chat_settings_key(&blob.session_id), &json) {
        tracing::warn!(session_id = %blob.session_id, %err, "chat blob save failed");
        return false;
    }
    true
}
