//! Filesystem mutations for the file explorer: duplicate and move-to-Trash.
//!
//! Kept separate from the menu/render code so the path math (collision-free
//! duplicate name) and the macOS Trash FFI stay independently testable and
//! don't bloat the context-menu module.

use std::path::{Path, PathBuf};

/// Duplicate `src` next to itself with a collision-free name and return the
/// new path. Files copy byte-for-byte; directories copy recursively. The new
/// name inserts " copy" before the extension (`notes.md` → `notes copy.md`,
/// `notes copy.md` → `notes copy 2.md`), mirroring Finder's scheme. A folder
/// `src` (no extension) becomes `src copy`, then `src copy 2`.
pub fn duplicate_path(src: &Path) -> std::io::Result<PathBuf> {
    let parent = src.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot duplicate a path with no parent directory",
        )
    })?;
    let dest = collision_free_duplicate_name(src, parent);
    // `is_dir()` follows symlinks; a symlink-to-dir at the top level is copied
    // as a file (its link target), which is the Finder-equivalent behavior.
    if src.is_dir() && !src.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        // On any mid-tree failure, remove the partial destination so a failed
        // duplicate doesn't leave a half-copied folder behind.
        if let Err(err) = copy_dir_recursive(src, &dest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(err);
        }
    } else {
        std::fs::copy(src, &dest)?;
    }
    Ok(dest)
}

/// Compute a non-existing duplicate target in `parent`. Pure path math (no IO
/// beyond existence probing) so it's unit-testable. The first candidate is
/// `<stem> copy<.ext>`; subsequent collisions append ` 2`, ` 3`, ….
fn collision_free_duplicate_name(src: &Path, parent: &Path) -> PathBuf {
    // Split into stem + extension. `file_stem`/`extension` treat a leading-dot
    // name (`.gitignore`) as all-stem, which is what we want — the duplicate
    // becomes `.gitignore copy`, not `.gitignore.copy`.
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = src.extension().map(|e| e.to_string_lossy().into_owned());

    let build = |suffix: &str| -> PathBuf {
        let name = match &ext {
            Some(ext) => format!("{stem} copy{suffix}.{ext}"),
            None => format!("{stem} copy{suffix}"),
        };
        parent.join(name)
    };

    let first = build("");
    if !first.exists() {
        return first;
    }
    // " copy" was taken — count up " copy 2", " copy 3", … until free.
    for n in 2..10_000 {
        let candidate = build(&format!(" {n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Pathological fallback: 10k duplicates already exist. Hand back the first
    // candidate and let the copy fail loudly rather than loop forever.
    first
}

/// Recursively copy a directory tree. Used by `duplicate_path` for folders;
/// `std::fs` has no built-in recursive copy.
fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        // `file_type()` does NOT follow symlinks. Recreate symlinks verbatim
        // (preserving the link, not its target) so a symlink-to-dir doesn't
        // fall through to `fs::copy` and fail with EISDIR mid-copy.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
        } else if ft.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Move `path` to the macOS Trash via `NSFileManager trashItemAtURL:`. Unlike
/// `std::fs::remove_*` this is reversible (the user can restore from Trash),
/// which is the right default for a one-click Delete in the file tree.
/// Returns a human-readable error string on failure.
#[cfg(target_os = "macos")]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let path_str = path.to_string_lossy();
    // `fileURLWithPath:` and `trashItemAtURL:resultingItemURL:error:` are safe
    // objc2 wrappers; we pass no out-param for the resulting URL.
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path_str));
    let fm = NSFileManager::defaultManager();
    fm.trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|err| err.localizedDescription().to_string())
}

/// Non-macOS fallback. OxiMux ships macOS-only today; this keeps the crate
/// compiling on other targets (CI lint hosts) without a real Trash.
#[cfg(not(target_os = "macos"))]
pub fn move_to_trash(_path: &Path) -> Result<(), String> {
    Err("move to Trash is only implemented on macOS".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        // Per-test unique subdir. This used to be a nanosecond timestamp, which
        // is NOT unique: the tests in this module run on parallel threads, and
        // two that read the clock inside the same tick got the same directory.
        // Whichever finished first then `remove_dir_all`'d it out from under the
        // other, which failed mid-copy with ENOENT — a ~12% flake locally and a
        // red CI run. pid + counter is unique by construction, across both
        // threads and the concurrently-run test binaries.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "oximux-dup-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn duplicate_file_inserts_copy_before_extension() {
        let dir = tmp();
        let src = dir.join("notes.md");
        fs::write(&src, b"hello").unwrap();
        let dest = duplicate_path(&src).unwrap();
        assert_eq!(dest.file_name().unwrap(), "notes copy.md");
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_second_time_counts_up() {
        let dir = tmp();
        let src = dir.join("a.txt");
        fs::write(&src, b"x").unwrap();
        let first = duplicate_path(&src).unwrap();
        assert_eq!(first.file_name().unwrap(), "a copy.txt");
        let second = duplicate_path(&src).unwrap();
        assert_eq!(second.file_name().unwrap(), "a copy 2.txt");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_extensionless_folder() {
        let dir = tmp();
        let src = dir.join("subdir");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("inner.txt"), b"y").unwrap();
        let dest = duplicate_path(&src).unwrap();
        assert_eq!(dest.file_name().unwrap(), "subdir copy");
        assert!(dest.join("inner.txt").is_file());
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_folder_with_symlink_to_dir_recreates_link() {
        let dir = tmp();
        let src = dir.join("tree");
        fs::create_dir(&src).unwrap();
        let real_target = dir.join("target-dir");
        fs::create_dir(&real_target).unwrap();
        // A symlink-to-dir inside the tree must be recreated as a link, not
        // copied (which would fail with EISDIR and leave a partial dest).
        std::os::unix::fs::symlink(&real_target, src.join("link")).unwrap();
        let dest = duplicate_path(&src).unwrap();
        assert_eq!(dest.file_name().unwrap(), "tree copy");
        let copied_link = dest.join("link");
        assert!(copied_link.symlink_metadata().unwrap().file_type().is_symlink());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_dotfile_keeps_leading_dot() {
        let dir = tmp();
        let src = dir.join(".gitignore");
        fs::write(&src, b"target/").unwrap();
        let dest = duplicate_path(&src).unwrap();
        assert_eq!(dest.file_name().unwrap(), ".gitignore copy");
        fs::remove_dir_all(&dir).ok();
    }
}
