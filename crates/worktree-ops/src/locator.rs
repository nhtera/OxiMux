//! Where a host puts a new worktree — and whether it may clear what it finds
//! there.
//!
//! The path scheme used to be one function, [`worktree_path`], and one
//! sentence of doctrine: *a client names a project and a slug, never a
//! location*. That sentence is a security property of the **remote** surface,
//! and it still holds — [`HostDerivedLocator`] is that derivation, and the RPC
//! service constructs it itself. But it was never a law for the desktop, which
//! is the host, and whose worktrees ended up under an opaque project UUID in
//! Application Support where no one could find them.
//!
//! So the seam is a trait rather than a config flag. A locator answers two
//! questions that must come from the same place:
//!
//! - [`WorktreeLocator::locate`] — where this slug's worktree goes.
//! - [`WorktreeLocator::may_reclaim`] — whether a directory already at some
//!   path is debris this locator's own interrupted create left behind.
//!
//! **The second is the invariant of this module.** Orphan reclaim is a
//! delete, and the only thing that makes it safe is that it targets a path
//! *this locator minted for this slug* — never a directory a person chose. The
//! answer is per path and per slug; a locator that answered from its own kind
//! (`true` for host-derived, `false` for configured) would either delete a
//! user's directory or leave an interrupted create wedged forever, and both
//! have happened in the design of this crate.

use std::fmt;
use std::path::{Path, PathBuf};

use oximux_core::Project;
use oximux_git::worktree::derive_slug;

/// Where a host puts a new worktree. Implemented once per host kind.
///
/// `Send + Sync` because the desktop holds one across the create's await
/// points; `Debug` because [`crate::Provision`] and the outcomes around it are.
pub trait WorktreeLocator: Send + Sync + fmt::Debug {
    /// The directory a worktree for `slug` in `project` should be created at.
    ///
    /// Validates every time and returns `Err` rather than falling back
    /// silently: a configured root can be hand-edited in `git.toml` without
    /// ever passing through the settings pane, and a repository or a symlink
    /// can appear under a validated path afterwards.
    fn locate(&self, project: &Project, slug: &str) -> Result<PathBuf, LocateError>;

    /// May orphan reclaim clear debris at `path` for `slug`?
    ///
    /// True only when this locator itself would have minted exactly `path` for
    /// exactly `slug` — i.e. the directory is one an interrupted OxiMux create
    /// left behind, not a place a person chose. **Never a blanket answer about
    /// the locator kind**, and never `true` when either side fails to
    /// canonicalize: a mint-check that compares two spellings of a path is how
    /// a reclaim deletes something it did not make.
    fn may_reclaim(&self, project: &Project, slug: &str, path: &Path) -> bool {
        let Ok(minted) = self.locate(project, slug) else {
            return false;
        };
        match (dunce::canonicalize(&minted), dunce::canonicalize(path)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

/// Why a locator refused to name a path.
///
/// Each carries enough to say *what* was wrong with *which* directory, because
/// the reader is a person looking at a settings field or a failed create, and
/// "invalid worktree directory" sends them to a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// No root to derive under: the platform reported no home directory and
    /// nothing is configured.
    NoRoot,
    /// The root sits inside a git working tree. Worktrees nested in a working
    /// tree confuse every tool that walks up to find `.git`, including git.
    InsideRepository { root: PathBuf, repository: PathBuf },
    /// The root is the app's data directory or inside it — the place this
    /// setting exists to move worktrees *out of*.
    InsideDataDir { root: PathBuf },
    /// The root, or its nearest existing ancestor, is not a writable
    /// directory.
    NotWritable { root: PathBuf, reason: String },
    /// The root is relative. A relative root would resolve against whatever
    /// the process's working directory happens to be — `/` for a GUI launch —
    /// and `~user` forms, which nothing here expands, arrive relative too.
    NotAbsolute { root: PathBuf },
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoot => {
                write!(f, "no worktree directory: no home directory and none configured")
            }
            Self::InsideRepository { root, repository } => write!(
                f,
                "worktree directory {} is inside the git repository at {}; \
                 choose a directory outside any working tree",
                root.display(),
                repository.display()
            ),
            Self::InsideDataDir { root } => write!(
                f,
                "worktree directory {} is inside OxiMux's data directory; \
                 choose a directory you can browse",
                root.display()
            ),
            Self::NotWritable { root, reason } => {
                write!(f, "worktree directory {} is not writable: {reason}", root.display())
            }
            Self::NotAbsolute { root } => write!(
                f,
                "worktree directory {} is not an absolute path; spell it out from the root, \
                 or start it with ~/",
                root.display()
            ),
        }
    }
}

impl std::error::Error for LocateError {}

/// Compose the host-derived worktree dir path:
/// `<data_dir>/projects/<project_id>/worktrees/<slug>`.
///
/// `data_dir` is passed rather than resolved here because the two hosts
/// disagree about it: the desktop always uses its own app data root, while
/// `oximux serve` honours `--data-dir`. Deriving it internally would put a
/// server's worktrees under the desktop's directory.
pub fn worktree_path(data_dir: &Path, project_id: &str, slug: &str) -> PathBuf {
    data_dir
        .join("projects")
        .join(project_id)
        .join("worktrees")
        .join(slug)
}

/// The scheme every headless host uses: [`worktree_path`] under a data
/// directory the *host* chose.
///
/// `oximux serve` and the RPC service construct this directly and take no
/// locator from a caller — that is what keeps "a client never supplies a
/// location" true even now that a locator exists which reads a setting.
#[derive(Debug, Clone)]
pub struct HostDerivedLocator {
    data_dir: PathBuf,
}

impl HostDerivedLocator {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    /// The root this locator derives under.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl WorktreeLocator for HostDerivedLocator {
    /// No validation: the data directory is the host's own, and the scheme
    /// under it is the one every existing row was created with.
    fn locate(&self, project: &Project, slug: &str) -> Result<PathBuf, LocateError> {
        Ok(worktree_path(&self.data_dir, &project.id, slug))
    }
}

/// Validate a user-chosen worktree root and return it in a canonical form.
///
/// [`validate_worktree_root_shape`] plus a write probe of the nearest
/// existing ancestor. The probe creates and removes a file, so this is the
/// form for a *commit* — a create, or the settings field being saved — and
/// the shape-only form is for feedback on every keystroke.
///
/// Rejects, in this order:
///
/// 0. a relative root — including a `~user` form nothing expanded;
/// 1. a root equal to or inside `data_dir` — that is the location this
///    setting exists to leave (skipped when the host has no data directory
///    to speak of, which no shipped desktop is without);
/// 2. a root inside a git working tree, judged from its nearest existing
///    ancestor so a root that does not exist yet is checked against where it
///    *would* be created;
/// 3. a root whose nearest existing ancestor is not a writable directory.
///
/// The repository rule runs on the default root too: a user whose home is
/// itself a dotfiles repository would otherwise get worktrees nested inside
/// it, which is exactly the case the rule exists to catch.
///
/// The returned path is canonical as far as it exists — the deepest existing
/// ancestor is resolved and the not-yet-created tail re-joined — so two
/// spellings of one directory (`/tmp/x` and `/private/tmp/x` on macOS) mint
/// one path.
pub fn validate_worktree_root(
    root: &Path,
    data_dir: Option<&Path>,
) -> Result<PathBuf, LocateError> {
    let root = validate_worktree_root_shape(root, data_dir)?;
    let anchor = deepest_existing_ancestor(&root);
    if let Err(reason) = probe_writable(&anchor) {
        return Err(LocateError::NotWritable { root, reason });
    }
    Ok(root)
}

/// The rules of [`validate_worktree_root`] that touch nothing on disk beyond
/// reading it: absolute, outside the data directory, outside any git working
/// tree. Cheap enough to run on every keystroke of the settings field, which
/// is what it is for; the write probe is not, and a probe file created and
/// removed in the user's home directory per keystroke is not something a
/// settings pane should do.
pub fn validate_worktree_root_shape(
    root: &Path,
    data_dir: Option<&Path>,
) -> Result<PathBuf, LocateError> {
    if !root.is_absolute() {
        return Err(LocateError::NotAbsolute { root: root.to_path_buf() });
    }
    let root = canonicalize_lenient(root);
    if let Some(data_dir) = data_dir
        && root.starts_with(canonicalize_lenient(data_dir))
    {
        return Err(LocateError::InsideDataDir { root });
    }
    let anchor = deepest_existing_ancestor(&root);
    if let Some(repository) = enclosing_repository(&anchor) {
        return Err(LocateError::InsideRepository { root, repository });
    }
    Ok(root)
}

/// The directory name a project gets under a configured root.
///
/// Readable first: the project's name, slugified, so `~/OxiMux/worktrees/api/`
/// is what someone browsing to it expects. Unique always: when another project
/// in `all` slugifies to the same name, this one carries a short piece of its
/// id (`api-3f9c2a1b`), because two repositories both called `api` must not
/// share a directory and then collide on every slug.
///
/// The disambiguation is symmetric — *every* project in a name clash carries
/// its id, not just the newcomer — because the alternative ("first one keeps
/// the plain name") needs an ordering nothing durable records.
pub fn project_dir_name<'a>(project: &Project, all: impl IntoIterator<Item = &'a Project>) -> String {
    let base = derive_slug(&project.name);
    let clashes = all
        .into_iter()
        .any(|other| other.id != project.id && derive_slug(&other.name) == base);
    if clashes {
        let tag: String = project.id.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
        format!("{base}-{tag}")
    } else {
        base
    }
}

/// Resolve `path` as far as it exists: the deepest existing ancestor is
/// canonicalized and the remaining components re-joined. A path that exists
/// end to end is simply canonicalized; one that does not exist at all is
/// returned as written.
///
/// Through `dunce`, so a Windows result is `C:\...` and not the `\\?\C:\...`
/// verbatim form — which git would not take as a worktree path, and which
/// `Path::starts_with` would not match against the plain spelling every
/// other path in the app carries.
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(real) = dunce::canonicalize(path) {
        return real;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(real) = dunce::canonicalize(parent) {
            let mut out = real;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

/// The nearest ancestor of `path` (inclusive) that exists on disk.
fn deepest_existing_ancestor(path: &Path) -> PathBuf {
    let mut cursor = path;
    loop {
        if cursor.exists() {
            return cursor.to_path_buf();
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            // A relative path with no existing component: the current
            // directory is where it would be created.
            None => return std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// The working tree that contains `dir`, if any.
///
/// Process-free on purpose: this runs from a settings field's on-change check
/// as well as from every create. It reads exactly what git reads — a `.git`
/// entry, directory *or* file, since a linked worktree's `.git` is a file —
/// and walks up. A bare repository has no working tree and no `.git` entry,
/// so it is correctly not a match.
fn enclosing_repository(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Whether `dir` is a directory the current user can create entries in.
///
/// A permissions bit is not an answer — a read-only *directory* flag means
/// nothing on most filesystems, and ACLs, sandboxes and mounts all say no in
/// ways `metadata` cannot see. So this asks the only way that is always
/// right: create a file and remove it.
///
/// The name carries a per-process counter as well as the pid: two probes in
/// one process — the settings field committing while a create's own
/// `locate` runs — must not collide on `create_new` and report a writable
/// directory as `AlreadyExists`.
fn probe_writable(dir: &Path) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let probe = dir.join(format!(
        ".oximux-write-probe-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|err| err.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, name: &str) -> Project {
        Project {
            id: id.to_string(),
            name: name.to_string(),
            root_path: format!("/repos/{id}"),
            default_branch: "main".to_string(),
            created_at: String::new(),
            last_opened_at: None,
            sort_order: 0.0,
        }
    }

    #[test]
    fn worktree_path_is_host_derived_from_the_data_dir() {
        let path = worktree_path(Path::new("/data"), "proj-1", "feat-x");
        assert_eq!(path, Path::new("/data/projects/proj-1/worktrees/feat-x"));
    }

    /// The whole reason `data_dir` is a parameter: two hosts, two roots.
    #[test]
    fn a_different_data_dir_relocates_the_worktree() {
        let serve = worktree_path(Path::new("/srv/oximux"), "proj-1", "feat-x");
        let desktop = worktree_path(Path::new("/home/u/Library"), "proj-1", "feat-x");
        assert_ne!(serve, desktop);
        assert!(serve.starts_with("/srv/oximux"));
    }

    /// The host-derived locator IS `worktree_path`; extracting the trait
    /// changed no path anywhere.
    #[test]
    fn the_host_derived_locator_mints_the_same_path_the_function_always_did() {
        let locator = HostDerivedLocator::new(PathBuf::from("/data"));
        assert_eq!(
            locator.locate(&project("proj-1", "Api"), "feat-x").expect("locate"),
            worktree_path(Path::new("/data"), "proj-1", "feat-x")
        );
    }

    /// The invariant: reclaim answers for the minted path and nothing else.
    #[test]
    fn may_reclaim_is_true_for_the_minted_path_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let locator = HostDerivedLocator::new(tmp.path().to_path_buf());
        let p = project("proj-1", "Api");
        let minted = locator.locate(&p, "feat").expect("locate");
        std::fs::create_dir_all(&minted).expect("mkdir minted");
        let elsewhere = tmp.path().join("mine");
        std::fs::create_dir_all(&elsewhere).expect("mkdir elsewhere");

        assert!(locator.may_reclaim(&p, "feat", &minted));
        assert!(!locator.may_reclaim(&p, "feat", &elsewhere), "a path we did not mint");
        assert!(!locator.may_reclaim(&p, "other", &minted), "minted, but for another slug");
        assert!(
            !locator.may_reclaim(&project("proj-2", "Api"), "feat", &minted),
            "minted, but for another project"
        );
    }

    /// "Never when canonicalization of either side fails": a minted path that
    /// is not on disk cannot be debris, and must not be reclaimed by a literal
    /// compare that happens to match.
    #[test]
    fn may_reclaim_is_false_when_the_path_does_not_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let locator = HostDerivedLocator::new(tmp.path().to_path_buf());
        let p = project("proj-1", "Api");
        let minted = locator.locate(&p, "feat").expect("locate");
        assert!(!minted.exists());
        assert!(!locator.may_reclaim(&p, "feat", &minted));
    }

    #[test]
    fn a_project_dir_is_the_slugified_name_when_unique() {
        let api = project("11111111-aaaa", "API Service");
        let web = project("22222222-bbbb", "Web");
        assert_eq!(project_dir_name(&api, [&api, &web]), "api-service");
    }

    /// Two repositories both called `api` must not share a directory — and
    /// both carry the tag, so the answer does not depend on which one the
    /// caller listed first.
    #[test]
    fn same_named_projects_each_carry_a_piece_of_their_id() {
        let a = project("3f9c2a1b-1111", "api");
        let b = project("7e0d4c5f-2222", "Api");
        let all = [&a, &b];
        assert_eq!(project_dir_name(&a, all), "api-3f9c2a1b");
        assert_eq!(project_dir_name(&b, all), "api-7e0d4c5f");
        assert_ne!(project_dir_name(&a, all), project_dir_name(&b, all));
    }

    #[test]
    fn a_project_does_not_clash_with_itself() {
        let a = project("3f9c2a1b-1111", "api");
        assert_eq!(project_dir_name(&a, [&a]), "api");
    }

    #[test]
    fn a_root_inside_the_data_dir_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("mkdir");
        let err = validate_worktree_root(&data_dir.join("worktrees"), Some(&data_dir)).unwrap_err();
        assert!(matches!(err, LocateError::InsideDataDir { .. }), "{err}");
        let err = validate_worktree_root(&data_dir, Some(&data_dir)).unwrap_err();
        assert!(matches!(err, LocateError::InsideDataDir { .. }), "equal counts too: {err}");
    }

    /// `/data-x` is not inside `/data`: component-wise, never a string prefix.
    #[test]
    fn a_sibling_that_shares_a_string_prefix_is_not_inside_the_data_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        let sibling = tmp.path().join("data-worktrees");
        std::fs::create_dir_all(&data_dir).expect("mkdir");
        std::fs::create_dir_all(&sibling).expect("mkdir");
        assert!(!sibling.starts_with(&data_dir));
        assert!(validate_worktree_root(&sibling, Some(&data_dir)).is_ok());
    }

    /// The rule the default root can trip: a home that is itself a repository.
    #[test]
    fn a_root_inside_a_git_working_tree_is_refused_with_the_repository_named() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".git")).expect("fake repo");
        let data_dir = tmp.path().join("data");
        // The root does not exist yet — judged from where it would be created.
        let root = home.join("OxiMux").join("worktrees");
        let err = validate_worktree_root(&root, Some(&data_dir)).unwrap_err();
        match err {
            LocateError::InsideRepository { repository, .. } => {
                assert_eq!(repository, canonicalize_lenient(&home));
            }
            other => panic!("expected InsideRepository, got {other}"),
        }
    }

    /// A linked worktree's `.git` is a file, and nesting under one is just as
    /// wrong as nesting under a primary checkout.
    #[test]
    fn a_git_file_counts_as_a_working_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let linked = tmp.path().join("linked");
        std::fs::create_dir_all(&linked).expect("mkdir");
        std::fs::write(linked.join(".git"), "gitdir: /elsewhere/.git/worktrees/linked\n")
            .expect("write");
        let err = validate_worktree_root(&linked.join("wt"), Some(&tmp.path().join("data")))
            .unwrap_err();
        assert!(matches!(err, LocateError::InsideRepository { .. }), "{err}");
    }

    #[test]
    fn a_plain_writable_directory_is_accepted_and_canonicalized() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("worktrees");
        let ok = validate_worktree_root(&root, Some(&tmp.path().join("data"))).expect("valid");
        assert_eq!(ok, canonicalize_lenient(&root));
        assert!(!root.exists(), "validation must not create the root");
        assert!(
            std::fs::read_dir(tmp.path())
                .expect("read")
                .all(|e| !e.expect("entry").file_name().to_string_lossy().contains("probe")),
            "the write probe must not be left behind"
        );
    }

    /// A relative root would be judged against the process's working
    /// directory, which for a GUI launch is `/` — so `wt` would validate as
    /// writable-or-not against the wrong place entirely, and `~bob/wt` would
    /// mean a directory literally called `~bob`.
    #[test]
    fn a_relative_root_is_refused_before_anything_is_touched() {
        for rel in ["wt", "./wt", "~bob/wt"] {
            let err = validate_worktree_root(Path::new(rel), None).unwrap_err();
            assert!(matches!(err, LocateError::NotAbsolute { .. }), "{rel}: {err}");
        }
    }

    #[test]
    fn a_root_whose_anchor_is_a_file_is_not_writable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("a-file");
        std::fs::write(&file, "x").expect("write");
        let err = validate_worktree_root(&file.join("wt"), Some(&tmp.path().join("data")))
            .unwrap_err();
        assert!(matches!(err, LocateError::NotWritable { .. }), "{err}");
    }

    /// The shape check is what runs per keystroke; it must reach the same
    /// verdicts as the full check for everything but writability, and touch
    /// nothing.
    #[test]
    fn the_shape_check_agrees_with_the_full_check_and_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("repo").join(".git")).expect("fake repo");
        let data_dir = tmp.path().join("data");
        let cases = [tmp.path().join("ok"), tmp.path().join("repo").join("wt"), data_dir.join("x")];
        for root in &cases {
            let shape = validate_worktree_root_shape(root, Some(&data_dir));
            let full = validate_worktree_root(root, Some(&data_dir));
            assert_eq!(shape, full, "{}", root.display());
        }
        // A file at the anchor is the one thing only the probe catches.
        let file = tmp.path().join("a-file");
        std::fs::write(&file, "x").expect("write");
        assert!(validate_worktree_root_shape(&file.join("wt"), None).is_ok());
        assert!(validate_worktree_root(&file.join("wt"), None).is_err());
    }

    /// Canonical, but never verbatim: on Windows `fs::canonicalize` answers
    /// `\\?\C:\...`, which git refuses as a worktree path. On every platform
    /// the answer must start the way the input's real location is spelled.
    #[test]
    fn canonicalize_lenient_never_yields_a_verbatim_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = canonicalize_lenient(tmp.path());
        assert!(!real.to_string_lossy().starts_with(r"\\?\"), "{}", real.display());
        assert_eq!(real, dunce::canonicalize(tmp.path()).expect("canon"));
    }

    #[test]
    fn canonicalize_lenient_resolves_the_existing_head_and_keeps_the_tail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = dunce::canonicalize(tmp.path()).expect("canon");
        let want = real.join("not").join("yet");
        assert_eq!(canonicalize_lenient(&tmp.path().join("not").join("yet")), want);
    }
}
