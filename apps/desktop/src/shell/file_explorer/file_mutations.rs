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
            // Windows needs to know up front whether the link points at a
            // directory, so it resolves the source (`from`, following the
            // link) rather than inspecting `target`, which may be relative.
            // Creating these can require Developer Mode; the error propagates
            // rather than leaving a copy that quietly lost its links.
            #[cfg(windows)]
            if std::fs::metadata(&from).is_ok_and(|m| m.is_dir()) {
                std::os::windows::fs::symlink_dir(&target, &to)?;
            } else {
                std::os::windows::fs::symlink_file(&target, &to)?;
            }
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

/// Move `path` to the Windows Recycle Bin via `SHFileOperationW` with
/// `FOF_ALLOWUNDO` — the same reversible delete the macOS arm gets from
/// `trashItemAtURL:`. The younger `IFileOperation` COM API does the same job
/// with apartment-threading ceremony this one call doesn't need.
#[cfg(windows)]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };

    // A relative path would be resolved against the shell's idea of the
    // current directory, not the file tree's root — refuse rather than
    // recycle a guess.
    if !path.is_absolute() {
        return Err(format!("path is not absolute: {}", path.display()));
    }
    // `SHFileOperationW` predates both forward-slash tolerance and the `\\?\`
    // long-path prefix: the former is normalized here, the latter rejected
    // (the API fails on it with an unrelated-looking error, so name the real
    // problem instead). Explorer's own delete has the same length ceiling.
    if path.as_os_str().encode_wide().take(4).eq(r"\\?\".encode_utf16()) {
        return Err(format!(
            "path uses the \\\\?\\ prefix, which the shell delete API cannot handle: {}",
            path.display()
        ));
    }
    // pFrom is a double-NUL-terminated list of NUL-separated paths; a single
    // path still needs BOTH terminators or the shell reads past the string.
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|c| if c == u16::from(b'/') { u16::from(b'\\') } else { c })
        .chain([0, 0])
        .collect();

    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: wide.as_ptr(),
        pTo: std::ptr::null(),
        // ALLOWUNDO is the Recycle Bin itself; the three UI suppressions make
        // this silent like the macOS call — OxiMux's confirm dialog already
        // asked, so a second shell-owned prompt would be noise. windows-sys
        // types the constants u32 but the struct field is the header's WORD.
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };

    // SAFETY: `op` is a fully-initialized SHFILEOPSTRUCTW; `wide` outlives the
    // call and carries the double-NUL terminator the pFrom contract requires.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code == 0 && op.fAnyOperationsAborted == 0 {
        return Ok(());
    }
    if code == 0 {
        return Err("delete was cancelled".to_string());
    }
    // Mixed error space: Win32 codes below 0x71, pre-Win32 shell `DE_*` codes
    // from 0x71 up. Translate the ones a file-tree delete can realistically
    // hit; anything else keeps its number so a bug report stays actionable.
    let reason = match code as u32 {
        0x02 | 0x03 | 0x7C => "file or folder not found",
        0x05 | 0x78 | 0x86 => "access denied",
        0x20 => "the file is in use by another program",
        0x74 => "the operation was cancelled",
        0x7E => "a file with that folder's name already exists",
        other => return Err(format!("shell delete failed (code 0x{other:X})")),
    };
    Err(reason.to_string())
}

/// Fallback for platforms with neither Trash FFI nor a Recycle Bin binding
/// (CI lint hosts). Keeps the crate compiling; never reachable from a shipped
/// build.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn move_to_trash(_path: &Path) -> Result<(), String> {
    Err("move to Trash is not implemented on this platform".to_string())
}

/// What the reversible-delete destination is called on this platform — the
/// difference between a dialog that reads native ("Recycle Bin") and one that
/// reads ported ("Trash" on Windows).
pub fn trash_name() -> &'static str {
    if cfg!(windows) { "Recycle Bin" } else { "Trash" }
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

    // Really recycles a file (one tiny tombstone in the runner's Recycle Bin);
    // the assertion that matters is that the original path is gone, i.e. the
    // shell accepted the operation rather than erroring or silently no-oping.
    #[cfg(windows)]
    #[test]
    fn move_to_trash_removes_the_file() {
        let dir = tmp();
        let victim = dir.join("recycle-me.txt");
        fs::write(&victim, b"bye").unwrap();
        move_to_trash(&victim).unwrap();
        assert!(!victim.exists(), "file must be gone from its original path");
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn move_to_trash_rejects_relative_paths() {
        let err = move_to_trash(Path::new("relative\\nope.txt")).unwrap_err();
        assert!(err.contains("not absolute"), "got: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn move_to_trash_reports_missing_files() {
        let dir = tmp();
        let err = move_to_trash(&dir.join("never-existed.txt")).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
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
