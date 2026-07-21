//! The async RPC surface: list sessions, drive a turn, resolve permissions. Each
//! grabs the live [`RemoteSession`](oximux_remote_session::RemoteSession) and
//! delegates to its wire method, mapping errors to [`MobileError`].

use std::sync::atomic::{AtomicU64, Ordering};

use oximux_agent_core::thread::{AskQuestion, PermissionDecision, QuestionAnswers};

use crate::client::MobileClient;
use crate::ffi_types::{
    ChatImage, CheckRun, ForgeItem, ForgeItemDetail, ForgeItemKind, ForgeState, MobileError,
    PermissionReply, SessionChoices, SessionSummary,
};

/// Correlates a queued prompt with its ack; unique per process is enough.
static CORR: AtomicU64 = AtomicU64::new(1);

/// Headroom left below the transport's frame cap for everything else the request
/// carries — session id, prompt text, and the postcard envelope. Generous,
/// because the cost of guessing low is a refused attachment the user can retry
/// smaller, while guessing high is a dropped frame they cannot diagnose.
const FRAME_HEADROOM: usize = 1024 * 1024;

/// Refuse a prompt whose attachments cannot fit in one frame.
///
/// The transport rejects an oversize frame as an IO error, which reaches the app
/// as a bare transport failure some distance from the photo that caused it. A
/// phone camera roll makes this reachable without doing anything unusual: base64
/// inflates by ~4/3, so a handful of full-resolution shots clears the cap.
fn check_prompt_size(text: &str, images: &[ChatImage]) -> Result<(), MobileError> {
    let budget = oximux_remote_iroh::MAX_FRAME.saturating_sub(FRAME_HEADROOM);
    let total: usize = images.iter().map(|i| i.data.len() + i.media_type.len()).sum::<usize>()
        + text.len();
    if total > budget {
        let mb = |n: usize| n as f64 / (1024.0 * 1024.0);
        return Err(MobileError::Rpc(format!(
            "attachments total {:.1} MB, over the {:.0} MB a single message can carry \u{2014} send fewer or smaller images",
            mb(total),
            mb(budget),
        )));
    }
    Ok(())
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// The host's current sessions, with live seq + awaiting-permission flags.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, MobileError> {
        let session = self.shared.session()?;
        let rows = session.list_sessions().await.map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(rows.into_iter().map(SessionSummary::from).collect())
    }

    /// Queue a prompt into a session's turn.
    pub async fn send_prompt(
        &self,
        session_id: String,
        text: String,
        images: Vec<ChatImage>,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        check_prompt_size(&text, &images)?;
        let images: Vec<oximux_agent_core::thread::ChatImage> =
            images.into_iter().map(Into::into).collect();
        let corr = CORR.fetch_add(1, Ordering::Relaxed);
        session
            .send_prompt(&session_id, &text, &images, corr)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Answer a pending permission request. Returns `true` if this call decided it,
    /// `false` if it was already decided (idempotent, not an error).
    pub async fn resolve_permission(
        &self,
        session_id: String,
        request_id: String,
        reply: PermissionReply,
    ) -> Result<bool, MobileError> {
        let session = self.shared.session()?;
        let decision = match reply {
            PermissionReply::Allow { updated_input_json } => {
                // Allow MUST echo the tool input; refuse rather than send a
                // malformed empty allow that the CLI would silently treat as a deny.
                let updated_input = serde_json::from_str(&updated_input_json)
                    .map_err(|e| MobileError::Rpc(format!("updated_input is not valid JSON: {e}")))?;
                PermissionDecision::Allow { updated_input }
            }
            PermissionReply::AllowWithSuggestion { updated_input_json, suggestion_json } => {
                let updated_input = serde_json::from_str(&updated_input_json)
                    .map_err(|e| MobileError::Rpc(format!("updated_input is not valid JSON: {e}")))?;
                // Parsed rather than forwarded as a string because the wire type
                // is a typed `PermissionSuggestion`. A malformed one is refused
                // here rather than sent: applying a suggestion changes what the
                // agent is allowed to do for the rest of the session, so a
                // half-understood payload is the wrong thing to guess at.
                let suggestion = serde_json::from_str(&suggestion_json)
                    .map_err(|e| MobileError::Rpc(format!("suggestion is not valid JSON: {e}")))?;
                PermissionDecision::AllowWithSuggestion { updated_input, suggestion }
            }
            PermissionReply::Deny { message } => PermissionDecision::Deny { message },
        };
        session
            .resolve_permission(&session_id, &request_id, &decision)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Answer a pending `AskUserQuestion`. Returns `true` if this call answered it,
    /// `false` if it was already answered (idempotent, not an error).
    ///
    /// Both payloads cross as JSON rather than as generated records: the phone
    /// already holds the questions verbatim inside `ThreadSnapshot.thread_json`,
    /// so it quotes them straight back instead of round-tripping a hand-maintained
    /// record shape that could drift from the fold's.
    ///
    /// A question marked `is_secret` is **refused here**. The desktop redacts a
    /// secret answer by setting `redact_result` on its `ChatThread` at the moment
    /// of answering — the last point the secret-ness is knowable — and the session
    /// registry this path runs through holds no thread to mark. Answering one
    /// remotely would leave the echoed credential in the persisted transcript in
    /// plain text. This is a guard against the accidental case, not a security
    /// boundary: `is_secret` arrives from the client, so it protects an honest
    /// caller, and a paired device could already send prompts and approve tools.
    pub async fn answer_question(
        &self,
        session_id: String,
        request_id: String,
        questions_json: String,
        answers_json: String,
    ) -> Result<bool, MobileError> {
        let session = self.shared.session()?;
        let questions: Vec<AskQuestion> = serde_json::from_str(&questions_json)
            .map_err(|e| MobileError::Rpc(format!("questions are not valid JSON: {e}")))?;
        let answers: QuestionAnswers = serde_json::from_str(&answers_json)
            .map_err(|e| MobileError::Rpc(format!("answers are not valid JSON: {e}")))?;
        if questions.iter().any(|q| q.is_secret) {
            return Err(MobileError::Rpc(
                "this question asks for a secret and can only be answered on the desktop".into(),
            ));
        }
        session
            .answer_question(&session_id, &request_id, &questions, &answers)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Steer an in-flight turn with extra guidance.
    pub async fn steer(&self, session_id: String, text: String) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session.steer(&session_id, &text).await.map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Cancel a session's current turn.
    pub async fn cancel(&self, session_id: String) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session.cancel(&session_id).await.map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Start a new agent session on the desktop, returning its id so the caller
    /// can open it immediately rather than re-listing and guessing which row is
    /// new.
    ///
    /// `cwd` must be a path that exists on the **desktop**, not the phone. A
    /// desktop with no window open, or one that cannot start sessions, refuses.
    pub async fn create_session(
        &self,
        cwd: String,
        agent_id: Option<String>,
    ) -> Result<String, MobileError> {
        let session = self.shared.session()?;
        session
            .create_session(&cwd, agent_id.as_deref())
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Rewind a session to an earlier turn.
    ///
    /// **Destructive**: the turn at `ordinal` and everything after it are
    /// dropped. Returns once the desktop accepts the rewind; the transcript
    /// updates when the resulting `Rewound` event folds through the existing
    /// subscription, so the caller does not apply anything itself.
    ///
    /// `include_files` also restores the working tree to that turn's snapshot,
    /// discarding uncommitted work — including edits the person at the desktop
    /// made outside this session. The desktop may refuse it and still perform
    /// the conversation rewind.
    pub async fn rewind_session(
        &self,
        session_id: String,
        ordinal: u32,
        include_files: bool,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session
            .rewind_session(&session_id, ordinal, include_files)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Issues or pull requests for the session's repository.
    ///
    /// **Empty is a normal answer.** A repo hosted nowhere relevant, a forge CLI
    /// that is absent or signed out on the desktop, or simply no matching items
    /// all come back empty — the desktop cannot tell them apart, so the app must
    /// show "nothing here" rather than guessing at a reason.
    ///
    /// No credential is involved on this device: the desktop runs the CLI that
    /// is already signed in there.
    pub async fn list_forge_items(
        &self,
        session_id: String,
        kind: ForgeItemKind,
        state: ForgeState,
        mine: bool,
    ) -> Result<Vec<ForgeItem>, MobileError> {
        let session = self.shared.session()?;
        let items = session
            .list_forge_items(&session_id, kind.into(), state.into(), mine)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(items.into_iter().map(ForgeItem::from).collect())
    }

    /// Body + author of one issue/PR, fetched when it is opened.
    ///
    /// `None` means the desktop's forge CLI could not supply it — a different
    /// answer from an item whose body is genuinely empty, which arrives as
    /// `Some` with an empty string.
    pub async fn forge_item_detail(
        &self,
        session_id: String,
        kind: ForgeItemKind,
        number: u64,
    ) -> Result<Option<ForgeItemDetail>, MobileError> {
        let session = self.shared.session()?;
        let detail = session
            .forge_item_detail(&session_id, kind.into(), number)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(detail.map(ForgeItemDetail::from))
    }

    /// CI check runs for the current branch's pull request. Empty when there is
    /// no PR, no checks, or the forge reports none.
    pub async fn list_forge_checks(
        &self,
        session_id: String,
    ) -> Result<Vec<CheckRun>, MobileError> {
        let session = self.shared.session()?;
        let checks = session
            .list_forge_checks(&session_id)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(checks.into_iter().map(CheckRun::from).collect())
    }

    /// The models and permission modes this session's backend offers.
    ///
    /// Empty lists mean there is nothing to choose between — a dynamic-catalog
    /// backend before its handshake completes, or an agent with no mode options.
    /// The app hides the picker rather than showing an empty one.
    pub async fn list_choices(&self, session_id: String) -> Result<SessionChoices, MobileError> {
        let session = self.shared.session()?;
        let choices = session
            .list_choices(&session_id)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(choices.into())
    }

    /// Switch the session's model.
    ///
    /// Fails on a backend that fixes its model at spawn (Claude, Codex): the
    /// desktop recovers those by respawning the child, which the remote path
    /// cannot do. The error carries the host's explanation so the app can show
    /// it rather than leaving a control that appears to do nothing.
    pub async fn set_model(&self, session_id: String, model: String) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session.set_model(&session_id, &model).await.map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Switch the session's permission mode. Same fix-at-spawn caveat as
    /// [`Self::set_model`].
    pub async fn set_permission_mode(
        &self,
        session_id: String,
        mode: String,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session
            .set_permission_mode(&session_id, &mode)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }
}
