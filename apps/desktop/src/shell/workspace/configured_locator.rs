//! The desktop's worktree locator: a configured, browsable root.
//!
//! Rail-created worktrees used to live at
//! `<data_dir>/projects/<uuid>/worktrees/<slug>` — inside Application Support,
//! under an opaque project id, findable by nothing. Chat-created ones lived
//! somewhere else again, as a sibling `oximux-wt-<slug>` beside the repo. Both
//! now resolve through this one locator, under a root the user can see and
//! change: `~/OxiMux/worktrees/<project>/<slug>` by default.
//!
//! **Existing rows are not moved.** The row stores its own `worktree_path`, so
//! a worktree created under the old scheme keeps working at its old location
//! forever; the setting affects only the next create.
//!
//! The RPC surface does not use this. `oximux serve` and the desktop's own
//! remote service keep the host-derived scheme through
//! [`oximux_worktree_ops::HostDerivedLocator`], which they construct themselves.

use std::path::{Path, PathBuf};

use gpui::App;
use oximux_core::Project;
use oximux_settings::git::GitSettings;
use oximux_storage::ProjectRepo;
use oximux_worktree_ops::{
    LocateError, WorktreeLocator, project_dir_name, validate_worktree_root,
    validate_worktree_root_shape,
};

/// The default root, beneath the home directory.
pub const DEFAULT_ROOT_UNDER_HOME: &str = "OxiMux/worktrees";

/// Where the desktop puts a new worktree: `<root>/<project>/<slug>`.
#[derive(Debug, Clone)]
pub struct ConfiguredLocator {
    /// The configured or default root, or `None` when neither exists (no home
    /// directory and nothing configured).
    root: Option<PathBuf>,
    /// The app data directory, which the root must not be inside.
    data_dir: Option<PathBuf>,
    /// Every project this host knows, so two repositories with one name get
    /// two directories. See [`project_dir_name`].
    projects: Vec<Project>,
}

impl ConfiguredLocator {
    pub fn new(root: Option<PathBuf>, data_dir: Option<PathBuf>, projects: Vec<Project>) -> Self {
        Self { root, data_dir, projects }
    }

    /// The root `settings` names: `worktree_dir` with a leading `~` expanded,
    /// or [`DEFAULT_ROOT_UNDER_HOME`] when it is unset or blank.
    ///
    /// `~` is honoured because `git.toml` is meant to be hand-edited and
    /// version-controlled, and a path with a home directory spelled out is
    /// not portable between two machines the same file lives on.
    pub fn root_from(settings: &GitSettings, home: Option<&Path>) -> Option<PathBuf> {
        match settings.worktree_dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(configured) => Some(expand_home(configured, home)),
            None => home.map(|h| h.join(DEFAULT_ROOT_UNDER_HOME)),
        }
    }

    /// The root this locator resolves under, as configured and before
    /// validation.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The validated root, or why it was refused. What every
    /// [`locate`](Self::locate) starts from, and what the settings pane runs
    /// when the field is committed.
    pub fn validated_root(&self) -> Result<PathBuf, LocateError> {
        let root = self.root.as_deref().ok_or(LocateError::NoRoot)?;
        validate_worktree_root(root, self.data_dir.as_deref())
    }

    /// [`validated_root`](Self::validated_root) without the write probe —
    /// the per-keystroke form for the settings field, which must not create
    /// a file in the user's home directory on every character typed.
    pub fn root_shape(&self) -> Result<PathBuf, LocateError> {
        let root = self.root.as_deref().ok_or(LocateError::NoRoot)?;
        validate_worktree_root_shape(root, self.data_dir.as_deref())
    }

    /// The directory a project's worktrees share: `<root>/<project dir name>`.
    pub fn project_dir(&self, project: &Project) -> Result<PathBuf, LocateError> {
        Ok(self.validated_root()?.join(project_dir_name(project, &self.projects)))
    }
}

impl WorktreeLocator for ConfiguredLocator {
    /// Validated on every call, not only when the pane saved it — a
    /// hand-edited `git.toml`, or a repository that appeared under the root
    /// since, is caught here rather than producing a nested worktree.
    fn locate(&self, project: &Project, slug: &str) -> Result<PathBuf, LocateError> {
        Ok(self.project_dir(project)?.join(slug))
    }
}

/// `~` and `~/rest` against `home`; anything else as written. With no home
/// directory a `~` path is left as written and the writability check refuses
/// it with a reason, which beats silently creating a directory named `~`.
fn expand_home(configured: &str, home: Option<&Path>) -> PathBuf {
    match (configured.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with(['/', '\\']) => home.join(&rest[1..]),
        _ => PathBuf::from(configured),
    }
}

/// The desktop's locator, built from the saved git settings, the app data
/// directory, the home directory, and the project list.
///
/// Built fresh per create rather than held as a global: it snapshots the
/// project list for name disambiguation, and a global would go stale the
/// moment a project was added.
pub fn desktop_locator(project_repo: &ProjectRepo, cx: &App) -> ConfiguredLocator {
    let settings = crate::git_settings::settings(cx);
    let home = dirs::home_dir();
    let root = ConfiguredLocator::root_from(&settings, home.as_deref());
    let projects = project_repo.list_ordered(usize::MAX).unwrap_or_else(|err| {
        // Without the list, two same-named projects could share a directory.
        // Locating still works; it just cannot disambiguate. Logged because
        // it is a storage failure, not a normal state.
        tracing::warn!(?err, "worktree locator: project list unavailable");
        Vec::new()
    });
    ConfiguredLocator::new(root, crate::app_paths::data_dir(), projects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, name: &str, root: &Path) -> Project {
        Project {
            id: id.to_string(),
            name: name.to_string(),
            root_path: root.to_string_lossy().into_owned(),
            default_branch: "main".to_string(),
            created_at: String::new(),
            last_opened_at: None,
            sort_order: 0.0,
        }
    }

    fn with_dir(dir: Option<&str>) -> GitSettings {
        GitSettings { worktree_dir: dir.map(str::to_string), ..GitSettings::shipped() }
    }

    #[test]
    fn the_default_root_is_under_the_home_directory() {
        let home = Path::new("/home/u");
        assert_eq!(
            ConfiguredLocator::root_from(&with_dir(None), Some(home)),
            Some(PathBuf::from("/home/u/OxiMux/worktrees"))
        );
        // Blank means unset — a user who cleared the field gets the default,
        // not a worktree at the current directory.
        assert_eq!(
            ConfiguredLocator::root_from(&with_dir(Some("   ")), Some(home)),
            Some(PathBuf::from("/home/u/OxiMux/worktrees"))
        );
    }

    #[test]
    fn a_configured_root_is_used_as_written_with_tilde_expanded() {
        let home = Path::new("/home/u");
        assert_eq!(
            ConfiguredLocator::root_from(&with_dir(Some("/srv/wt")), Some(home)),
            Some(PathBuf::from("/srv/wt"))
        );
        assert_eq!(
            ConfiguredLocator::root_from(&with_dir(Some("~/wt")), Some(home)),
            Some(PathBuf::from("/home/u/wt"))
        );
        assert_eq!(
            ConfiguredLocator::root_from(&with_dir(Some("~")), Some(home)),
            Some(PathBuf::from("/home/u"))
        );
        // `~user` is not ours to expand.
        assert_eq!(
            ConfiguredLocator::root_from(&with_dir(Some("~bob/wt")), Some(home)),
            Some(PathBuf::from("~bob/wt"))
        );
    }

    #[test]
    fn no_home_and_no_setting_means_no_root() {
        assert_eq!(ConfiguredLocator::root_from(&with_dir(None), None), None);
        let locator = ConfiguredLocator::new(None, None, Vec::new());
        let p = project("p1", "api", Path::new("/repos/api"));
        assert_eq!(locator.locate(&p, "feat"), Err(LocateError::NoRoot));
    }

    /// The layout the whole phase is for: readable, browsable, per project.
    #[test]
    fn locate_lays_out_root_project_slug() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("worktrees");
        let p = project("p1", "API Service", Path::new("/repos/api"));
        let locator = ConfiguredLocator::new(Some(root.clone()), None, vec![p.clone()]);
        assert_eq!(
            locator.locate(&p, "fix-login").expect("locate"),
            oximux_worktree_ops::canonicalize_lenient(&root)
                .join("api-service")
                .join("fix-login")
        );
    }

    #[test]
    fn two_projects_with_one_name_get_two_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = project("aaaa1111-x", "api", Path::new("/work/api"));
        let b = project("bbbb2222-y", "api", Path::new("/oss/api"));
        let locator = ConfiguredLocator::new(
            Some(tmp.path().to_path_buf()),
            None,
            vec![a.clone(), b.clone()],
        );
        let pa = locator.locate(&a, "feat").expect("a");
        let pb = locator.locate(&b, "feat").expect("b");
        assert_ne!(pa, pb);
        assert!(pa.parent().unwrap().ends_with("api-aaaa1111"), "{}", pa.display());
    }

    /// Validation is in `locate`, not only in the pane: a hand-edited
    /// `git.toml` pointing into a repository is refused at create time with
    /// the reason, never silently redirected.
    #[test]
    fn locate_refuses_a_root_inside_a_repository_every_time() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".git")).expect("fake repo");
        let p = project("p1", "api", Path::new("/repos/api"));
        let locator =
            ConfiguredLocator::new(Some(tmp.path().join("wt")), None, vec![p.clone()]);
        assert!(matches!(
            locator.locate(&p, "feat"),
            Err(LocateError::InsideRepository { .. })
        ));
    }

    /// A root inside the data directory is the location this setting exists
    /// to leave, and is refused as such.
    #[test]
    fn locate_refuses_a_root_inside_the_data_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("mkdir");
        let p = project("p1", "api", Path::new("/repos/api"));
        let locator = ConfiguredLocator::new(
            Some(data_dir.join("worktrees")),
            Some(data_dir),
            vec![p.clone()],
        );
        assert!(matches!(locator.locate(&p, "feat"), Err(LocateError::InsideDataDir { .. })));
    }

    /// The narrowed reclaim rule, on the configured locator: the minted path
    /// and nothing else — not a directory of the user's own under the same
    /// root, not another slug's.
    #[test]
    fn may_reclaim_recognises_only_its_own_minted_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = project("p1", "api", Path::new("/repos/api"));
        let locator =
            ConfiguredLocator::new(Some(tmp.path().to_path_buf()), None, vec![p.clone()]);
        let minted = locator.locate(&p, "feat").expect("locate");
        std::fs::create_dir_all(&minted).expect("mkdir");
        let users_own = tmp.path().join("api").join("my-notes");
        std::fs::create_dir_all(&users_own).expect("mkdir");

        assert!(locator.may_reclaim(&p, "feat", &minted));
        assert!(!locator.may_reclaim(&p, "feat", &users_own));
        assert!(!locator.may_reclaim(&p, "my-notes", &minted));
    }
}
