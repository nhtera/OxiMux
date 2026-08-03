//! The local control listener's lifecycle: accept CLI connections on the
//! owner-only socket, map each authenticated claim onto a dispatcher scope,
//! and take everything down — listener AND in-flight connections — the moment
//! the handle drops.
//!
//! A fresh bearer token is minted at every bind and written (owner-only,
//! readback-verified) by the bind itself, so a token can never outlive the
//! listener that honors it: toggling local access off and on rotates it.

use std::path::PathBuf;
use std::sync::Arc;

use oximux_remote_host::{Dispatcher, LocalScope};
use oximux_remote_local::{LocalClaim, LocalControlListener};

/// The running listener. Dropping it aborts the accept loop, which drops the
/// bound socket (unlinking the node) and the `JoinSet` of per-connection
/// tasks — cutting every live CLI connection, which is what "toggle off"
/// must mean for a revocable surface.
pub struct LocalHandle {
    task: tokio::task::JoinHandle<()>,
    /// Shared with the accept loop so credentials can be minted and dropped
    /// while it runs.
    listener: Arc<LocalControlListener>,
}

impl LocalHandle {
    /// Mint the credential confining one agent process to `session_id`,
    /// returning the secret to inject into that process's environment beside
    /// [`SESSION_ENV_VAR`](oximux_remote_local::SESSION_ENV_VAR).
    ///
    /// The agent spawner calls this; nothing else should. A secret handed to
    /// two processes confines neither.
    pub fn grant_session(&self, session_id: &str) -> String {
        self.listener.grant_session(session_id)
    }

    /// Drop a session's credential when its agent ends.
    pub fn revoke_session(&self, session_id: &str) {
        self.listener.revoke_session(session_id);
    }
}

impl Drop for LocalHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind and serve on `runtime_dir` until the returned handle drops. Binding
/// happens inside the spawned task (the tokio listener needs its runtime);
/// a bind failure is logged and the task ends — the CLI then reports the
/// host unreachable, which is truthful.
/// Binding happens on the caller's thread (it is synchronous file + socket
/// work) so a failure is reported here rather than swallowed by a task, and so
/// the returned handle can hand out session credentials immediately — an agent
/// spawned in the same tick as the toggle must not race the bind.
pub fn start(
    dispatcher: Arc<Dispatcher>,
    runtime_dir: PathBuf,
    rt: tokio::runtime::Handle,
) -> anyhow::Result<LocalHandle> {
    let token = oximux_remote_local::generate_token();
    let listener = {
        // `interprocess`'s tokio listener registers with the reactor at bind.
        let _guard = rt.enter();
        Arc::new(LocalControlListener::bind(&runtime_dir, &token)?)
    };
    let accept_listener = listener.clone();
    let task = rt.spawn(async move {
        let listener = accept_listener;
        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((transport, claim)) => {
                        let dispatcher = dispatcher.clone();
                        // The claim arriving here is already the verdict of a
                        // proof against a registered credential — the handshake
                        // grants the scope of the secret presented, not one the
                        // caller asked for. This maps that verdict onto the
                        // dispatcher's vocabulary and nothing more.
                        let scope = match claim {
                            LocalClaim::Operator => LocalScope::Full,
                            LocalClaim::Session(id) => LocalScope::Session(id),
                        };
                        conns.spawn(async move {
                            dispatcher.serve_local(transport.as_ref(), scope).await;
                        });
                    }
                    // A failed handshake (wrong token, torn connection) ends
                    // that caller only, never the listener.
                    Err(err) => tracing::debug!(%err, "local control connection refused"),
                },
                // Reap finished connections so the set does not grow for the
                // listener's lifetime.
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    });
    Ok(LocalHandle { task, listener })
}
