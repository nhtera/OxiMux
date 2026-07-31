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
//! flipping the master does not hand the screen-control tools to every agent in
//! every project at once — an agent driving the GUI is not something to opt into
//! by side effect.
//!
//! # What the project list does not do
//!
//! It scopes the *tools*, not the *permission*. Turning screen control on
//! anywhere requires OxiMux to hold macOS Accessibility, because the Escape kill
//! switch is an event tap and a tap does not exist without it. macOS attributes
//! that grant to OxiMux as the responsible process, and every descendant
//! inherits it — measured, not assumed: a binary spawned from an agent's shell
//! tool reports `AXIsProcessTrusted() == true`, through an intervening helper
//! whose whole job is to disclaim responsibility.
//!
//! So an agent's shell can reach GUI automation in a project that never appears
//! in this list. `oximux_computer_use::gui_scripting` refuses the obvious
//! commands that do, and the settings pane says so plainly rather than implying
//! a fence that is not there.
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

/// Off, everywhere, with nothing pre-approved — the one default a feature that
/// can click on the user's behalf is allowed to have. Pinned by
/// `default_is_off_everywhere_with_nothing_approved`, so a future field whose
/// derived default is "on" fails a test rather than shipping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

impl Global for ComputerUseSettings {}

/// A path in the form both sides of a comparison can agree on.
///
/// `/tmp/x` and `/private/tmp/x` are one directory, and a rule written through a
/// symlinked home would otherwise never fire.
///
/// Resolves the deepest ancestor that exists and re-attaches the rest, rather
/// than resolving the whole path or nothing. Both halves of that matter:
///
/// - A stored root can name a directory that is temporarily absent — an
///   unmounted volume, a worktree since deleted — and dropping the rule then
///   would be indistinguishable from one the user never set.
/// - Resolving *only* whole paths is worse than it looks, because it fails
///   **asymmetrically**: the root resolves to `/private/var/…` while a
///   not-yet-created subdirectory stays `/var/…`, and a containment check
///   between them silently answers no. One side of a comparison must not be
///   normalized while the other is not.
///
/// Same job as `Provenance::new`'s canonicalization on the enforcement side, and
/// deliberately the same answer: the two run in different processes, and a root
/// that resolved differently in each would decide the same chat twice.
fn comparable(path: &Path) -> PathBuf {
    let mut trailing = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(resolved) = cursor.canonicalize() {
            return trailing
                .iter()
                .rev()
                .fold(resolved, |acc: PathBuf, part| acc.join(part));
        }
        // Ran out of ancestors (a relative path, or one whose root does not
        // resolve): nothing here is comparable, so hand back what we were given.
        let (Some(name), Some(parent)) = (cursor.file_name(), cursor.parent()) else {
            return path.to_path_buf();
        };
        trailing.push(name.to_os_string());
        cursor = parent;
    }
}

impl ComputerUseSettings {
    pub const FILE_NAME: &'static str = "computer_use.toml";

    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// May agents working in `dir` drive the screen? Both switches must agree,
    /// which is the whole point of having two.
    ///
    /// `dir` is a chat's working directory, not necessarily a project root, so
    /// this asks whether an opted-in root *covers* it — the root itself or
    /// anything beneath. A chat opened on a subdirectory is still a chat in the
    /// project the user enabled, and requiring the two to be spelled identically
    /// would leave the pane listing a project whose chats silently get nothing.
    ///
    /// Containment is not the whole answer: a linked worktree is a *sibling* of
    /// its project (`suggest_worktree_path` puts it at `<parent>/oximux-wt-…`),
    /// so no amount of prefix matching reaches it. Resolving a worktree back to
    /// its main repository needs git and belongs to the caller; this stays pure
    /// so it can be tested without one.
    pub fn is_enabled_for(&self, dir: &Path) -> bool {
        self.covering_root(dir).is_some()
    }

    /// *Which* opted-in root covers `dir`, if any.
    ///
    /// The distinction matters to any control that offers to turn a project
    /// back off. If the project is covered by an ancestor the user opted in —
    /// a monorepo root, say, with a sub-package also added as its own project —
    /// then removing the sub-package's own path removes nothing and the tools
    /// stay. A control built on [`is_enabled_for`](Self::is_enabled_for) alone
    /// would report "on", offer "turn off", and do nothing: exactly the silent
    /// no-op this list is supposed to have stopped being.
    pub fn covering_root(&self, dir: &Path) -> Option<&Path> {
        if !self.enabled {
            return None;
        }
        let dir = comparable(dir);
        // Component-wise, so a sibling named `<root>-2` is not the prefix match
        // a string comparison would make it.
        self.projects
            .iter()
            .find(|root| dir.starts_with(comparable(root)))
            .map(PathBuf::as_path)
    }

    /// Opt `project` in. Idempotent, so a double-click on the toggle cannot
    /// leave two entries behind.
    ///
    /// Stores the resolved path: the spawn path compares against a cwd the
    /// kernel already resolved, and a root recorded through a symlink would sit
    /// in the file looking enabled while matching nothing.
    pub fn enable_project(&mut self, project: &Path) {
        let project = comparable(project);
        if !self.projects.iter().any(|p| comparable(p) == project) {
            self.projects.push(project);
        }
    }

    /// Opt `project` back out. Returns whether anything changed.
    ///
    /// Matches the way [`enable_project`](Self::enable_project) stores, so a row
    /// added before this normalization existed can still be removed by clicking
    /// it.
    pub fn disable_project(&mut self, project: &Path) -> bool {
        let project = comparable(project);
        let before = self.projects.len();
        self.projects.retain(|p| comparable(p) != project);
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

    /// An absolute path naming something that does not exist, spelled for the
    /// running platform.
    ///
    /// A `/repo` literal is not a substitute: on Windows an absolute path needs
    /// a prefix, so `Path::new("/repo").is_absolute()` is `false` there and
    /// `sanitized` drops it as a relative path. Built from `temp_dir` rather
    /// than a hardcoded drive so nothing here assumes `C:` exists or that the
    /// suite runs from any particular volume.
    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    /// Make `link` a second spelling of the directory `target`.
    ///
    /// Windows uses a **junction**, not a symlink. `symlink_dir` needs
    /// `SeCreateSymbolicLinkPrivilege`, which an unelevated process does not
    /// hold outside Developer Mode — it fails with `ERROR_PRIVILEGE_NOT_HELD`
    /// (os error 1314). A junction needs no privilege, and `canonicalize`
    /// resolves it through the same `GetFinalPathNameByHandle` path, so the
    /// aliasing this test is about is exercised identically. The alternative
    /// was skipping the case on Windows, which would leave the platform where
    /// path spellings diverge most as the one platform with no coverage.
    #[cfg(unix)]
    fn alias_dir(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("symlink");
    }

    #[cfg(windows)]
    fn alias_dir(target: &Path, link: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run mklink");
        assert!(status.success(), "mklink /J failed: {status}");
    }

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
    fn an_opted_in_project_covers_the_directories_beneath_it() {
        // A chat opened on a subdirectory is still a chat in the project the
        // user enabled. Requiring the two to be spelled identically left the
        // pane listing a project whose chats silently got nothing.
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        s.enable_project(Path::new("/repo"));
        assert!(s.is_enabled_for(Path::new("/repo")));
        assert!(s.is_enabled_for(Path::new("/repo/crates/git")));
    }

    #[test]
    fn a_sibling_sharing_a_name_prefix_is_not_covered() {
        // The reason coverage is component-wise rather than a string prefix:
        // `/repo-2` is a different repository that happens to sort next to the
        // enabled one, and `starts_with` on `&str` would hand it the tools.
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        s.enable_project(Path::new("/repo"));
        assert!(!s.is_enabled_for(Path::new("/repo-2")));
        assert!(!s.is_enabled_for(Path::new("/repository")));
        assert!(!s.is_enabled_for(Path::new("/")));
    }

    #[test]
    fn coverage_survives_an_aliased_spelling_of_the_same_directory() {
        // The failure this prevents is silent and total: the pane lists the
        // project, every chat in it is refused, and nothing says why.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("repo");
        std::fs::create_dir(&real).expect("create");
        let link = dir.path().join("link-to-repo");
        alias_dir(&real, &link);

        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        // Opted in through the link, asked about through the real path.
        s.enable_project(&link);
        assert!(s.is_enabled_for(&real));
        // A subdirectory that does not exist yet — a worktree about to be
        // created, a build dir not written. The root resolves and this does
        // not, and resolving only one side of that comparison answers no.
        assert!(s.is_enabled_for(&real.join("not-created-yet")));
        // And the reverse, since either side may be the symlinked spelling.
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        s.enable_project(&real);
        assert!(s.is_enabled_for(&link));
    }

    #[test]
    fn coverage_names_the_root_it_came_from() {
        // So a "turn this off" control can tell the difference between a
        // project it can switch off and one enabled by an ancestor, where
        // removing its own path would silently do nothing.
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        let root = abs("monorepo");
        s.enable_project(&root);
        // Compare against the entry `enable_project` actually stored, not
        // against `root` as written. `comparable` normalizes on the way in, and
        // what it returns is platform-specific — on Windows a canonicalized
        // path carries a `\\?\` prefix, on macOS `/tmp` resolves to
        // `/private/tmp`. The contract under test is "covering_root hands back
        // the stored root", and `projects` is where that root is visible.
        let stored = s.projects[0].clone();
        assert_eq!(
            s.covering_root(&root.join("packages/app")),
            Some(stored.as_path())
        );
        assert_eq!(s.covering_root(&root), Some(stored.as_path()));
        assert_eq!(s.covering_root(&abs("elsewhere")), None);
    }

    #[test]
    fn nothing_is_covered_while_the_master_switch_is_off() {
        let mut s = ComputerUseSettings::default();
        s.enable_project(Path::new("/repo"));
        assert_eq!(s.covering_root(Path::new("/repo")), None);
        assert_eq!(s.covering_root(Path::new("/repo/sub")), None);
    }

    #[test]
    fn a_root_that_no_longer_exists_still_matches_itself() {
        // An unmounted volume or a deleted worktree must not silently drop the
        // user's rule — that is indistinguishable from never having set it.
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        s.enable_project(Path::new("/definitely/not/mounted"));
        assert!(s.is_enabled_for(Path::new("/definitely/not/mounted")));
        assert!(s.is_enabled_for(Path::new("/definitely/not/mounted/sub")));
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
                abs("repo"),
                abs("repo"),
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

        assert_eq!(raw.projects, vec![abs("repo")]);
        assert_eq!(raw.allowed_apps.len(), 1);
        assert_eq!(raw.allowed_apps[0].bundle_id, "com.apple.Safari");
        assert_eq!(raw.allowed_apps[0].name, "Safari");
    }
}
