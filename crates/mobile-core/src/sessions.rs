//! Session-list subscription over the FFI: the pump that forwards pushed
//! session-list snapshots to the app's [`SessionsSink`], the sink registration, and
//! the per-connection (re)subscribe.
//!
//! The core stays out of session-list *policy* — the host builds the per-device
//! list; the core forwards each pushed snapshot verbatim, so the app replaces its
//! list wholesale rather than folding deltas. This is the push counterpart to the
//! pull [`list_sessions`](MobileClient::list_sessions) RPC, which stays as a manual
//! refresh affordance.

use std::sync::Arc;

use futures::StreamExt;
use oximux_remote_session::{RemoteSession, SessionsStream};

use crate::callbacks::SessionsSink;
use crate::client::{MobileClient, Shared};
use crate::ffi_types::SessionSummary;

/// Forward pushed session-list snapshots to the registered sink until the stream
/// ends (the connection closed). The sink is read per push, not captured once, so
/// the app may register or replace it at any time — including after pushes have
/// started — without a captured `None` swallowing every later snapshot.
pub(crate) async fn run_sessions_pump(shared: Arc<Shared>, mut pushes: SessionsStream) {
    while let Some(rows) = pushes.next().await {
        let sink = shared.sessions_sink.lock().unwrap().clone();
        let Some(sink) = sink else { continue };
        sink.on_sessions(rows.into_iter().map(SessionSummary::from).collect());
    }
}

/// Subscribe to the live session list on a freshly-(re)connected session and hand
/// the initial snapshot to the sink. Called from `activate` on every (re)connect —
/// the host rebuilds its subscriber set per connection, so a redial must
/// re-subscribe to resume pushes. A failed subscribe is skipped: the manual
/// `list_sessions` pull still works, and the next reconnect retries. Issued before
/// the session is published (like the per-session resubscribe), which is why it
/// takes `session` directly rather than reading it back off `shared`.
pub(crate) async fn resubscribe_sessions(shared: &Arc<Shared>, session: &Arc<RemoteSession>) {
    let Ok(rows) = session.subscribe_sessions().await else { return };
    let sink = shared.sessions_sink.lock().unwrap().clone();
    let Some(sink) = sink else { return };
    sink.on_sessions(rows.into_iter().map(SessionSummary::from).collect());
}

#[uniffi::export]
impl MobileClient {
    /// Register the sink that receives the live session list. Replaces any previous
    /// one; register it before `connect`/`reconnect` so the first snapshot arrives
    /// heard.
    pub fn set_sessions_sink(&self, sink: Arc<dyn SessionsSink>) {
        *self.shared.sessions_sink.lock().unwrap() = Some(sink);
    }
}
