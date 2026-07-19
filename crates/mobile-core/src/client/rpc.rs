//! The async RPC surface: list sessions, drive a turn, resolve permissions. Each
//! grabs the live [`RemoteSession`](oximux_remote_session::RemoteSession) and
//! delegates to its wire method, mapping errors to [`MobileError`].

use std::sync::atomic::{AtomicU64, Ordering};

use oximux_agent_core::thread::PermissionDecision;

use crate::client::MobileClient;
use crate::ffi_types::{MobileError, PermissionReply, SessionSummary};

/// Correlates a queued prompt with its ack; unique per process is enough.
static CORR: AtomicU64 = AtomicU64::new(1);

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// The host's current sessions, with live seq + awaiting-permission flags.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, MobileError> {
        let session = self.shared.session()?;
        let rows = session.list_sessions().await.map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(rows.into_iter().map(SessionSummary::from).collect())
    }

    /// Queue a prompt into a session's turn.
    pub async fn send_prompt(&self, session_id: String, text: String) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        let corr = CORR.fetch_add(1, Ordering::Relaxed);
        session
            .send_prompt(&session_id, &text, &[], corr)
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
            PermissionReply::Deny { message } => PermissionDecision::Deny { message },
        };
        session
            .resolve_permission(&session_id, &request_id, &decision)
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
}
