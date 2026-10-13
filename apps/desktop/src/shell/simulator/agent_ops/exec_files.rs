//! The host side of `sim exec`'s adb transfers, done so that adb touches only
//! what the user approved: it never opens a worktree path itself.
//!
//! The question shows the worktree paths, but the user may answer up to a
//! minute and a half later, and adb resolves a path string again when it runs,
//! following any symlink an agent put there meanwhile. So after the answer an
//! input (`install`, `push`) is opened without following a final symlink, its
//! real path checked inside the worktree, and copied into a private folder
//! that adb reads instead. A `pull` writes into a private folder too, and its
//! files are then placed in the worktree the same way: each opened without
//! following a symlink and checked where it really landed, as a plain file
//! (no execute bit), never under a `.git` folder.

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
        let mut file = open_inside(input, root, OpenOptions::new().read(true), false).map_err(|_| moved(input))?;
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
/// sources, else as it. Returns how many files were placed.
pub(super) fn place_pulled(root: &Path, from: &Path, dest: &Path, sources: usize) -> Result<u32, SimErrorWire> {
    let failed = |e: io::Error| SimErrorWire::Failed(format!("the pulled files were not placed: {e}"));
    let mut pulled: Vec<PathBuf> = fs::read_dir(from).map_err(failed)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>().map_err(failed)?;
    pulled.sort();
    let into_dir = sources > 1 || fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir());
    let mut placed = 0;
    for src in pulled {
        let to = match src.file_name() {
            Some(name) if into_dir => dest.join(name),
            _ => dest.to_path_buf(),
        };
        place(root, &src, &to, &mut placed)?;
    }
    Ok(placed)
}

fn place(root: &Path, src: &Path, dest: &Path, placed: &mut u32) -> Result<(), SimErrorWire> {
    let refused = || SimErrorWire::Refused(format!("{} is not a place exec writes to", dest.display()));
    if in_git(dest.strip_prefix(root).map_err(|_| refused())?) {
        return Err(refused());
    }
    let kind = fs::symlink_metadata(src).map_err(|e| SimErrorWire::Failed(format!("{}: {e}", src.display())))?;
    if kind.is_dir() {
        make_dir_inside(dest, root).map_err(|_| refused())?;
        let mut entries: Vec<PathBuf> = fs::read_dir(src).and_then(|d| d.map(|e| e.map(|e| e.path())).collect()).map_err(|e| SimErrorWire::Failed(e.to_string()))?;
        entries.sort();
        for entry in entries {
            let name = entry.file_name().ok_or_else(refused)?.to_owned();
            place(root, &entry, &dest.join(name), placed)?;
        }
    } else if kind.is_file() {
        let mut out = create_inside(dest, root).map_err(|_| refused())?;
        let copied = open_plain(src).and_then(|mut f| io::copy(&mut f, &mut out));
        copied.map_err(|e| SimErrorWire::Failed(format!("{} was not written: {e}", dest.display())))?;
        *placed += 1;
    }
    // Anything else a pull left (a link, a device node) is not placed.
    Ok(())
}

/// `dest` as a folder inside `root`: kept when it is one, else made. A
/// folder this made outside (through a link swapped in meanwhile) is removed.
fn make_dir_inside(dest: &Path, root: &Path) -> io::Result<()> {
    let outside = || io::Error::other("outside the worktree");
    let made = match fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => false,
        Ok(_) => return Err(outside()),
        Err(_) => {
            fs::create_dir(dest)?;
            true
        }
    };
    let real = fs::canonicalize(dest)?;
    if real.starts_with(root) {
        return Ok(());
    }
    if made {
        let _ = fs::remove_dir(&real);
    }
    Err(outside())
}

/// `dest` opened for writing, emptied, as a plain file inside `root`. A file
/// this creates outside (through a link swapped in meanwhile) is removed,
/// and an existing one outside is never opened for long, nor emptied.
fn create_inside(dest: &Path, root: &Path) -> io::Result<File> {
    let mut new = OpenOptions::new();
    new.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut new, 0o644);
    match open_inside(dest, root, &mut new, true) {
        Ok(file) => return Ok(file),
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    let file = open_inside(dest, root, OpenOptions::new().write(true), false)?;
    plain_file(&file)?;
    file.set_len(0)?;
    // What a pull writes is never executable, a new file or an old one.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    Ok(file)
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

/// `path` opened with `options`, never through a final symlink, when the file
/// it opened really lies inside `root`. `created`: the options create a new
/// file, which is removed again when it landed elsewhere.
#[cfg(target_os = "macos")]
fn open_inside(path: &Path, root: &Path, options: &mut OpenOptions, created: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    // Non-blocking: opening a named pipe would otherwise wait for a writer
    // for good. A plain file (all that is used) reads and writes as usual.
    let file = options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let real = real_path(&file)?;
    if real.starts_with(root) {
        return Ok(file);
    }
    drop(file);
    if created {
        let _ = fs::remove_file(&real);
    }
    Err(io::Error::other("outside the worktree"))
}

/// Where the file behind `file` really is, whatever path opened it.
#[cfg(target_os = "macos")]
fn real_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `F_GETPATH` writes at most PATH_MAX bytes, NUL-terminated, into
    // `buf`, which is that long; the descriptor is open for the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
}

/// The Mobile Emulator runs on macOS only; nowhere else is a file checked
/// where it really landed, so nothing is opened.
#[cfg(not(target_os = "macos"))]
fn open_inside(_path: &Path, _root: &Path, _options: &mut OpenOptions, _created: bool) -> io::Result<File> {
    Err(io::Error::other("exec moves files on macOS only"))
}
