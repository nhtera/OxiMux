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

/// What a session's model and permission-mode pickers need, cached so they can
/// be rendered for a session with nothing running behind it.
///
/// A backend answers "which models do you offer?" over its live connection, so
/// without this a remote client opening a dormant session would have to make the
/// desktop spawn an agent just to populate two dropdowns — which is exactly the
/// cost that serving history from disk exists to avoid. Cached for the same
/// reason [`PersistedChatTranscript::slash_commands`] is: a resumed session says
/// nothing until the first message, so the last known answer is the only one
/// available until then.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PersistedChoices {
    /// The model catalog this session's backend offered.
    #[serde(default)]
    pub models: Vec<oximux_agents::thread::ModelChoice>,
    /// The permission/edit modes it offered.
    #[serde(default)]
    pub modes: Vec<oximux_agents::thread::ModeChoice>,
    /// The model actually in force — including the backend's own default when the
    /// user never picked one, which [`PersistedChatTranscript::model`]
    /// deliberately omits so a restored tab still launches on that default rather
    /// than pinning it. A picker needs the resolved answer or it renders every
    /// model unselected, as if the session ran on none.
    #[serde(default)]
    pub current_model: Option<String>,
    /// The permission mode in force, resolved the same way and for the same
    /// reason: `None` means the backend's baseline, which no picker can name.
    #[serde(default)]
    pub current_mode: Option<String>,
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
    /// What the session advertised at `system/init` (tools, MCP servers,
    /// subagents, cwd), cached for the same reason `slash_commands` is: a
    /// resumed session sends no init until the first message, so without this
    /// the session-detail popover would be empty on a restored chat.
    /// `#[serde(default)]` keeps blobs written before this field loadable.
    #[serde(default)]
    pub session_meta: oximux_agents::thread::SessionMeta,
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
    /// For an ACP session: the command that was spawned (e.g. `gemini`). The
    /// launch settings key ACP configs by adapter id, which the persisted tab
    /// doesn't carry — so the command rides in the transcript to make restore
    /// self-contained (and faithful to the command that minted the session).
    /// `#[serde(default)]` (→ `None`) keeps older blobs and Claude/Codex blobs
    /// loadable unchanged.
    #[serde(default)]
    pub acp_command: Option<String>,
    /// argv that followed `acp_command` at launch (e.g. `--experimental-acp`).
    /// `#[serde(default)]` (→ empty) for older / non-ACP blobs.
    #[serde(default)]
    pub acp_args: Vec<String>,
    /// The launch-settings key this chat was opened under, and the named
    /// profile within it, so a restore can re-resolve the adapter's configured
    /// environment from **current** settings.
    ///
    /// The env values themselves are deliberately NOT persisted here. They can
    /// hold credentials, and `agent_launch.toml` is already the one plaintext
    /// place they live — copying them into every session blob would multiply
    /// that exposure. Re-resolving also means a corrected base URL takes effect
    /// on the next open instead of being pinned to whatever was set the day the
    /// session started.
    ///
    /// `#[serde(default)]` (→ `None`) keeps every older blob loadable; those
    /// restore with no env, which is exactly their pre-existing behavior.
    #[serde(default)]
    pub adapter_id: Option<String>,
    /// See [`Self::adapter_id`]. `None` = the adapter's plain entry.
    #[serde(default)]
    pub launch_profile: Option<String>,
    /// The Codex posture `(approval_policy, sandbox)` chosen via the composer's
    /// Approvals/Sandbox selects, so a reopened Codex chat resumes under the same
    /// posture. `#[serde(default)]` (→ `None`) keeps older / non-Codex blobs
    /// loadable and defaults them to on-request / workspace-write.
    #[serde(default)]
    pub codex_posture: Option<(String, String)>,
    /// Pi's session-level tool posture chosen via the composer, so a reopened Pi
    /// chat resumes under the same allowlist. `#[serde(default)]` (→ `None`)
    /// keeps older / non-Pi blobs loadable.
    ///
    /// Pi has no per-call approval, so this posture is the only thing standing
    /// between the session and an ungated `bash`. A restore that dropped it
    /// would silently *widen* what the agent may do — hence it is persisted
    /// rather than re-derived.
    #[serde(default)]
    pub pi_posture: Option<oximux_agents::thread::pi::posture::PiPosture>,
    /// omp's approval posture (`--approval-mode`) chosen via the composer, so
    /// a reopened omp chat respawns under the same mode. Its own field, NOT a
    /// reuse of `pi_posture` — the domains differ (a tool allowlist vs an
    /// approval mode). `#[serde(default)]` (→ `None`) keeps older blobs
    /// loadable; a restore then uses OxiMux's `Write` default, never omp's
    /// own `yolo` (the spawn flag is always explicit).
    ///
    /// The stakes are the same as Pi's: a lost posture silently WIDENS what a
    /// restored session may do, so it is persisted rather than re-derived.
    #[serde(default)]
    pub omp_posture: Option<oximux_agents::thread::omp::posture::OmpPosture>,
    /// Claude's fast-mode toggle as last set in the composer, so a reopened
    /// Claude chat respawns with the same `fastMode` overlay. `None` = never
    /// touched (the CLI's own setting applies). `#[serde(default)]` keeps
    /// older blobs loadable.
    #[serde(default)]
    pub claude_fast_mode: Option<bool>,
    /// What this session's pickers offer, so a remote client can render them
    /// without a backend to ask. `#[serde(default)]` (→ empty) keeps blobs
    /// written before this field loadable; their pickers stay hidden until the
    /// desktop next saves the session, which is the same outcome as a backend
    /// that offers no choices.
    #[serde(default)]
    pub choices: PersistedChoices,
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

    /// The fast-mode pick survives the blob, and an older blob without the
    /// field loads as "never touched".
    #[test]
    fn claude_fast_mode_round_trips_and_defaults_to_none() {
        let repo = repo();
        let mut t = PersistedChatTranscript {
            session_id: "sid-fast".into(),
            model: Some("opus[1m]".into()),
            entries: vec![],
            slash_commands: vec![],
            session_meta: Default::default(),
            thinking_level: Default::default(),
            provider: Default::default(),
            acp_command: None,
            acp_args: vec![],
            adapter_id: None,
            launch_profile: None,
            codex_posture: None,
            pi_posture: None,
            omp_posture: None,
            claude_fast_mode: Some(true),
            choices: Default::default(),
        };
        save_chat_transcript(&repo, &t);
        let loaded = load_chat_transcript(&repo, "sid-fast").expect("saved blob loads");
        assert_eq!(loaded.claude_fast_mode, Some(true));
        t.claude_fast_mode = None;
        let json = serde_json::to_string(&t).unwrap();
        let stripped: PersistedChatTranscript = serde_json::from_str(&json).unwrap();
        assert_eq!(stripped.claude_fast_mode, None);
    }

    #[test]
    fn pi_posture_round_trips_so_a_restore_cannot_silently_widen_it() {
        use oximux_agents::thread::pi::posture::{PiPosture, TOOLS_READ_ONLY};
        let repo = repo();
        let t = PersistedChatTranscript {
            session_meta: Default::default(),
            session_id: "pi-sess".into(),
            model: Some("gpt-5.5".into()),
            entries: vec![],
            slash_commands: vec![],
            thinking_level: Default::default(),
            provider: Transport::Rpc,
            acp_command: None,
            acp_args: vec![],
            adapter_id: None,
            launch_profile: None,
            codex_posture: None,
            pi_posture: Some(PiPosture { tools: TOOLS_READ_ONLY.into(), context_files: false }),
            omp_posture: None,
            claude_fast_mode: None,
            choices: Default::default(),
        };
        save_chat_transcript(&repo, &t);
        let loaded = load_chat_transcript(&repo, "pi-sess").expect("blob loads");
        assert_eq!(loaded.provider, Transport::Rpc);
        // Pi has no per-call approval, so this posture is the session's only tool
        // gate — and the default is the MOST permissive option. Losing it here
        // would silently hand a restricted session `bash` back.
        let p = loaded.pi_posture.expect("posture must survive the round trip");
        assert_eq!(p.tools, TOOLS_READ_ONLY);
        assert!(!p.context_files);
        assert_ne!(p, PiPosture::default(), "must not have reverted to the permissive default");
    }

    #[test]
    fn omp_posture_round_trips_so_a_restore_cannot_silently_widen_it() {
        use oximux_agents::thread::omp::posture::OmpPosture;
        let repo = repo();
        let t = PersistedChatTranscript {
            session_meta: Default::default(),
            session_id: "omp-sess".into(),
            model: Some("openai-codex/gpt-5.6-sol".into()),
            entries: vec![],
            slash_commands: vec![],
            thinking_level: Default::default(),
            provider: Transport::OmpRpc,
            acp_command: None,
            acp_args: vec![],
            adapter_id: None,
            launch_profile: None,
            codex_posture: None,
            pi_posture: None,
            omp_posture: Some(OmpPosture::AlwaysAsk),
            claude_fast_mode: None,
            choices: Default::default(),
        };
        save_chat_transcript(&repo, &t);
        let loaded = load_chat_transcript(&repo, "omp-sess").expect("blob loads");
        assert_eq!(loaded.provider, Transport::OmpRpc);
        // The critical direction: a lost posture falls to a MORE permissive
        // mode on respawn (Write — and omp's OWN default is yolo). A user who
        // chose Always-ask must never come back to fewer questions.
        let p = loaded.omp_posture.expect("posture must survive the round trip");
        assert_eq!(p, OmpPosture::AlwaysAsk);
        assert_ne!(p, OmpPosture::default(), "must not have reverted to the wider default");
    }

    #[test]
    fn a_blob_predating_pi_posture_still_loads() {
        // Older blobs have no `pi_posture` key at all; they must not fail to
        // parse (which would drop the whole transcript).
        let repo = repo();
        let raw = r#"{"session_id":"old","entries":[],"provider":"streamjson"}"#;
        repo.set(&chat_settings_key("old"), raw).expect("seed");
        let loaded = load_chat_transcript(&repo, "old").expect("older blob must still load");
        assert_eq!(loaded.pi_posture, None);
        assert_eq!(loaded.provider, Transport::StreamJson);
    }

    #[test]
    fn round_trips_a_transcript() {
        let repo = repo();
        let t = PersistedChatTranscript {
            session_meta: Default::default(),
            session_id: "sid-1".into(),
            model: Some("opus".into()),
            entries: vec![
                ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None },
                ThreadEntry::Assistant(AssistantMessage { text: "hello".into(), thinking: String::new() }),
            ],
            slash_commands: vec!["compact".into(), "research".into()],
            thinking_level: ThinkingLevel::Expanded,
            provider: Transport::StreamJson,
            acp_command: None,
            acp_args: vec![],
            adapter_id: None,
            launch_profile: None,
            codex_posture: None,
            pi_posture: None,
            omp_posture: None,
            claude_fast_mode: None,
            choices: Default::default(),
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
            session_meta: Default::default(),
            session_id: "acp-sess".into(),
            model: None,
            entries: vec![],
            slash_commands: vec![],
            thinking_level: Default::default(),
            provider: Transport::Acp,
            acp_command: Some("gemini".into()),
            acp_args: vec!["--experimental-acp".into()],
            adapter_id: None,
            launch_profile: None,
            codex_posture: None,
            pi_posture: None,
            omp_posture: None,
            claude_fast_mode: None,
            choices: Default::default(),
        };
        save_chat_transcript(&repo, &t);
        let loaded = load_chat_transcript(&repo, "acp-sess").unwrap();
        assert_eq!(loaded.provider, Transport::Acp);
        // The ACP command + args round-trip so restore can respawn the same agent.
        assert_eq!(loaded.acp_command.as_deref(), Some("gemini"));
        assert_eq!(loaded.acp_args, vec!["--experimental-acp".to_string()]);
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
