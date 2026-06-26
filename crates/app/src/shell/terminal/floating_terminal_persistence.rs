//! Persisted floating-terminal tab set.
//!
//! One JSON blob per window under `floating_terminal.tabs.<window_id>` in
//! the settings repo. Written (debounced) on every tab mutation, so quit
//! needs no capture pass. Restore happens lazily on the first toggle after
//! launch: tabs whose relay session is still alive in the daemon reattach
//! (scrollback intact); the rest respawn fresh at their persisted cwd.
//!
//! Hardened-restore conventions apply: absent key, garbage JSON, or
//! missing fields all decode to an empty/default blob — never an error,
//! never a crash.

use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

/// Settings key for one window's floating-tab blob.
pub fn tabs_key(window_id: &str) -> String {
    format!("floating_terminal.tabs.{window_id}")
}

/// One persisted floating tab — enough to reattach (external id) or
/// respawn (cwd), plus the user's custom title when set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PersistedFloatingTab {
    /// User-set title; `None` renders the positional default.
    #[serde(default)]
    pub custom_title: Option<String>,
    /// Spawn cwd — the respawn fallback when reattach fails.
    #[serde(default)]
    pub cwd: String,
    /// Relay-side PTY id for cross-restart reattach. `None` when the tab
    /// ran on the in-process fallback backend (dies with the app).
    #[serde(default)]
    pub external_id: Option<String>,
}

/// The whole per-window blob.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FloatingTabsBlob {
    /// Relay daemon session the external ids belong to. Reattach is only
    /// attempted when this matches the live daemon's session id (same
    /// guard the pane restore path uses).
    #[serde(default)]
    pub relay_session: Option<String>,
    /// Active tab index at capture time (clamped on restore).
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub tabs: Vec<PersistedFloatingTab>,
}

impl FloatingTabsBlob {
    /// Load the blob for `key`. Absent repo/key/garbage → default (empty).
    pub fn load(repo: Option<&SettingsRepo>, key: &str) -> Self {
        repo.and_then(|r| r.get(key).ok().flatten())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write the blob. Failures are logged — losing one capture only costs
    /// the next restore, never the live session.
    pub fn save(&self, repo: &SettingsRepo, key: &str) {
        match serde_json::to_string(self) {
            Ok(json) => {
                if let Err(err) = repo.set(key, &json) {
                    tracing::warn!(?err, "failed to persist floating terminal tabs");
                }
            }
            Err(err) => tracing::warn!(?err, "failed to encode floating terminal tabs"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> SettingsRepo {
        SettingsRepo::new(oximux_storage::open_memory().unwrap())
    }

    fn blob() -> FloatingTabsBlob {
        FloatingTabsBlob {
            relay_session: Some("sess-1".into()),
            active: 1,
            tabs: vec![
                PersistedFloatingTab {
                    custom_title: None,
                    cwd: "/w/a".into(),
                    external_id: Some("ext-1".into()),
                },
                PersistedFloatingTab {
                    custom_title: Some("build".into()),
                    cwd: "/w/b".into(),
                    external_id: None,
                },
            ],
        }
    }

    #[test]
    fn tabs_key_is_per_window() {
        assert_eq!(tabs_key("main"), "floating_terminal.tabs.main");
        assert_ne!(tabs_key("main"), tabs_key("w2"));
    }

    #[test]
    fn blob_round_trips_through_repo() {
        let r = repo();
        let b = blob();
        b.save(&r, &tabs_key("main"));
        assert_eq!(FloatingTabsBlob::load(Some(&r), &tabs_key("main")), b);
    }

    #[test]
    fn load_absent_key_is_default() {
        let r = repo();
        assert_eq!(
            FloatingTabsBlob::load(Some(&r), &tabs_key("main")),
            FloatingTabsBlob::default()
        );
    }

    #[test]
    fn load_no_repo_is_default() {
        assert_eq!(
            FloatingTabsBlob::load(None, "x"),
            FloatingTabsBlob::default()
        );
    }

    #[test]
    fn load_garbage_json_is_default_not_error() {
        let r = repo();
        r.set(&tabs_key("main"), "{not json").unwrap();
        assert_eq!(
            FloatingTabsBlob::load(Some(&r), &tabs_key("main")),
            FloatingTabsBlob::default()
        );
    }

    #[test]
    fn legacy_blob_without_new_fields_still_parses() {
        // Absent-tolerance: every field is #[serde(default)], so a minimal
        // (or future-trimmed) blob decodes instead of erroring.
        let b: FloatingTabsBlob = serde_json::from_str(r#"{"tabs":[{"cwd":"/x"}]}"#).unwrap();
        assert_eq!(b.tabs.len(), 1);
        assert_eq!(b.tabs[0].cwd, "/x");
        assert_eq!(b.tabs[0].external_id, None);
        assert_eq!(b.active, 0);
        assert_eq!(b.relay_session, None);
    }

    #[test]
    fn empty_object_parses_to_default() {
        let b: FloatingTabsBlob = serde_json::from_str("{}").unwrap();
        assert_eq!(b, FloatingTabsBlob::default());
    }
}
