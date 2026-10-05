//! Per-terminal shell history.
//!
//! Every OxiMux terminal keeps its own Up-arrow history, keyed by its stable
//! `OXIMUX_TAB_ID`: a split starts from a copy of its parent's file, then the
//! two diverge, and a terminal brought back after a restart (same id) reads
//! its own file again. Every command still reaches the user's own history
//! file, so nothing typed in OxiMux is missing from it.
//!
//! Two halves:
//! - [`scripts`]: the shell blocks the integration overlays append after the
//!   user's rc. They point the shell's history at `<dir>/<tab>.<shell>_history`
//!   and tee each new command to the user's file.
//! - File ops ([`inherit`], [`forget`], [`gc`]): what the app does around a
//!   terminal's life. Pure and path-injected, so they test here, GPUI-free.
//!
//! Every path is built from a [`TabId`], which only a lowercase UUID parses
//! into: the id comes from persisted state, and a `../` in it must never reach
//! a path.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Env var naming the directory the per-terminal files live in. Its absence
/// turns the feature off in every shell block.
pub const HISTORY_DIR_ENV: &str = "OXIMUX_HISTORY_DIR";

/// Env var a user sets to `0` to keep every terminal on their own history file.
pub const OPT_OUT_ENV: &str = "OXIMUX_PER_TERMINAL_HISTORY";

/// The terminal identity env var (minted by the app, persisted with the pane).
pub const TAB_ID_ENV: &str = "OXIMUX_TAB_ID";

/// A terminal id that is safe to put in a file name: a lowercase hyphenated
/// UUID, exactly what the app mints. Anything else means "no per-terminal
/// history", in Rust and in every shell block alike.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TabId(String);

impl TabId {
    /// `Some` only for `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in `[0-9a-f]`.
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        let ok = bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        });
        ok.then(|| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fish history session name for this terminal. fish allows only
    /// alphanumerics and `_` there.
    fn fish_session(&self) -> String {
        format!("oximux_{}", self.0.replace('-', "_"))
    }
}

/// The shells whose per-terminal file lives in the history dir itself. fish
/// keeps its history in its own data dir and is reached through a pointer
/// file instead (see [`fish_pointer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryShell {
    Zsh,
    Bash,
}

impl HistoryShell {
    const ALL: [Self; 2] = [Self::Zsh, Self::Bash];

    fn extension(self) -> &'static str {
        match self {
            Self::Zsh => "zsh_history",
            Self::Bash => "bash_history",
        }
    }
}

/// `<dir>/<tab>.<shell>_history`, the file the shell block points HISTFILE at.
pub fn tab_file(dir: &Path, id: &TabId, shell: HistoryShell) -> PathBuf {
    let path = dir.join(format!("{}.{}", id.as_str(), shell.extension()));
    debug_assert_eq!(path.parent(), Some(dir));
    path
}

/// Files a shell leaves beside its tab file: zsh's save-by-copy temp and
/// lock, bash's per-command scratch buffer.
const SIDE_SUFFIXES: [&str; 2] = [".new", ".LOCK"];

/// Where the fish block records the absolute path of this terminal's fish
/// history file. Rust never guesses fish's data dir (the app's environment is
/// not the shell's), so it only ever acts on what fish itself reported.
fn fish_pointer(dir: &Path, id: &TabId) -> PathBuf {
    dir.join(format!("{}.fish_path", id.as_str()))
}

/// The fish history file a pointer names, if it is safe to touch: an absolute
/// path whose file name is exactly this terminal's session file, and which is
/// not a symlink. The pointer sits in the app's owner-only dir, but its
/// target is in the user's fish dir, so the target is checked, never globbed.
fn fish_target(dir: &Path, id: &TabId) -> Option<PathBuf> {
    let text = fs::read_to_string(fish_pointer(dir, id)).ok()?;
    let target = PathBuf::from(text.trim_end_matches(['\n', '\r']));
    let expected = format!("{}_history", id.fish_session());
    let named_right = target.file_name().and_then(|n| n.to_str()) == Some(expected.as_str());
    if !target.is_absolute() || !named_right {
        return None;
    }
    match fs::symlink_metadata(&target) {
        Ok(meta) if !meta.file_type().is_file() => None,
        _ => Some(target),
    }
}

/// Seed `child`'s history from `parent`'s, before the child's shell starts.
///
/// Copies each shell's file the parent has (zsh, bash, and fish through its
/// pointer). The child file is created with `O_EXCL`, so an existing file or a
/// planted symlink is never written through; a parent without history is a
/// no-op and the child's shell seeds itself from the user's file.
pub fn inherit(dir: &Path, parent: &TabId, child: &TabId) -> io::Result<()> {
    if parent == child {
        return Ok(());
    }
    for shell in HistoryShell::ALL {
        copy_new(&tab_file(dir, parent, shell), &tab_file(dir, child, shell))?;
    }
    if let Some(src) = fish_target(dir, parent)
        && let Some(fish_dir) = src.parent()
    {
        let dst = fish_dir.join(format!("{}_history", child.fish_session()));
        if copy_new(&src, &dst)? {
            if let Err(err) = backdate_recent_fish_items(&dst, SystemTime::now()) {
                // Same as a torn copy: better none than a half-written one.
                let _ = fs::remove_file(&dst);
                return Err(err);
            }
            // Record it now, so closing the child before its shell got to
            // write the pointer still finds the file to delete.
            let mut pointer = create_new(&fish_pointer(dir, child))?;
            io::Write::write_all(&mut pointer, format!("{}\n", dst.display()).as_bytes())?;
        }
    }
    Ok(())
}

/// fish hides history entries stamped in the second its session started (it
/// takes them for another session's), and a split spawns right after the
/// copy. Stamp the copy's newest entries two seconds back so they show.
fn backdate_recent_fish_items(path: &Path, now: SystemTime) -> io::Result<()> {
    let now = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let floor = now.saturating_sub(2);
    let text = fs::read(path)?;
    let mut changed = false;
    let mut out = Vec::with_capacity(text.len());
    for line in text.split_inclusive(|&b| b == b'\n') {
        let stamp = std::str::from_utf8(line)
            .ok()
            .and_then(|l| l.strip_prefix("  when: "))
            .and_then(|rest| rest.trim_end().parse::<u64>().ok());
        match stamp {
            Some(when) if when > floor => {
                out.extend_from_slice(format!("  when: {floor}\n").as_bytes());
                changed = true;
            }
            _ => out.extend_from_slice(line),
        }
    }
    if changed { fs::write(path, out) } else { Ok(()) }
}

/// Delete everything this terminal's history left behind. Missing files are
/// fine; the first other error is returned after every removal was tried.
pub fn forget(dir: &Path, id: &TabId) -> io::Result<()> {
    forget_files(dir, id, true)
}

/// [`forget`], but keep the fish pointer: the first of the two passes after a
/// close. fish may write its history once more as it exits, and the second
/// pass can only find that file through the pointer.
pub fn forget_keeping_fish_pointer(dir: &Path, id: &TabId) -> io::Result<()> {
    forget_files(dir, id, false)
}

fn forget_files(dir: &Path, id: &TabId, drop_pointer: bool) -> io::Result<()> {
    let mut paths = Vec::new();
    for shell in HistoryShell::ALL {
        let file = tab_file(dir, id, shell);
        for suffix in SIDE_SUFFIXES {
            let mut side = file.clone().into_os_string();
            side.push(suffix);
            paths.push(PathBuf::from(side));
        }
        paths.push(file);
    }
    let mut first_err = None;
    // The fish file first: if it cannot go, its pointer stays so a later
    // pass can still find it.
    let mut fish_left = false;
    if let Some(target) = fish_target(dir, id)
        && let Err(err) = fs::remove_file(&target)
        && err.kind() != io::ErrorKind::NotFound
    {
        fish_left = true;
        first_err = Some(err);
    }
    if drop_pointer && !fish_left {
        paths.push(fish_pointer(dir, id));
    }
    for path in paths {
        match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                first_err.get_or_insert(err);
            }
            _ => {}
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Remove the history of terminals that no longer exist: ids not in
/// `referenced` whose files all went untouched for `max_age`.
///
/// Only names of the form `<uuid>.<known suffix>` are considered; anything
/// else in the dir is left alone. The caller must pass the complete set of
/// live ids: an incomplete set deletes live history, so a caller that could
/// not read every persisted layout should skip the sweep rather than call
/// this. Returns how many terminals were forgotten.
pub fn gc(
    dir: &Path,
    referenced: &HashSet<TabId>,
    max_age: Duration,
    now: SystemTime,
) -> io::Result<usize> {
    // id -> newest mtime among its files.
    let mut newest: HashMap<TabId, SystemTime> = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(owning_tab) else {
            continue;
        };
        if referenced.contains(&id) {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        let at = newest.entry(id).or_insert(mtime);
        *at = (*at).max(mtime);
    }
    let mut forgotten = 0;
    for (id, at) in newest {
        let age = now.duration_since(at).unwrap_or_default();
        if age >= max_age {
            forget(dir, &id)?;
            forgotten += 1;
        }
    }
    Ok(forgotten)
}

/// The terminal a history-dir file belongs to, if its name is one this
/// module writes.
fn owning_tab(name: &str) -> Option<TabId> {
    let (id, rest) = name.split_at_checked(36)?;
    let id = TabId::parse(id)?;
    let known = rest == ".fish_path"
        || HistoryShell::ALL.iter().any(|shell| {
            rest.strip_prefix('.')
                .and_then(|r| r.strip_prefix(shell.extension()))
                .is_some_and(|side| side.is_empty() || SIDE_SUFFIXES.contains(&side))
        });
    known.then_some(id)
}

/// Copy `src` to a new `dst`. `Ok(false)` when there is nothing to copy (no
/// source, or it is not a regular file) or `dst` already exists.
fn copy_new(src: &Path, dst: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(src) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    }
    let mut input = File::open(src)?;
    let mut out = match create_new(dst) {
        Ok(out) => out,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => return Err(err),
    };
    if let Err(err) = io::copy(&mut input, &mut out) {
        // A torn copy would shadow the user's history; without it the child
        // shell seeds from the user's file instead.
        let _ = fs::remove_file(dst);
        return Err(err);
    }
    Ok(true)
}

/// `O_CREAT | O_EXCL`, owner-only: refuses an existing path, symlink included.
fn create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// The shell blocks, appended to each integration overlay after the user's rc
/// (and after the OSC 133 hooks). Kept as real shell files so they can be read
/// and linted as what they are. Each one re-validates `OXIMUX_TAB_ID` itself
/// and stays off (today's shared history) when anything it needs is missing.
pub mod scripts {
    /// zsh: zsh writes the tab file per command; `preexec`/`precmd` copy the
    /// new bytes to the user's HISTFILE (`zsh/system`, no fork).
    pub const ZSH_BLOCK: &str = include_str!("history/per_terminal.zsh");
    /// bash: `history -a` into a scratch file each prompt, then `tee` it to
    /// both files.
    pub const BASH_BLOCK: &str = include_str!("history/per_terminal.bash");
    /// fish >= 4.0: a per-terminal history session, with each kept command
    /// `history append`ed to the user's session.
    pub const FISH_BLOCK: &str = include_str!("history/per_terminal.fish");
}

#[cfg(test)]
#[path = "history/tests.rs"]
mod tests;
