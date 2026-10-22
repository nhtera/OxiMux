//! Read-only terminals: a lock that keeps every OxiMux input path from
//! reaching a program — keys, paste, dictation, insert-path, mouse reports,
//! text sent to an agent — from any view of it (pane, floating, torn-off) and
//! from a paired phone or `oximux term send` routed through this desktop.
//!
//! Keyed by the terminal's stable tab id (`OXIMUX_TAB_ID`), which every view of
//! one terminal shares and which survives a restart — the daemon PTY id does
//! not, on a cold restore. The PTY ids currently behind locked tabs are kept
//! too, for the remote terminal source, since remote input names only the PTY.
//!
//! Process-wide rather than a GPUI global: the remote source is built before
//! any app context exists, and persistence snapshots run without one.
//!
//! The PTY ids are also kept on disk ([`persist_to`]): a locked terminal's PTY
//! outlives the app, and after a restart nothing else knows to refuse remote
//! input to it until its project is opened and its view adopts the PTY again.
//!
//! Limits (documented): a headless `oximux serve` has no UI to set a lock, and
//! a same-user process talking to the relay socket directly bypasses the host.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

/// PTY ids whose terminals are read-only, shared with the remote host.
pub type LockedPtys = Arc<RwLock<HashSet<String>>>;

struct Locks {
    tabs: RwLock<HashSet<String>>,
    ptys: LockedPtys,
    /// Where the PTY set is kept between runs. Unset in tests, so they never
    /// touch a real data directory.
    store: OnceLock<PathBuf>,
}

fn locks() -> &'static Locks {
    static LOCKS: OnceLock<Locks> = OnceLock::new();
    LOCKS.get_or_init(|| Locks {
        tabs: RwLock::default(),
        ptys: LockedPtys::default(),
        store: OnceLock::new(),
    })
}

/// Load the PTY ids a previous run locked, and keep the file current from now
/// on. Called once at boot, before the remote terminal source serves anything.
/// Ids of PTYs that have since ended are harmless: PTY ids are never reused.
pub fn persist_to(path: PathBuf) {
    let l = locks();
    l.ptys.write().unwrap_or_else(|p| p.into_inner()).extend(read_ids(&path));
    let _ = l.store.set(path);
}

/// Write the PTY set out, called with it held so writes land in order.
fn save(ptys: &HashSet<String>) {
    let Some(path) = locks().store.get() else { return };
    if let Err(err) = write_ids(path, ptys) {
        tracing::warn!(?err, "could not save the read-only terminal list");
    }
}

/// One id per line. A missing or unreadable file is an empty set.
fn read_ids(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned).collect()
}

fn write_ids(path: &std::path::Path, ids: &HashSet<String>) -> std::io::Result<()> {
    let mut ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    ids.sort_unstable();
    std::fs::write(path, ids.join("\n"))
}

/// The shared PTY set, for the remote terminal source.
pub fn shared_locked_ptys() -> LockedPtys {
    Arc::clone(&locks().ptys)
}

/// Whether the terminal with stable tab id `tab_id` is read-only.
pub fn is_locked(tab_id: &str) -> bool {
    !tab_id.is_empty() && locks().tabs.read().unwrap_or_else(|p| p.into_inner()).contains(tab_id)
}

/// Lock or unlock a terminal. `pty_id` is the daemon PTY behind it now, if it
/// has one.
pub fn set(tab_id: &str, pty_id: Option<&str>, locked: bool) {
    if tab_id.is_empty() {
        return;
    }
    let l = locks();
    let mut tabs = l.tabs.write().unwrap_or_else(|p| p.into_inner());
    let mut ptys = l.ptys.write().unwrap_or_else(|p| p.into_inner());
    let changed = if locked {
        tabs.insert(tab_id.to_owned());
        pty_id.is_some_and(|pty| ptys.insert(pty.to_owned()))
    } else {
        tabs.remove(tab_id);
        pty_id.is_some_and(|pty| ptys.remove(pty))
    };
    if changed {
        save(&ptys);
    }
}

/// A locked terminal now runs on `pty_id` (a restore re-attached it, or a
/// respawn replaced it): lock that PTY for remote input too.
pub fn bind_pty(tab_id: &str, pty_id: &str) {
    if is_locked(tab_id) {
        let mut ptys = locks().ptys.write().unwrap_or_else(|p| p.into_inner());
        if ptys.insert(pty_id.to_owned()) {
            save(&ptys);
        }
    }
}

/// Whether `pty_id` is read-only, for a holder of the shared set.
pub fn pty_is_locked(ptys: &LockedPtys, pty_id: &str) -> bool {
    ptys.read().unwrap_or_else(|p| p.into_inner()).contains(pty_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_covers_the_tab_and_its_pty_and_survives_a_rebind() {
        let ptys = shared_locked_ptys();
        assert!(!is_locked("locks-test-tab"));
        set("locks-test-tab", Some("locks-test-pty-a"), true);
        assert!(is_locked("locks-test-tab"));
        assert!(pty_is_locked(&ptys, "locks-test-pty-a"));
        // Restored onto a new PTY: that one is locked too.
        bind_pty("locks-test-tab", "locks-test-pty-b");
        assert!(pty_is_locked(&ptys, "locks-test-pty-b"));
        // An unlocked tab binds nothing.
        bind_pty("locks-test-other", "locks-test-pty-c");
        assert!(!pty_is_locked(&ptys, "locks-test-pty-c"));
        set("locks-test-tab", Some("locks-test-pty-b"), false);
        assert!(!is_locked("locks-test-tab"));
        assert!(!pty_is_locked(&ptys, "locks-test-pty-b"));
        // An empty id (no stable identity) is never locked.
        set("", None, true);
        assert!(!is_locked(""));
    }

    #[test]
    fn the_locked_pty_list_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked-terminals");
        assert!(read_ids(&path).is_empty(), "no file yet is an empty set");
        let ids: HashSet<String> = ["pty-b", "pty-a"].map(String::from).into();
        write_ids(&path, &ids).unwrap();
        assert_eq!(read_ids(&path), ["pty-a", "pty-b"]);
    }
}
