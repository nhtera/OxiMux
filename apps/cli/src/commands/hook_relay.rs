//! The terminal daemon, as the hook verbs reach it.
//!
//! `agent-status` and `notify` name a pane by `OXIMUX_PTY_ID`, and the pane
//! belongs to the relay daemon, so they talk to the relay directly — its own
//! `relay-v*` socket and token in the host's data root, the files the
//! desktop's supervisor writes — never the desktop's control socket beside
//! them. Dialing the control socket with the relay's protocol fails the
//! handshake, and because a hook swallows its errors, a verb that did so would
//! report nothing, silently, forever.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oximux_relay_proto::{Request, Response};

/// Bounded, because the exchange sits in front of the user's next reply on
/// every agent that runs its hooks synchronously, and the relay does not answer
/// `AgentStatus` for a pty it does not know (a pane closed mid-turn). Generous
/// for a local socket round-trip, so a loaded machine still reports.
const DEADLINE: Duration = Duration::from_secs(3);

/// The relay's socket and token under `runtime_dir`, or `None` when no relay
/// has written its token there (no OxiMux running — the common case outside
/// one, and not an error).
pub(crate) fn endpoint(runtime_dir: &Path) -> Option<(PathBuf, String)> {
    let supervisor = oximux_relay_supervisor::RelaySupervisor::new(
        runtime_dir.to_path_buf(),
        // Only consulted when spawning a daemon, which a hook never does.
        runtime_dir.join("logs"),
    );
    let token = std::fs::read_to_string(supervisor.token_path()).ok()?;
    Some((supervisor.socket_path(), token.trim().to_owned()))
}

/// Send `request` to the relay at `socket`, within [`DEADLINE`].
pub(crate) async fn deliver(socket: &Path, token: &str, request: Request) -> Result<Response, String> {
    tokio::time::timeout(DEADLINE, async {
        let client = oximux_relay_client::RelayClient::connect(socket, token)
            .await
            .map_err(|err| format!("connect failed: {err}"))?;
        client.request(request).await.map_err(|err| format!("request failed: {err}"))
    })
    .await
    .unwrap_or_else(|_| Err("the relay did not answer in time".to_owned()))
}

/// Where to look for the relay: the data root of the one that spawned this
/// pane, which it advertises as `OXIMUX_SOCKET_PATH` (a host serving another
/// `--data-dir` included), else this machine's default data root.
pub(crate) fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("OXIMUX_SOCKET_PATH")
        .map(PathBuf::from)
        .and_then(|socket| socket.parent().map(Path::to_path_buf))
        .or_else(oximux_remote_local::default_runtime_dir)
}

/// A current-thread runtime for one exchange, or `None` if one cannot be built.
pub(crate) fn runtime() -> Option<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_verbs_dial_the_relay_not_the_control_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Only the desktop's control-socket token: that is not a relay, and
        // the verbs must not mistake it for one (they once did).
        std::fs::write(oximux_remote_local::token_path(dir.path()), "control").expect("write");
        assert!(endpoint(dir.path()).is_none());

        let supervisor =
            oximux_relay_supervisor::RelaySupervisor::new(dir.path().to_path_buf(), dir.path().join("logs"));
        std::fs::write(supervisor.token_path(), "relay-token\n").expect("write");
        assert_eq!(endpoint(dir.path()), Some((supervisor.socket_path(), "relay-token".to_owned())));
    }
}
