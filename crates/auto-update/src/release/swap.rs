//! Replacing the installed binaries — all of them, or none.
//!
//! The CLI and the relay speak a mutually-authenticated handshake that is
//! versioned in lockstep, so an update that lands one and not the other leaves
//! an installation that cannot talk to itself. Every swap here is therefore
//! staged, then committed in two passes with a rollback between them.
//!
//! **Windows is why the shape is "move aside, then move in" rather than "move
//! over".** A running `.exe` cannot be overwritten, but it *can* be renamed —
//! so the live binary is renamed out of the way first and the new one takes
//! the vacated name. That works identically on unix, so both platforms run one
//! code path; the only difference is the last step, where unix deletes the
//! backup immediately and Windows cannot (the bytes are still mapped into the
//! running process) and defers it to [`sweep_backups`] at the next start.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Backups are `<name>.old-<hex>`. The random suffix means a swap never has to
/// clear a previous backup first — an important property on Windows, where a
/// previous backup may still be un-deletable because something is running it.
const BACKUP_INFIX: &str = ".old-";

#[derive(Debug)]
pub struct Replacement {
    /// The binary being replaced, at its installed path.
    pub installed: PathBuf,
    /// The verified new bytes, already on the same filesystem.
    pub staged: PathBuf,
}

#[derive(Debug)]
pub enum SwapError {
    /// A rename failed and everything was put back. The installation is
    /// exactly as it was.
    RolledBack { path: PathBuf, detail: String },
    /// A rename failed and the rollback failed too. This is the one state
    /// worth shouting about, because a human has to look.
    Inconsistent { path: PathBuf, detail: String, backups: Vec<PathBuf> },
    /// Making a staged binary executable failed.
    ///
    /// unix-only, and gated so it says so: the mode bits are the only thing
    /// [`make_executable`] touches, and its Windows counterpart is a no-op
    /// because Windows has no execute bit. Left ungated, this variant is
    /// unreachable there — which `-D warnings` turns into a dead-code build
    /// failure on the one platform a macOS developer never compiles for.
    #[cfg(unix)]
    Io { path: PathBuf, detail: String },
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RolledBack { path, detail } => write!(
                f,
                "could not replace {} ({detail}) — nothing was changed",
                path.display()
            ),
            Self::Inconsistent { path, detail, backups } => write!(
                f,
                "could not replace {} ({detail}) AND could not undo the change. The previous \
                 binaries are at: {}",
                path.display(),
                backups.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
            #[cfg(unix)]
            Self::Io { path, detail } => write!(f, "{}: {detail}", path.display()),
        }
    }
}

impl SwapError {
    /// The directory a failure was about, and why it failed — the two facts
    /// every caller's advice is built from.
    ///
    /// The wording itself deliberately is not here. "Reinstall instead: curl …
    /// install-cli.sh" is right for the CLI and nonsense for the desktop app,
    /// which shares this swap but not that remedy, so each renders its own
    /// guidance from these two facts.
    ///
    /// `None` for the inconsistent case, which is not about one directory: the
    /// backups named in the error are what a human has to act on.
    pub fn context(&self) -> Option<(&Path, &str)> {
        // Bound in a match rather than an or-pattern because `Io` is unix-only
        // and an or-pattern cannot carry a `#[cfg]` on just one alternative.
        let (path, detail) = match self {
            Self::Inconsistent { .. } => return None,
            Self::RolledBack { path, detail } => (path, detail),
            #[cfg(unix)]
            Self::Io { path, detail } => (path, detail),
        };
        Some((path.parent().unwrap_or(path), detail.as_str()))
    }

    /// Whether the failure was the install directory refusing to be written —
    /// by far the most common one, and the only one with a specific answer.
    pub fn is_permission_denied(&self) -> bool {
        self.context()
            .is_some_and(|(_, detail)| detail.to_lowercase().contains("permission"))
    }
}

/// Commit every replacement, or none of them.
///
/// Returns the backup paths left behind. On unix that list is empty — the
/// backups are gone by the time this returns. On Windows it is what
/// [`sweep_backups`] will clear at the next start.
pub fn swap_all(replacements: &[Replacement]) -> Result<Vec<PathBuf>, SwapError> {
    for replacement in replacements {
        make_executable(&replacement.staged)?;
    }

    // Pass 1: vacate every installed name. A binary that is not installed yet
    // — a hand-copied CLI with no relay beside it — has nothing to vacate and
    // is simply placed in pass 2; undoing that means deleting it again.
    let mut moved_aside: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for replacement in replacements {
        if !replacement.installed.exists() {
            moved_aside.push((replacement.installed.clone(), None));
            continue;
        }
        let backup = backup_path(&replacement.installed);
        if let Err(err) = fs::rename(&replacement.installed, &backup) {
            undo_moves(&moved_aside);
            return Err(SwapError::RolledBack {
                path: replacement.installed.clone(),
                detail: err.to_string(),
            });
        }
        moved_aside.push((replacement.installed.clone(), Some(backup)));
    }

    // Pass 2: move the new binaries into the vacated names.
    let mut installed: Vec<PathBuf> = Vec::new();
    for replacement in replacements {
        if let Err(err) = fs::rename(&replacement.staged, &replacement.installed) {
            // Undo pass 2 first, so pass 1's undo finds its destinations free.
            for path in &installed {
                let _ = fs::rename(path, staged_of(replacements, path));
            }
            let failed = undo_moves(&moved_aside);
            let detail = err.to_string();
            return Err(if failed.is_empty() {
                SwapError::RolledBack { path: replacement.installed.clone(), detail }
            } else {
                SwapError::Inconsistent {
                    path: replacement.installed.clone(),
                    detail,
                    backups: failed,
                }
            });
        }
        installed.push(replacement.installed.clone());
    }

    // Pass 3: retire the backups. Unix can delete them now; Windows cannot
    // delete an image that is still mapped into this very process.
    let mut left_behind = Vec::new();
    for backup in moved_aside.into_iter().filter_map(|(_, backup)| backup) {
        if fs::remove_file(&backup).is_err() {
            left_behind.push(backup);
        }
    }
    Ok(left_behind)
}

/// Put every moved-aside binary back. Returns the ones that could not be
/// restored — a non-empty result is the inconsistent case.
fn undo_moves(moved: &[(PathBuf, Option<PathBuf>)]) -> Vec<PathBuf> {
    moved
        .iter()
        .filter_map(|(original, backup)| match backup {
            Some(backup) => fs::rename(backup, original).is_err().then(|| backup.clone()),
            // Nothing was displaced; pass 2 may have created it. Removing a
            // file that was never created is a no-op, so this is safe either
            // way and leaves no half-installed binary behind.
            None => {
                let _ = fs::remove_file(original);
                None
            }
        })
        .collect()
}

fn staged_of(replacements: &[Replacement], installed: &Path) -> PathBuf {
    replacements
        .iter()
        .find(|r| r.installed == installed)
        .map(|r| r.staged.clone())
        .unwrap_or_else(|| installed.with_extension("rollback"))
}

fn backup_path(installed: &Path) -> PathBuf {
    let mut suffix = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
    let name = installed.file_name().unwrap_or_else(|| OsStr::new("oximux")).to_string_lossy();
    let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    installed.with_file_name(format!("{name}{BACKUP_INFIX}{hex}"))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), SwapError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|err| SwapError::Io {
        path: path.to_path_buf(),
        detail: format!("could not make the new binary executable: {err}"),
    })
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), SwapError> {
    Ok(())
}

/// Delete backups a previous update could not remove because the files were
/// still running. Best-effort and silent: a read-only install directory is a
/// reason to leave the file, not to fail a command the user actually ran.
///
/// Call once at start. On unix it finds nothing (pass 3 already deleted them),
/// so it is Windows' completion of the swap rather than a shared step.
///
/// `ours` decides which names may be deleted, and there is no default: the two
/// callers own very different directories. The CLI shares `~/.local/bin` with
/// every other tool on the machine and must claim only names it installed; the
/// desktop app owns its install directory outright and claims all of them. A
/// built-in answer would be wrong for one of them, silently.
pub fn sweep_backups(install_dir: &Path, ours: impl Fn(&str) -> bool) {
    for (backup, _) in backups_in(install_dir, ours) {
        let _ = fs::remove_file(backup);
    }
}

/// The same sweep, except that a backup whose original is *missing* is renamed
/// back instead of deleted.
///
/// This is the repair for a swap that was killed between its two passes — the
/// window where a file has been moved aside and its replacement has not yet
/// landed. `swap_all` rolls that back itself when it merely fails, but it
/// cannot roll back a process that no longer exists, and the plain sweep would
/// then delete the only surviving copy of the file.
///
/// Not needed by the CLI, whose two binaries are replaced in a single call the
/// user is watching; very much needed by the desktop app, which replaces a
/// directory's worth of files during quit and can be killed at any point in it.
pub fn restore_or_sweep_backups(install_dir: &Path, ours: impl Fn(&str) -> bool) {
    for (backup, original) in backups_in(install_dir, ours) {
        if original.exists() {
            let _ = fs::remove_file(backup);
        } else {
            let _ = fs::rename(&backup, &original);
        }
    }
}

/// Every `<name>.old-<hex>` in `dir` that `ours` claims, paired with the
/// `<name>` it was moved aside from.
fn backups_in(dir: &Path, ours: impl Fn(&str) -> bool) -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let original = name.split(BACKUP_INFIX).next()?;
            // `split` yields the whole string when the infix is absent, so the
            // length check is what actually decides whether this is a backup.
            if original.len() == name.len() || !ours(&name) {
                return None;
            }
            Some((entry.path(), dir.join(original)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two "installed" binaries and their staged replacements, in one dir so
    /// the renames stay on one filesystem exactly as the real pipeline does.
    fn install(dir: &Path, pairs: &[(&str, &str, &str)]) -> Vec<Replacement> {
        pairs
            .iter()
            .map(|(name, old, new)| {
                let installed = dir.join(name);
                let staged = dir.join(format!(".{name}.new"));
                fs::write(&installed, old).expect("write installed");
                fs::write(&staged, new).expect("write staged");
                Replacement { installed, staged }
            })
            .collect()
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("readable")
    }

    #[test]
    fn both_binaries_move_together() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = install(
            dir.path(),
            &[("oximux", "cli v1", "cli v2"), ("oximux-relay", "relay v1", "relay v2")],
        );
        let left = swap_all(&plan).expect("swaps");

        assert_eq!(read(&plan[0].installed), "cli v2");
        assert_eq!(read(&plan[1].installed), "relay v2");
        assert!(!plan[0].staged.exists(), "the staged copy is consumed, not left behind");
        #[cfg(unix)]
        assert!(left.is_empty(), "unix deletes its backups in-line");
        #[cfg(not(unix))]
        let _ = left;
    }

    /// The property the pairing exists for: if the second binary cannot be
    /// replaced, the first must not stay replaced either — a new CLI against
    /// an old relay cannot complete a handshake.
    ///
    /// Failing *mid*-pass-2 is arranged by pointing both replacements at one
    /// staged file: the first move consumes it and the second finds nothing.
    /// That is the same shape as a staged copy vanishing between verification
    /// and the swap, and the only construction that fails deterministically on
    /// every platform after a binary is already in place.
    #[test]
    fn a_failure_on_the_second_binary_rolls_the_first_one_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut plan = install(
            dir.path(),
            &[("oximux", "cli v1", "cli v2"), ("oximux-relay", "relay v1", "relay v2")],
        );
        fs::remove_file(&plan[1].staged).expect("remove");
        plan[1].staged = plan[0].staged.clone();

        let err = swap_all(&plan).expect_err("must fail");
        assert!(matches!(err, SwapError::RolledBack { .. }), "{err}");
        assert_eq!(read(&plan[0].installed), "cli v1", "the CLI must be back to v1");
        assert_eq!(read(&plan[1].installed), "relay v1");
    }

    /// A CLI copied by hand has no relay beside it. The update carries one, so
    /// the swap places it rather than failing on "nothing to move aside" —
    /// otherwise updating would be the one operation that cannot fix a
    /// half-installed installation.
    #[test]
    fn a_binary_that_is_not_installed_yet_is_placed_alongside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut plan = install(dir.path(), &[("oximux", "cli v1", "cli v2")]);
        let fresh = dir.path().join("oximux-relay");
        let staged_relay = dir.path().join(".oximux-relay.new");
        fs::write(&staged_relay, "relay v2").expect("write");
        plan.push(Replacement { installed: fresh.clone(), staged: staged_relay });

        swap_all(&plan).expect("swaps");
        assert_eq!(read(&plan[0].installed), "cli v2");
        assert_eq!(read(&fresh), "relay v2");
    }

    /// …and rolling back must not leave that freshly placed binary behind:
    /// half an update is what the pairing exists to prevent.
    #[test]
    fn rolling_back_removes_a_binary_that_had_nothing_to_restore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_relay = dir.path().join(".oximux-relay.new");
        fs::write(&staged_relay, "relay v2").expect("write");
        let fresh = dir.path().join("oximux-relay");
        // The relay is placed first, then the CLI's move fails — same shared-
        // staged-file construction as above.
        let mut plan = vec![Replacement { installed: fresh.clone(), staged: staged_relay }];
        plan.extend(install(dir.path(), &[("oximux", "cli v1", "cli v2")]));
        plan[1].staged = plan[0].staged.clone();

        swap_all(&plan).expect_err("must fail");
        assert!(!fresh.exists(), "the freshly placed relay must be removed again");
        assert_eq!(read(&plan[1].installed), "cli v1");
    }

    /// Windows leaves its backups for the next start; this is that next start.
    #[test]
    fn the_sweep_removes_our_backups_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = dir.path().join("oximux.exe.old-deadbeef");
        let relay = dir.path().join("oximux-relay.old-01020304");
        let theirs = dir.path().join("notes.old-01020304");
        let live = dir.path().join("oximux");
        for path in [&ours, &relay, &theirs, &live] {
            fs::write(path, "x").expect("write");
        }

        sweep_backups(dir.path(), |name| name.starts_with("oximux"));

        assert!(!ours.exists() && !relay.exists(), "our backups are swept");
        assert!(theirs.exists(), "another program's file must survive");
        assert!(live.exists(), "the installed binary must survive");
    }

    /// The desktop app's filter. Its install directory holds `rg.exe` and
    /// `onnxruntime.dll` beside the oximux binaries, so a name-prefix rule
    /// would strand exactly the backups Windows most often cannot delete.
    #[test]
    fn a_caller_that_owns_the_whole_directory_sweeps_every_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dll = dir.path().join("onnxruntime.dll.old-deadbeef");
        let live = dir.path().join("onnxruntime.dll");
        for path in [&dll, &live] {
            fs::write(path, "x").expect("write");
        }

        sweep_backups(dir.path(), |_| true);

        assert!(!dll.exists(), "a non-oximux backup is still ours to sweep");
        assert!(live.exists(), "the installed library must survive");
    }

    /// A swap killed between its two passes: the file was moved aside and its
    /// replacement never landed. Deleting the backup here would destroy the
    /// only copy — the install would be missing a DLL with nothing to restore.
    #[test]
    fn a_backup_whose_original_is_missing_is_put_back_not_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orphaned = dir.path().join("onnxruntime.dll.old-deadbeef");
        let superseded = dir.path().join("oximux.exe.old-01020304");
        let live = dir.path().join("oximux.exe");
        for path in [&orphaned, &superseded, &live] {
            fs::write(path, "x").expect("write");
        }

        restore_or_sweep_backups(dir.path(), |_| true);

        assert!(!orphaned.exists(), "the orphaned backup is consumed");
        assert!(dir.path().join("onnxruntime.dll").is_file(), "…by being renamed back");
        assert!(!superseded.exists(), "a backup whose original is present is still swept");
        assert!(live.is_file());
    }

    #[test]
    fn the_sweep_is_silent_on_a_directory_that_is_not_there() {
        sweep_backups(Path::new("/definitely/not/a/directory/here"), |_| true);
        restore_or_sweep_backups(Path::new("/definitely/not/a/directory/here"), |_| true);
    }

    #[test]
    fn backup_names_do_not_collide() {
        let path = Path::new("/tmp/oximux");
        let a = backup_path(path);
        let b = backup_path(path);
        assert_ne!(a, b);
        for name in [&a, &b] {
            let name = name.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("oximux.old-"), "{name}");
        }
    }

    /// The failure a user hits when they installed with a different account.
    #[cfg(unix)]
    #[test]
    fn a_permission_failure_says_which_directory_is_unwritable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("bin");
        fs::create_dir(&locked).expect("mkdir");
        let plan = install(&locked, &[("oximux", "cli v1", "cli v2")]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");

        let outcome = swap_all(&plan);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restore");

        // root ignores the mode bits, so a CI container running as root sees
        // the swap succeed. That is not this test's subject.
        let Err(err) = outcome else {
            return;
        };
        assert!(err.is_permission_denied(), "{err}");
        let (dir, _) = err.context().expect("a rolled-back swap names its directory");
        assert_eq!(dir, locked, "the advice has to name the directory to fix");
    }
}
