//! Per-shell context environment.
//!
//! Every terminal child gets a small set of identity variables so agents
//! and scripts running inside a pane can tell *where* they are: which
//! workspace, which split surface, which terminal tab, and how to reach
//! the daemon socket. These complement `OXIMUX_PTY_ID` (injected at the
//! daemon spawn site) — together they let a tool like `oximux notify`
//! address the exact pane it runs in.
//!
//! The ids are minted by the app at spawn time and threaded through
//! `SpawnConfig.env`. They are persisted alongside the pane layout so a
//! surface keeps the same identity across an app quit -> reattach, and
//! re-injected verbatim when a dormant pane respawns its shell.

use std::sync::OnceLock;

use uuid::Uuid;

/// Daemon socket path, captured once at boot when the relay supervisor
/// comes up. Injected as `OXIMUX_SOCKET_PATH` so external tools can dial
/// the daemon without rediscovering the well-known path. Unset when the
/// relay never started (in-process fallback) — the var is then omitted.
static RELAY_SOCKET_PATH: OnceLock<String> = OnceLock::new();

/// Record the daemon socket path. Called once at boot next to
/// `install_shared_backend`. Idempotent — a second call is ignored.
pub fn set_relay_socket_path(path: impl Into<String>) {
    let _ = RELAY_SOCKET_PATH.set(path.into());
}

/// The recorded daemon socket path, if the relay is up.
pub fn relay_socket_path() -> Option<&'static str> {
    RELAY_SOCKET_PATH.get().map(String::as_str)
}

/// Mint a fresh, stable identity for a split surface (one per leaf).
pub fn new_surface_id() -> String {
    Uuid::new_v4().to_string()
}

/// Mint a fresh, stable identity for a terminal tab (one per terminal).
pub fn new_tab_id() -> String {
    Uuid::new_v4().to_string()
}

/// The stable identity triple a terminal carries. Threaded into the
/// spawn env and stored on the view so a dormant respawn re-injects the
/// exact same ids. `workspace_id` is the project root path; the other
/// two are minted UUIDs persisted alongside the pane layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurfaceIds {
    pub workspace_id: String,
    pub surface_id: String,
    pub tab_id: String,
}

impl SurfaceIds {
    /// Mint fresh surface + tab ids for a new terminal under `workspace_id`.
    pub fn fresh(workspace_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            surface_id: new_surface_id(),
            tab_id: new_tab_id(),
        }
    }

    /// Rebuild ids from a persisted blob. Empty persisted ids (older
    /// snapshots that predate context env) get freshly minted so every
    /// restored terminal still has a stable identity going forward.
    pub fn restored(
        workspace_id: impl Into<String>,
        surface_id: String,
        tab_id: String,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            surface_id: if surface_id.is_empty() {
                new_surface_id()
            } else {
                surface_id
            },
            tab_id: if tab_id.is_empty() {
                new_tab_id()
            } else {
                tab_id
            },
        }
    }

    /// The `OXIMUX_*` env pairs for this triple (+ socket path when set).
    pub fn env(&self) -> Vec<(String, String)> {
        context_env(&self.workspace_id, &self.surface_id, &self.tab_id)
    }
}

/// Build the `OXIMUX_*` context env pairs for a terminal child.
///
/// `workspace_id` is the project root path; `surface_id` names the split
/// pane; `tab_id` names the individual terminal. `OXIMUX_SOCKET_PATH` is
/// appended when the relay is up. `OXIMUX_PTY_ID` is NOT set here — the
/// daemon injects it at the spawn site since only the daemon mints it.
pub fn context_env(workspace_id: &str, surface_id: &str, tab_id: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("OXIMUX_WORKSPACE_ID".to_string(), workspace_id.to_string()),
        ("OXIMUX_SURFACE_ID".to_string(), surface_id.to_string()),
        ("OXIMUX_TAB_ID".to_string(), tab_id.to_string()),
    ];
    if let Some(socket) = relay_socket_path() {
        env.push(("OXIMUX_SOCKET_PATH".to_string(), socket.to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_env_carries_the_three_ids() {
        let env = context_env("/p/root", "surf-1", "tab-1");
        assert!(env.contains(&("OXIMUX_WORKSPACE_ID".into(), "/p/root".into())));
        assert!(env.contains(&("OXIMUX_SURFACE_ID".into(), "surf-1".into())));
        assert!(env.contains(&("OXIMUX_TAB_ID".into(), "tab-1".into())));
    }

    #[test]
    fn socket_path_appended_once_set() {
        // OnceLock is process-global; set it (idempotent) then assert the
        // pair shows up. Other tests in this binary may have set it first,
        // so only assert presence of SOME socket value, not a specific one.
        set_relay_socket_path("/tmp/oximux-test.sock");
        let env = context_env("/w", "s", "t");
        assert!(env.iter().any(|(k, _)| k == "OXIMUX_SOCKET_PATH"));
    }

    #[test]
    fn minted_ids_are_unique() {
        assert_ne!(new_surface_id(), new_surface_id());
        assert_ne!(new_tab_id(), new_tab_id());
    }

    #[test]
    fn restored_preserves_present_ids() {
        let ids = SurfaceIds::restored("/w", "surf-1".into(), "tab-1".into());
        assert_eq!(ids.surface_id, "surf-1");
        assert_eq!(ids.tab_id, "tab-1");
    }

    #[test]
    fn restored_mints_when_empty() {
        // Older blobs carry empty ids; restore must mint non-empty ones so
        // the surface still has a stable identity going forward.
        let ids = SurfaceIds::restored("/w", String::new(), String::new());
        assert!(!ids.surface_id.is_empty());
        assert!(!ids.tab_id.is_empty());
    }
}
