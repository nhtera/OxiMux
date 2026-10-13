//! The host side of `sim exec`'s adb transfers, done so that adb touches only
//! what the user approved: it never opens a worktree path itself.
//!
//! The question shows the worktree paths, but the user may answer up to a
//! minute and a half later, and adb resolves a path string again when it runs,
//! following any symlink an agent put there meanwhile. So after the answer an
//! input (`install`, `push`) is opened without following a final symlink, its
//! real path checked inside the worktree, and copied into a private folder
//! that adb reads instead. A `pull` writes into a private folder too, and its
//! files are then placed in the worktree through folder handles: the target
//! folder is opened and checked where it really is once, and everything goes
//! in relative to that handle (never through a link), each file written
//! under a scratch name and renamed over its own, as a plain file (no execute
//! bit), never under a `.git` folder.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use oximux_remote_proto::simulator::SimErrorWire;

/// A word naming git's folder, in any case (the Mac's disk ignores case).
pub(super) fn is_git(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| n.eq_ignore_ascii_case(".git"))
}

/// Whether `path` passes through a `.git` folder.
pub(super) fn in_git(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::Normal(n) if is_git(n)))
}

/// Where transfers' private folders go: the app's data, not the temp folder,
/// which agents' sandboxes usually may write (and so could swap a staged or
/// pulled file mid-transfer).
pub(super) fn private_base() -> io::Result<PathBuf> {
    Ok(crate::app_paths::data_dir().ok_or_else(|| io::Error::other("no app data folder"))?.join("exec"))
}

/// A fresh folder under `base` (made owner-only) for one transfer.
pub(super) fn private_dir(base: &Path) -> io::Result<tempfile::TempDir> {
    fs::create_dir_all(base)?;
    #[cfg(unix)]
    fs::set_permissions(base, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    tempfile::Builder::new().prefix("t").tempdir_in(base)
}

/// One plain file with no other name: a hard link inside the worktree could
/// name a file outside it.
fn plain_file(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    #[cfg(unix)]
    let single = std::os::unix::fs::MetadataExt::nlink(&meta) == 1;
    #[cfg(not(unix))]
    let single = true;
    if meta.is_file() && single { Ok(()) } else { Err(io::Error::other("not a plain file")) }
}

fn moved(path: &Path) -> SimErrorWire {
    SimErrorWire::Refused(format!("{} changed after it was approved; ask again", path.display()))
}

/// Copy each approved input (a real path inside `root`) into `into` (a
/// private folder), and return the copies, in order. An input that is no
/// longer a plain file inside `root` is refused.
pub(super) fn stage_inputs(root: &Path, inputs: &[PathBuf], into: &Path) -> Result<Vec<PathBuf>, SimErrorWire> {
    let mut staged = Vec::with_capacity(inputs.len());
    for (n, input) in inputs.iter().enumerate() {
        let mut file = open_inside(input, root).map_err(|_| moved(input))?;
        if plain_file(&file).is_err() {
            return Err(SimErrorWire::BadInput(format!("{} is not a plain file", input.display())));
        }
        // A folder each, so two inputs of one name keep it (adb names the
        // device's copy after it).
        let dir = into.join(n.to_string());
        let name = input.file_name().ok_or_else(|| moved(input))?;
        let copy = dir.join(name);
        let written = fs::create_dir(&dir).and_then(|()| io::copy(&mut file, &mut File::create(&copy)?));
        written.map_err(|e| SimErrorWire::Failed(format!("{} was not copied: {e}", input.display())))?;
        staged.push(copy);
    }
    Ok(staged)
}

/// Place what a pull wrote into `from` (a private folder) at `dest` in the
/// worktree: into it when it is a folder already or there were several
/// sources (made when missing), else as it. Returns how many files were
/// placed.
pub(super) fn place_pulled(root: &Path, from: &Path, dest: &Path, sources: usize) -> Result<u32, SimErrorWire> {
    let failed = |e: io::Error| SimErrorWire::Failed(format!("the pulled files were not placed: {e}"));
    let refused = || SimErrorWire::Refused(format!("{} is not a place exec writes to", dest.display()));
    let mut pulled: Vec<PathBuf> = fs::read_dir(from).map_err(failed)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>().map_err(failed)?;
    pulled.sort();
    let into_dir = sources > 1 || fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir());
    let (parent, name) = (dest.parent().ok_or_else(refused)?, dest.file_name().ok_or_else(refused)?);
    let mut placed = 0;
    if into_dir {
        let dir = if fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir()) {
            at::checked_dir(dest, root).map_err(|_| refused())?
        } else {
            if is_git(name) {
                return Err(refused());
            }
            let parent = at::checked_dir(parent, root).map_err(|_| refused())?;
            at::make_dir(&parent, name).and_then(|()| at::open_dir(&parent, name)).map_err(|_| refused())?
        };
        for src in pulled {
            let name = src.file_name().ok_or_else(refused)?.to_owned();
            place(&src, &dir, &name, &mut placed)?;
        }
    } else {
        let parent = at::checked_dir(parent, root).map_err(|_| refused())?;
        for src in pulled {
            place(&src, &parent, name, &mut placed)?;
        }
    }
    Ok(placed)
}

/// Put `src` (from the private folder) in the folder `dir` as `name`.
fn place(src: &Path, dir: &at::Dir, name: &OsStr, placed: &mut u32) -> Result<(), SimErrorWire> {
    let refused = || SimErrorWire::Refused(format!("{} is not a place exec writes to", Path::new(name).display()));
    if is_git(name) || name == ".." || name == "." {
        return Err(refused());
    }
    let failed = |e: io::Error| SimErrorWire::Failed(format!("{} was not placed: {e}", Path::new(name).display()));
    let kind = fs::symlink_metadata(src).map_err(failed)?;
    if kind.is_dir() {
        // Never through a link: a folder of that name that is a link fails.
        let sub = at::make_dir(dir, name).and_then(|()| at::open_dir(dir, name)).map_err(|_| refused())?;
        let mut entries: Vec<PathBuf> = fs::read_dir(src).and_then(|d| d.map(|e| e.map(|e| e.path())).collect()).map_err(failed)?;
        entries.sort();
        for entry in entries {
            let child = entry.file_name().ok_or_else(refused)?.to_owned();
            place(&entry, &sub, &child, placed)?;
        }
    } else if kind.is_file() {
        let mut from = open_plain(src).map_err(failed)?;
        at::replace_file(dir, name, &mut from).map_err(failed)?;
        *placed += 1;
    }
    // Anything else a pull left (a link, a device node) is not placed.
    Ok(())
}

/// A file adb wrote into the private folder, read as a plain file only.
fn open_plain(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    plain_file(&file)?;
    Ok(file)
}

/// `path` opened for reading, never through a final symlink (nor blocking on
/// a named pipe), when the file it opened really lies inside `root` and
/// outside its `.git` folder.
#[cfg(target_os = "macos")]
fn open_inside(path: &Path, root: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    at::check_inside(&at::real_path(&file)?, root)?;
    Ok(file)
}

/// The Mobile Emulator runs on macOS only; nowhere else is a file checked
/// where it really is, so nothing is opened.
#[cfg(not(target_os = "macos"))]
fn open_inside(_path: &Path, _root: &Path) -> io::Result<File> {
    Err(io::Error::other("exec moves files on macOS only"))
}

/// Folder handles and the `*at` calls made relative to them, so a link swapped
/// into the worktree after a folder was checked changes nothing.
#[cfg(target_os = "macos")]
mod at {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    /// An open folder.
    pub(super) struct Dir(OwnedFd);

    fn cname(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::other("a name with a NUL"))
    }

    fn fd(ret: libc::c_int) -> io::Result<OwnedFd> {
        // SAFETY: a descriptor the call just returned, owned by nothing else.
        if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(ret) }) }
    }

    fn done(ret: libc::c_int) -> io::Result<()> {
        if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Where the file or folder behind `fd` really is, whatever path opened it.
    pub(super) fn real_path(fd: &impl AsRawFd) -> io::Result<PathBuf> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: `F_GETPATH` writes at most PATH_MAX bytes, NUL-terminated,
        // into `buf`, which is that long; the descriptor is open for the call.
        done(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) })?;
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(PathBuf::from(OsStr::from_bytes(&buf[..len])))
    }

    /// `real` lies inside `root`, outside its `.git` folder.
    pub(super) fn check_inside(real: &Path, root: &Path) -> io::Result<()> {
        match real.strip_prefix(root) {
            Ok(within) if !super::in_git(within) => Ok(()),
            _ => Err(io::Error::other("outside the worktree")),
        }
    }

    /// The folder at `path`, opened, when it really lies inside `root`.
    pub(super) fn checked_dir(path: &Path, root: &Path) -> io::Result<Dir> {
        let c = cname(path.as_os_str())?;
        // SAFETY: a NUL-terminated path; the result is checked.
        let dir = Dir(fd(unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) })?);
        check_inside(&real_path(&dir.0)?, root)?;
        Ok(dir)
    }

    /// Make the folder `name` in `dir`; one already there is fine.
    pub(super) fn make_dir(dir: &Dir, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: a NUL-terminated name, relative to an open folder.
        match done(unsafe { libc::mkdirat(dir.0.as_raw_fd(), c.as_ptr(), 0o755) }) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }

    /// The folder `name` in `dir`, never through a link.
    pub(super) fn open_dir(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
        let c = cname(name)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: a NUL-terminated name, relative to an open folder.
        Ok(Dir(fd(unsafe { libc::openat(dir.0.as_raw_fd(), c.as_ptr(), flags) })?))
    }

    /// Write `from` as `name` in `dir`: into a scratch file beside it, then
    /// renamed over the name, so a failed copy leaves the old file whole and a
    /// link or a second name is replaced, never written through.
    pub(super) fn replace_file(dir: &Dir, name: &OsStr, from: &mut File) -> io::Result<()> {
        let mut scratch = b".oximux-pull-".to_vec();
        scratch.extend_from_slice(name.as_bytes());
        let (scratch, target) = (cname(OsStr::from_bytes(&scratch))?, cname(name)?);
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: a NUL-terminated name, relative to an open folder.
        let out = fd(unsafe { libc::openat(dir.0.as_raw_fd(), scratch.as_ptr(), flags, 0o644 as libc::c_uint) })?;
        let mut out = File::from(out);
        // SAFETY (both): NUL-terminated names, relative to an open folder.
        let unlink = || unsafe { libc::unlinkat(dir.0.as_raw_fd(), scratch.as_ptr(), 0) };
        let written = io::copy(from, &mut out).and_then(|_| out.sync_all());
        let renamed = written.and_then(|()| done(unsafe { libc::renameat(dir.0.as_raw_fd(), scratch.as_ptr(), dir.0.as_raw_fd(), target.as_ptr()) }));
        if renamed.is_err() {
            unlink();
        }
        renamed
    }
}

/// Elsewhere nothing is placed (see [`open_inside`]).
#[cfg(not(target_os = "macos"))]
mod at {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub(super) struct Dir;

    fn no() -> io::Error {
        io::Error::other("exec moves files on macOS only")
    }

    pub(super) fn checked_dir(_path: &Path, _root: &Path) -> io::Result<Dir> {
        Err(no())
    }

    pub(super) fn make_dir(_dir: &Dir, _name: &OsStr) -> io::Result<()> {
        Err(no())
    }

    pub(super) fn open_dir(_dir: &Dir, _name: &OsStr) -> io::Result<Dir> {
        Err(no())
    }

    pub(super) fn replace_file(_dir: &Dir, _name: &OsStr, _from: &mut File) -> io::Result<()> {
        Err(no())
    }
}
