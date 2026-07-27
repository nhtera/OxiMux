//! Screen-control settings, loaded from `computer_use.toml` in the app data dir
//! and held as a GPUI [`Global`] so the settings pane and the agent spawn path
//! read one source of truth.
//!
//! Mirrors the `dictation` settings contract: `from_toml_str` / `to_toml_string`
//! / `sanitized`, a `FILE_NAME`, and a live-reload watcher in the app crate.
//!
//! # Two switches, both off by default
//!
//! [`enabled`](ComputerUseSettings::enabled) is the master switch; `projects`
//! then names the project roots that actually get it. Both must say yes, so
//! flipping the master does not hand screen control to every agent in every
//! project at once — an agent driving the GUI is not something to opt into by
//! side effect.
//!
//! # Why this is not a `.oximux/` file
//!
//! Every other per-project setting in this crate lives in a git-committable
//! `.oximux/*.toml` so a team can share it. That precedent is exactly wrong
//! here: a repository could then ship `enabled = true` and cloning it would
//! grant screen control before the user had seen a single line of its code.
//! The opt-in is keyed by path but stored in the *user's* data dir, where a
//! checkout cannot reach it.

use std::path::{Path, PathBuf};

use gpui::Global;
use serde::{Deserialize, Serialize};

/// An app the user has pre-approved, so driving it raises no card.
///
/// Keyed on **bundle id** rather than path: a freshly rebuilt app lands at a new
/// path (or the same path with new bytes) many times an hour, and a grant that
/// evaporated on every rebuild would train the user to click through the card
/// without reading it.
///
/// This is deliberately *not* where PID pinning lives. A pid is meaningful for
/// the length of one process; persisting one would either grant a recycled pid
/// or grant nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppGrant {
    /// `CFBundleIdentifier`, e.g. `com.apple.Safari`.
    pub bundle_id: String,
    /// Display name for the settings row. Cosmetic — never matched on.
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerUseSettings {
    /// Master switch. `false` means no agent is ever declared the screen-control
    /// server, whatever else is configured.
    pub enabled: bool,
    /// Project roots opted in, absolute. Empty means "nowhere yet" — which is
    /// what a freshly flipped master switch should mean.
    pub projects: Vec<PathBuf>,
    /// Apps that skip the consent card. Everything else asks.
    pub allowed_apps: Vec<AppGrant>,
}

impl Default for ComputerUseSettings {
    fn default() -> Self {
        // Off, everywhere, with nothing pre-approved. The one default a feature
        // that can click on the user's behalf is allowed to have.
        Self {
            enabled: false,
            projects: Vec::new(),
            allowed_apps: Vec::new(),
        }
    }
}

impl Global for ComputerUseSettings {}

impl ComputerUseSettings {
    pub const FILE_NAME: &'static str = "computer_use.toml";

    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// May agents rooted at `project` drive the screen? Both switches must
    /// agree, which is the whole point of having two.
    pub fn is_enabled_for(&self, project: &Path) -> bool {
        self.enabled && self.projects.iter().any(|p| p == project)
    }

    /// Opt `project` in. Idempotent, so a double-click on the toggle cannot
    /// leave two entries behind.
    pub fn enable_project(&mut self, project: &Path) {
        if !self.projects.iter().any(|p| p == project) {
            self.projects.push(project.to_path_buf());
        }
    }

    /// Opt `project` back out. Returns whether anything changed.
    pub fn disable_project(&mut self, project: &Path) -> bool {
        let before = self.projects.len();
        self.projects.retain(|p| p != project);
        self.projects.len() != before
    }

    /// Is `bundle_id` pre-approved?
    ///
    /// Case-insensitive, matching how the system compares bundle ids and how
    /// [`crate`]-external blocklists spell them — `com.lastpass.LastPass` and
    /// `com.lastpass.lastpass` are the same app, and a case-sensitive compare
    /// here would silently disagree with the runtime gate.
    pub fn is_allowed(&self, bundle_id: &str) -> bool {
        self.allowed_apps
            .iter()
            .any(|grant| grant.bundle_id.eq_ignore_ascii_case(bundle_id))
    }

    /// Pre-approve an app. Idempotent on bundle id; a repeat call refreshes the
    /// display name rather than adding a second row.
    pub fn allow(&mut self, bundle_id: &str, name: &str) {
        let bundle_id = bundle_id.trim();
        if bundle_id.is_empty() {
            return;
        }
        if let Some(existing) = self
            .allowed_apps
            .iter_mut()
            .find(|grant| grant.bundle_id.eq_ignore_ascii_case(bundle_id))
        {
            existing.name = name.trim().to_string();
            return;
        }
        self.allowed_apps.push(AppGrant {
            bundle_id: bundle_id.to_string(),
            name: name.trim().to_string(),
        });
    }

    /// Withdraw a pre-approval. Returns whether anything changed.
    pub fn revoke(&mut self, bundle_id: &str) -> bool {
        let before = self.allowed_apps.len();
        self.allowed_apps
            .retain(|grant| !grant.bundle_id.eq_ignore_ascii_case(bundle_id));
        self.allowed_apps.len() != before
    }

    /// Trim + normalize hand-edited values: drop blank and duplicate entries,
    /// and drop relative project paths.
    ///
    /// A relative path cannot be compared against the absolute worktree root the
    /// spawn path holds, so it would sit in the file looking enabled while
    /// matching nothing. Dropping it is the honest reading.
    pub fn sanitized(mut self) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.allowed_apps = std::mem::take(&mut self.allowed_apps)
            .into_iter()
            .map(|grant| AppGrant {
                bundle_id: grant.bundle_id.trim().to_string(),
                name: grant.name.trim().to_string(),
            })
            .filter(|grant| {
                !grant.bundle_id.is_empty() && seen.insert(grant.bundle_id.to_lowercase())
            })
            .collect();

        let mut seen_projects = std::collections::HashSet::new();
        self.projects = std::mem::take(&mut self.projects)
            .into_iter()
            .filter(|p| p.is_absolute() && seen_projects.insert(p.clone()))
            .collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_everywhere_with_nothing_approved() {
        let s = ComputerUseSettings::default();
        assert!(!s.enabled);
        assert!(s.projects.is_empty());
        assert!(s.allowed_apps.is_empty());
        assert!(!s.is_enabled_for(Path::new("/repo")));
    }

    #[test]
    fn the_master_switch_alone_enables_nothing() {
        // The reason there are two switches: flipping the master must not hand
        // screen control to every project at once.
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        assert!(!s.is_enabled_for(Path::new("/repo")));
        s.enable_project(Path::new("/repo"));
        assert!(s.is_enabled_for(Path::new("/repo")));
        assert!(!s.is_enabled_for(Path::new("/other")));
    }

    #[test]
    fn a_project_opt_in_alone_enables_nothing_either() {
        let mut s = ComputerUseSettings::default();
        s.enable_project(Path::new("/repo"));
        assert!(!s.is_enabled_for(Path::new("/repo")));
    }

    #[test]
    fn project_opt_in_is_idempotent_and_reversible() {
        let mut s = ComputerUseSettings::default();
        s.enable_project(Path::new("/repo"));
        s.enable_project(Path::new("/repo"));
        assert_eq!(s.projects.len(), 1);
        assert!(s.disable_project(Path::new("/repo")));
        assert!(!s.disable_project(Path::new("/repo")));
        assert!(s.projects.is_empty());
    }

    #[test]
    fn allowlist_matching_ignores_case() {
        // Must agree with the runtime blocklist, which compares the same way.
        let mut s = ComputerUseSettings::default();
        s.allow("com.apple.Safari", "Safari");
        assert!(s.is_allowed("com.apple.safari"));
        assert!(s.is_allowed("COM.APPLE.SAFARI"));
        assert!(!s.is_allowed("com.apple.Safari.helper"));
    }

    #[test]
    fn re_allowing_refreshes_the_name_rather_than_duplicating() {
        let mut s = ComputerUseSettings::default();
        s.allow("com.apple.Safari", "Safari");
        s.allow("com.apple.safari", "Safari Technology Preview");
        assert_eq!(s.allowed_apps.len(), 1);
        assert_eq!(s.allowed_apps[0].name, "Safari Technology Preview");
    }

    #[test]
    fn revoking_removes_the_grant_and_reports_the_change() {
        let mut s = ComputerUseSettings::default();
        s.allow("com.apple.Safari", "Safari");
        assert!(s.revoke("COM.APPLE.SAFARI"));
        assert!(!s.revoke("com.apple.Safari"));
        assert!(!s.is_allowed("com.apple.Safari"));
    }

    #[test]
    fn a_blank_bundle_id_is_not_a_grant() {
        let mut s = ComputerUseSettings::default();
        s.allow("   ", "Nothing");
        assert!(s.allowed_apps.is_empty());
    }

    #[test]
    fn round_trips_through_toml() {
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        s.enable_project(Path::new("/Users/x/repo"));
        s.allow("com.apple.Safari", "Safari");
        let parsed = ComputerUseSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed, s);
        assert!(parsed.is_enabled_for(Path::new("/Users/x/repo")));
    }

    #[test]
    fn a_file_missing_every_key_loads_as_the_safe_default() {
        // An empty or truncated file must not read as "on".
        let s = ComputerUseSettings::from_toml_str("").expect("empty parses");
        assert_eq!(s, ComputerUseSettings::default());
    }

    #[test]
    fn sanitizing_drops_blanks_duplicates_and_relative_paths() {
        let raw = ComputerUseSettings {
            enabled: true,
            projects: vec![
                PathBuf::from("/repo"),
                PathBuf::from("/repo"),
                // Cannot match the absolute root the spawn path holds, so it
                // would look enabled while doing nothing.
                PathBuf::from("relative/repo"),
            ],
            allowed_apps: vec![
                AppGrant {
                    bundle_id: "  com.apple.Safari  ".into(),
                    name: "  Safari  ".into(),
                },
                AppGrant {
                    bundle_id: "COM.APPLE.SAFARI".into(),
                    name: "dupe".into(),
                },
                AppGrant {
                    bundle_id: "   ".into(),
                    name: "blank".into(),
                },
            ],
        }
        .sanitized();

        assert_eq!(raw.projects, vec![PathBuf::from("/repo")]);
        assert_eq!(raw.allowed_apps.len(), 1);
        assert_eq!(raw.allowed_apps[0].bundle_id, "com.apple.Safari");
        assert_eq!(raw.allowed_apps[0].name, "Safari");
    }
}
