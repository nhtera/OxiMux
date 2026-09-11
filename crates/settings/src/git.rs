//! Git & Source Control settings — how OxiMux names the branches it creates,
//! where it puts the worktrees, and whether it freshens the default branch
//! first.
//!
//! Every worktree OxiMux has ever created is on a branch called
//! `oximux/<slug>`, hardcoded at five sites. That prefix is a reasonable
//! default and a poor requirement: a shared repository shows one contributor's
//! branches under a tool's name rather than under theirs, which is backwards
//! from how every other client names a branch.
//!
//! **The serde shape lives here; the resolution does not.** Turning
//! [`BranchPrefixMode::GitUsername`] into an actual prefix means reading
//! `git config user.name`, and this crate has no git dependency on purpose —
//! see `oximux_worktree_ops::branch_name`, which owns that half.

#[cfg(feature = "gpui")]
use gpui::Global;

/// The prefix OxiMux has always used, and still uses until someone changes it.
pub const DEFAULT_PREFIX: &str = "oximux";

/// Where the branch prefix comes from.
///
/// Three modes rather than a free-text field with a magic empty value: "use my
/// git username" is a *rule*, not a string, and it has to keep meaning that
/// after the user changes `user.name`. Storing the resolved username instead
/// would freeze it at the moment the setting was chosen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BranchPrefixMode {
    /// Slugified `git config user.name`, resolved fresh each time.
    GitUsername,
    /// The literal text in [`GitSettings::custom_prefix`]. The default, which
    /// is how an absent `git.toml` reproduces today's `oximux/` behaviour.
    #[default]
    Custom,
    /// No prefix at all — the branch is the bare slug.
    None,
}

impl BranchPrefixMode {
    /// Every variant, in the order the settings pane offers them.
    pub const ALL: &'static [Self] = &[Self::GitUsername, Self::Custom, Self::None];

    /// Label for the segmented control.
    pub fn label(self) -> &'static str {
        match self {
            Self::GitUsername => "Git username",
            Self::Custom => "Custom",
            Self::None => "None",
        }
    }
}

/// Git and source-control preferences, persisted to `git.toml`.
///
/// `#[serde(default)]` per field so a file written by an older build — or
/// hand-edited down to the one key someone cared about — still loads, with
/// every absent key falling back to today's behaviour.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GitSettings {
    /// Where the branch prefix comes from.
    pub branch_prefix: BranchPrefixMode,
    /// The literal prefix used by [`BranchPrefixMode::Custom`].
    ///
    /// Kept even while another mode is selected, deliberately: a user who
    /// tries `Git username` and switches back should find their own text
    /// still there rather than having it silently discarded by a control they
    /// were only looking at.
    pub custom_prefix: String,
    /// Where the desktop creates new worktrees, laid out as
    /// `<dir>/<project>/<slug>`. `None` means the default,
    /// `~/OxiMux/worktrees`. A leading `~` is expanded.
    ///
    /// Validated when it is *used*, not only when the pane saved it — the
    /// file is meant to be hand-edited — and refused with a reason when it
    /// points inside a git working tree, inside the app's data directory, or
    /// somewhere unwritable. Existing worktrees are never moved by changing
    /// it. The headless host ignores it: `oximux serve` keeps the host-derived
    /// scheme under its own data directory.
    pub worktree_dir: Option<String>,
    /// On worktree create, fetch and fast-forward the local default branch so
    /// the new worktree starts from current work rather than from whatever was
    /// last pulled. Off by default: it makes creation touch the network.
    pub keep_default_up_to_date: bool,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self::shipped()
    }
}

impl GitSettings {
    /// File this is persisted to, beside the other per-app settings.
    pub const FILE_NAME: &'static str = "git.toml";

    /// The shipped default: `oximux/<slug>` branches, the default worktree
    /// directory, no fetching on create.
    pub fn shipped() -> Self {
        Self {
            branch_prefix: BranchPrefixMode::Custom,
            custom_prefix: DEFAULT_PREFIX.to_string(),
            worktree_dir: None,
            keep_default_up_to_date: false,
        }
    }

    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("git settings serialize")
    }

    /// Read `git.toml` from an app data directory, degrading to the shipped
    /// default.
    ///
    /// **Shared with the headless host on purpose.** `oximux serve` creates
    /// the same worktrees the sidebar does, so it has to name their branches
    /// the same way — and a second reader would be a second place for the
    /// fallback behaviour, the log line, and the file name to drift. This is
    /// the same reason `load_for_project` lives beside its own type rather
    /// than in each caller.
    ///
    /// **One directory, not one machine.** A `serve` started with its own
    /// `--data-dir` reads that directory's `git.toml`, which the desktop never
    /// writes — so it mints the shipped prefix while the desktop mints the
    /// configured one. That is the correct reading of `--data-dir` (a server
    /// with its own root is a separate installation, not a view onto this
    /// one), but it does mean the two agree only when they share a directory.
    pub fn load_from_dir(dir: &std::path::Path) -> Self {
        let path = dir.join(Self::FILE_NAME);
        let Ok(text) = std::fs::read_to_string(&path) else {
            // Absent is the overwhelmingly common case — nobody has opened the
            // pane yet — and it is not worth a log line.
            return Self::shipped();
        };
        Self::from_toml_str(&text).unwrap_or_else(|err| {
            tracing::warn!(?path, %err, "git.toml parse failed; using defaults");
            Self::shipped()
        })
    }
}

#[cfg(feature = "gpui")]
impl Global for GitSettings {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_default_reproduces_todays_branch_names() {
        let s = GitSettings::shipped();
        assert_eq!(s.branch_prefix, BranchPrefixMode::Custom);
        assert_eq!(s.custom_prefix, "oximux");
        assert_eq!(s.worktree_dir, None);
        assert!(!s.keep_default_up_to_date);
    }

    #[test]
    fn settings_round_trip_through_toml() {
        for mode in BranchPrefixMode::ALL {
            let s = GitSettings {
                branch_prefix: *mode,
                custom_prefix: "team".to_string(),
                worktree_dir: Some("/tmp/wt".to_string()),
                keep_default_up_to_date: true,
            };
            assert_eq!(GitSettings::from_toml_str(&s.to_toml_string()).expect("parses"), s);
        }
    }

    /// A file naming one key must not reset the others to something *other*
    /// than the shipped default — that is the whole point of per-field
    /// defaults, and it is what lets a user hand-edit `git.toml` down to the
    /// single line they care about.
    #[test]
    fn a_partial_file_keeps_shipped_defaults_for_absent_keys() {
        let s = GitSettings::from_toml_str("keep_default_up_to_date = true").expect("parses");
        assert!(s.keep_default_up_to_date);
        assert_eq!(s.branch_prefix, BranchPrefixMode::Custom);
        assert_eq!(s.custom_prefix, "oximux");
    }

    /// Tolerant parsing, matching `project_scripts.rs`: a key this build does
    /// not know is ignored rather than failing the whole file, so a settings
    /// file written by a newer build still loads on an older one.
    #[test]
    fn unknown_keys_are_ignored_rather_than_fatal() {
        let s = GitSettings::from_toml_str(
            "branch_prefix = \"none\"\nsome_future_key = 42\n",
        )
        .expect("parses");
        assert_eq!(s.branch_prefix, BranchPrefixMode::None);
    }

    #[test]
    fn an_absent_file_loads_the_shipped_default_without_complaint() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(GitSettings::load_from_dir(dir.path()), GitSettings::shipped());
    }

    /// The single point where a user could lose their configuration to a typo.
    /// A malformed file must not stop the app from starting, and must not be
    /// silent about it either — the log line is the only signal the user's
    /// edit did not take.
    #[test]
    fn a_malformed_file_degrades_to_the_shipped_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(GitSettings::FILE_NAME), "branch_prefix = [[[")
            .expect("write");
        assert_eq!(GitSettings::load_from_dir(dir.path()), GitSettings::shipped());
    }

    #[test]
    fn a_written_file_reads_back_as_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let want = GitSettings {
            branch_prefix: BranchPrefixMode::GitUsername,
            custom_prefix: "kept".to_string(),
            worktree_dir: None,
            keep_default_up_to_date: true,
        };
        std::fs::write(dir.path().join(GitSettings::FILE_NAME), want.to_toml_string())
            .expect("write");
        assert_eq!(GitSettings::load_from_dir(dir.path()), want);
    }

    #[test]
    fn an_unreadable_prefix_mode_is_a_parse_error_not_a_silent_default() {
        // The caller (`git_settings::load`) is what turns this into the
        // shipped default plus a logged warning. Swallowing it here would
        // leave nothing to log.
        assert!(GitSettings::from_toml_str("branch_prefix = \"wat\"").is_err());
    }
}
