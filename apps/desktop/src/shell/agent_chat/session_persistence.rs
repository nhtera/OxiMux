//! Restore-side session lifecycle: dormant boot + transcript save gating.
//!
//! Two halves of the same contract — a restored chat costs nothing until it
//! is looked at, and an unchanged chat costs nothing when it is saved:
//!
//! * **Dormant boot.** `new_resumed` spawns NO subprocess; the view comes up
//!   resumable-idle and [`AgentChatView::ensure_connected`] performs the
//!   deferred connect. A resumed CLI re-reads its entire session file at
//!   startup, so a layout with several chat tabs would otherwise launch that
//!   many concurrent cold starts at boot — the >10s "app won't open" case.
//! * **Dirty-gated save.** The autosave/quit capture serializes every open
//!   chat's transcript; long sessions run to tens of MB of JSON, so
//!   [`AgentChatView::transcript_snapshot_for_save`] returns the body only
//!   when the chat changed since the last committed save. "Changed" is
//!   [`ChatThread::revision`](oximux_agents::thread::ChatThread::revision)
//!   (bumped inside every thread mutator — one chokepoint, not N call
//!   sites) plus a small view-side mark for blob fields the thread doesn't
//!   own. The dirty state clears in [`AgentChatView::commit_transcript_save`]
//!   only after the write succeeded, so a failed write retries next save.

use std::collections::HashMap;

use gpui::Context;
use oximux_agents::thread::pi::posture::{self as pi_posture, PiPosture};
use oximux_agents::thread::FeatureValue;

use super::AgentChatView;
use crate::persisted_chat::{PersistedChatTranscript, PersistedChoices};

/// How a chat view comes to life. Decided once per constructor and mapped
/// onto the state flags in ONE place (`assemble`), so no constructor patches
/// fields up after the fact — a new flavor extends this enum instead of
/// remembering which overrides to apply.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectMode {
    /// Spawn the subprocess immediately (fresh chat / per-agent quick-launch).
    Connect,
    /// Unbound *New Agent* draft: no subprocess until the first send binds.
    UnboundDraft,
    /// Bound restored chat: boots dormant + resumable-idle; connects on first
    /// render or an explicit remote open, via the `--resume` path.
    DormantResume,
    /// Transcript-only import bridge: never spawns, never sends.
    ImportBridge,
}

/// A restored chat's persisted, backend-specific posture. Seeded into both the
/// connection spawn and the composer's feature picks so a reopened session keeps
/// the choice the user made rather than reverting to a default. Empty (all
/// `None`) for a fresh launch.
///
/// One field per backend that has a posture — mirroring `ConnectSpec`'s
/// one-launch-field-per-transport convention. Carried as a struct so adding a
/// backend doesn't grow the arity of the construction chain it threads through.
#[derive(Debug, Clone, Default)]
pub struct RestoredPosture {
    /// Codex `(approval_policy, sandbox)`.
    pub codex: Option<(String, String)>,
    /// Pi's tool allowlist + context-file choice.
    pub pi: Option<PiPosture>,
}

/// Build the initial composer feature-pick overlay for a restored chat, so the
/// reopened session shows (and re-persists) the posture it was saved with.
///
/// For Pi this is load-bearing rather than cosmetic: its posture is the only
/// tool gate that exists, and the default is the most permissive option — so a
/// restore that dropped the picks would silently widen what the agent may do.
pub(super) fn seed_posture_feature_values(
    posture: &RestoredPosture,
) -> HashMap<String, FeatureValue> {
    let mut map = HashMap::new();
    if let Some((approval, sandbox)) = posture.codex.clone() {
        map.insert("codex_approval_policy".to_string(), FeatureValue::Choice(approval));
        map.insert("codex_sandbox".to_string(), FeatureValue::Choice(sandbox));
    }
    if let Some(pi) = posture.pi.clone() {
        map.insert(pi_posture::FEATURE_TOOLS.to_string(), FeatureValue::Choice(pi.tools));
        map.insert(
            pi_posture::FEATURE_CONTEXT_FILES.to_string(),
            FeatureValue::Bool(pi.context_files),
        );
    }
    map
}

impl AgentChatView {
    /// Connect a dormant restored chat: spawn its subprocess via the resume
    /// path and re-seed the composer from the now-live connection. No-op once
    /// connected (the flag clears on the first call) — callers can invoke it
    /// unconditionally. Driven from two kinds of places: the view's own first
    /// render (only the visible tab renders, so hidden restored chats never
    /// spawn) and the explicit remote entry points — the session-catalog open
    /// and a phone-initiated rewind — where no render may ever come.
    ///
    /// `retry_failed` distinguishes the two: an explicit user action (a phone
    /// tapping open/retry) passes `true` and re-attempts a deferred connect
    /// that previously failed to spawn. The render path passes `false` — it
    /// runs every frame, and retrying a permanently failing spawn from there
    /// would fork a doomed process per frame.
    pub fn ensure_connected(&mut self, retry_failed: bool, cx: &mut Context<Self>) {
        // "The deferred connect was tried and its spawn failed" — connection
        // is only `None` while disconnected on the failed-spawn path
        // (`respawn_with_env` takes the old connection before erroring); a
        // crashed live child keeps its stale handle, so this cannot re-spawn
        // a session the desktop's own error card governs.
        let failed_spawn = self.disconnected && self.connection.is_none();
        let should_connect = self.dormant || (retry_failed && failed_spawn);
        if !should_connect {
            return;
        }
        self.dormant = false;
        // An unbound draft or import bridge never sets `dormant`; a chat that
        // failed its spawn stays down on the render path (see above).
        if self.unbound || self.import_bridge.is_some() {
            return;
        }
        if self.disconnected && !(retry_failed && failed_spawn) {
            return;
        }
        self.respawn(cx);
        // Pick up the live connection's capability-gated pickers + vocab —
        // construction seeded the composer with connection-less defaults.
        self.sync_composer(cx);
    }

    /// The session id under which this chat persists, or `None` when there is
    /// nothing persistable (no session id yet, or an empty history). The ONE
    /// guard both snapshot flavors share — keeping it single-sourced is what
    /// guarantees the save path can never emit a pointer whose blob
    /// `transcript_snapshot` would refuse to build.
    fn persistable_session_id(&self) -> Option<String> {
        let session_id = self.thread.session_id.clone()?;
        (!self.thread.entries.is_empty()).then_some(session_id)
    }

    /// Whether the persisted blob is out of date: the thread's own mutation
    /// counter moved past the last saved revision, or a view-held blob field
    /// (model pick, permission mode, thinking level, posture) was touched.
    fn transcript_out_of_date(&self) -> bool {
        self.thread.revision() != self.last_saved_revision.get() || self.meta_dirty.get()
    }

    /// Record that this chat's blob is now up to date on disk. Called by the
    /// save path AFTER its write succeeded (or was skipped as byte-identical)
    /// — never at snapshot time, so a failed write leaves the chat dirty and
    /// the next save retries. Sound to read the live revision here because
    /// the snapshot→serialize→write→mark chain runs synchronously on the
    /// main thread; nothing can mutate the thread in between.
    pub(crate) fn commit_transcript_save(&self, saved_session_ids: &[String]) {
        let Some(sid) = self.thread.session_id.as_deref() else {
            return;
        };
        if saved_session_ids.iter().any(|s| s == sid) {
            self.last_saved_revision.set(self.thread.revision());
            self.meta_dirty.set(false);
        }
    }

    /// The save-path variant of [`Self::transcript_snapshot`]: the session id
    /// pointer always (the layout blob needs it to find the transcript on
    /// restore), the transcript body only when the chat changed since the
    /// last committed save. Serializing every open chat's full history on
    /// every save is what made autosave and quit scale with transcript size —
    /// a clean chat's blob is already on disk, so `None` lets the save skip
    /// it entirely. Pure: dirty state is cleared only by
    /// [`Self::commit_transcript_save`], after the write is known good.
    pub fn transcript_snapshot_for_save(
        &self,
    ) -> Option<(String, Option<PersistedChatTranscript>)> {
        let session_id = self.persistable_session_id()?;
        if !self.transcript_out_of_date() {
            return Some((session_id, None));
        }
        let transcript = self.build_transcript(session_id.clone());
        Some((session_id, Some(transcript)))
    }

    pub fn transcript_snapshot(&self) -> Option<PersistedChatTranscript> {
        let session_id = self.persistable_session_id()?;
        Some(self.build_transcript(session_id))
    }

    /// Assemble the persisted blob for an already-validated session id.
    fn build_transcript(&self, session_id: String) -> PersistedChatTranscript {
        PersistedChatTranscript {
            session_id,
            model: self.thread.model.clone().or_else(|| self.model.clone()),
            entries: self.thread.entries.clone(),
            slash_commands: self.thread.slash_commands.clone(),
            session_meta: self.thread.session_meta.clone(),
            thinking_level: self.thinking_level,
            // The backend that minted this session, so a restored tab reconnects
            // the same provider (Claude stream-json / Codex app-server / an ACP
            // command). The ACP command + args ride along because settings don't
            // retain them per session — the transcript is the source of truth on
            // restore. Empty for Claude/Codex.
            provider: self.backend.transport,
            acp_command: self.backend.acp_command.clone(),
            acp_args: self.backend.acp_args.clone(),
            codex_posture: self.codex_posture_snapshot(),
            pi_posture: self.pi_posture_snapshot(),
            // Read off the live connection while there is one: a remote client
            // opening this session once it is dormant has no backend to ask, and
            // making the desktop spawn one to fill two dropdowns would undo
            // serving its history from disk.
            choices: PersistedChoices {
                models: self.connection.as_ref().map(|c| c.models()).unwrap_or_default(),
                modes: self.connection.as_ref().map(|c| c.permission_modes()).unwrap_or_default(),
                current_model: self.effective_model(),
                current_mode: self.effective_permission_mode(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{
        ChatBackend, SessionMeta, StubConnection, ThreadEntry, ThreadEvent, Transport,
    };
    use oximux_settings::{Density, Theme, Typography};

    use super::super::{AgentChatView, RestoredPosture, ThinkingLevel};

    /// A restored chat must NOT spawn its subprocess at construction — a boot
    /// with many chat tabs would otherwise launch one resumed CLI per tab, and
    /// each re-reads its whole session file. It boots dormant + resumable-idle;
    /// the first RENDER (rendering is the visibility signal) attempts the
    /// connect. The ACP backend here carries an empty command so that attempt
    /// fails synchronously — proving the render ran it — without spawning any
    /// process or worker thread into the test scheduler.
    #[gpui::test]
    async fn restored_chat_boots_dormant_and_connects_on_first_render(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let backend = ChatBackend {
            transport: Transport::Acp,
            acp_command: Some(String::new()),
            acp_args: Vec::new(),
        };
        let window = cx.add_window(|window, cx| {
            let view = AgentChatView::new_resumed(
                std::env::temp_dir(),
                None,
                backend,
                Some("sid-dormant".into()),
                vec![ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None }],
                Vec::new(),
                SessionMeta::default(),
                ThinkingLevel::default(),
                RestoredPosture::default(),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            );
            // Pre-render: no subprocess, bound (not a draft), resumable-idle,
            // and the just-loaded blob is clean (a quit now must not rewrite it).
            assert!(view.connection.is_none(), "construction must not spawn");
            assert!(view.dormant);
            assert!(!view.unbound, "a restored chat is bound, not a draft");
            assert!(view.interrupted, "resumable-idle: a send respawns via --resume");
            assert!(!view.transcript_out_of_date(), "restored blob is the on-disk state");
            view
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, _cx| {
                assert!(!view.dormant, "first render consumed the dormant mark");
                // The empty ACP command refuses synchronously — the render-time
                // connect genuinely ran (and degraded exactly like a failed
                // eager spawn would have).
                assert!(view.disconnected, "failed connect degrades to read-only");
            })
            .expect("window update");
    }

    /// The quit-time capture serializes a transcript body only when the thread
    /// changed since the last take; the session-id pointer comes back every
    /// time (the layout blob needs it to find the transcript on restore).
    #[gpui::test]
    async fn transcript_save_takes_body_only_when_dirty(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                view.thread.session_id = Some("sid-1".into());
                view.thread.push_user_message("hello");
                let (sid, body) =
                    view.transcript_snapshot_for_save().expect("persistable transcript");
                assert_eq!(sid, "sid-1");
                assert!(body.is_some(), "changed thread → body serialized");
                // The snapshot is PURE: until a save commits, the chat stays
                // dirty — a failed write must retry on the next save instead
                // of silently losing the delta.
                let (_, body) =
                    view.transcript_snapshot_for_save().expect("persistable transcript");
                assert!(body.is_some(), "uncommitted take stays dirty");
                // A commit for some OTHER session must not clear this one.
                view.commit_transcript_save(&["other-sid".to_string()]);
                let (_, body) =
                    view.transcript_snapshot_for_save().expect("persistable transcript");
                assert!(body.is_some(), "foreign commit leaves this chat dirty");
                // The save path commits after its write succeeds: pointer
                // only from here — this skip is what keeps autosave/quit from
                // re-serializing megabytes of unchanged history per chat.
                view.commit_transcript_save(&["sid-1".to_string()]);
                let (sid, body) =
                    view.transcript_snapshot_for_save().expect("persistable transcript");
                assert_eq!(sid, "sid-1");
                assert!(body.is_none(), "committed → blob already on disk");
                // Any streamed event re-dirties (the thread's own revision
                // counter — no per-call-site bookkeeping).
                view.on_event(ThreadEvent::AssistantText("Hello!".into()), cx);
                let (_, body) =
                    view.transcript_snapshot_for_save().expect("persistable transcript");
                assert!(body.is_some(), "event re-dirtied the transcript");
            })
            .expect("window update");
    }
}
