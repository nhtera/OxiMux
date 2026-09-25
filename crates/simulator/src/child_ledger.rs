//! A crash-safe record of processes this app spawned on the simulator's
//! behalf — the helper and `simctl io recordVideo` — so a crash or `kill -9`
//! of the app itself doesn't leave them running forever.
//!
//! The ledger is one JSON file (`Ledger::open`'s caller decides where, e.g.
//! `data_dir/simulator/children.json`), guarded by an `fd-lock` exclusive
//! lock on a sibling `.lock` file and written with a tmp-file-then-`rename`
//! so a crash mid-write never corrupts it (a reader sees either the old
//! content or the new, never a half-written one). At startup, [`reap_stale`]
//! walks whatever the ledger says survived the previous run and kills
//! anything still there — see its docs for how it tells "our orphan" from
//! "the kernel reused this pid for something else", which matters more than
//! it sounds: **never trust a process name for that**, only the argument
//! vector it was actually launched with (see
//! [`oximux_proc_tree`]'s own docs on why `name` is weak evidence).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use fd_lock::RwLock as FdLock;
use serde::{Deserialize, Serialize};

use crate::{Result, SimError};

/// What kind of child a ledger [`Entry`] is, which decides how [`reap_stale`]
/// asks it to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `oximux-sim-helper`: a well-behaved stdio child that exits the moment
    /// its stdin closes, so `SIGTERM` (then `SIGKILL` after a short wait) is
    /// plenty.
    Helper,
    /// `simctl io <udid> recordVideo`: writes a video file that must be
    /// flushed and finalized, so it gets `SIGINT` (what `Ctrl-C` sends,
    /// `simctl`'s own documented way to stop a recording cleanly) and a
    /// longer grace period before `SIGKILL`.
    Record,
}

/// One process this app spawned and is responsible for cleaning up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pid: u32,
    pub kind: Kind,
    /// The path the child was actually launched with — compared against the
    /// live process's `argv[0]` by [`reap_stale`], never its name.
    pub exe: PathBuf,
    pub started_at_unix: u64,
    /// The simulator this child belongs to, when it's tied to one (both
    /// [`Kind`]s normally are; `None` is just defensive slack).
    pub udid: Option<String>,
    /// The OxiMux process that spawned the child. [`reap_stale`] leaves
    /// entries alone while their owner is still running (another instance
    /// sharing the data dir). `0` (ledgers written before this field) means
    /// unknown, which is treated as a dead owner.
    #[serde(default)]
    pub owner_pid: u32,
}

/// A JSON-file-backed set of [`Entry`], safe to share across processes (the
/// lock, not just in-process) and across threads within one.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    lock_path: PathBuf,
}

impl Ledger {
    /// Opens the ledger at `path`, creating its parent directory if needed.
    /// Does not require `path` itself to exist yet — a fresh ledger reads as
    /// empty until the first [`Ledger::record`].
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lock_path = sibling_with_suffix(&path, ".lock");
        Ok(Self { path, lock_path })
    }

    /// Adds `entry`, replacing any existing entry for the same pid.
    pub fn record(&self, entry: Entry) -> Result<()> {
        self.with_lock(|entries| {
            entries.retain(|e| e.pid != entry.pid);
            entries.push(entry);
        })
    }

    /// Removes the entry for `pid`, if any. Not an error if it's already
    /// gone — callers use this after they cleanly stopped a child they
    /// started, and a double-remove (e.g. a retried cleanup) is normal.
    pub fn remove(&self, pid: u32) -> Result<()> {
        self.with_lock(|entries| entries.retain(|e| e.pid != pid))
    }

    /// A snapshot of every entry currently recorded.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        read_entries(&self.path)
    }

    /// Runs `mutate` against the current entries while holding the exclusive
    /// lock, then writes the result back atomically. The lock is held for the
    /// whole read-mutate-write cycle, so two threads (or processes) calling
    /// this concurrently never lose one's write to the other's.
    fn with_lock<R>(&self, mutate: impl FnOnce(&mut Vec<Entry>) -> R) -> Result<R> {
        let file = fs::OpenOptions::new().create(true).write(true).truncate(false).open(&self.lock_path)?;
        let mut lock = FdLock::new(file);
        let _guard = lock.write()?;
        let mut entries = read_entries(&self.path)?;
        let result = mutate(&mut entries);
        write_entries_atomic(&self.path, &entries)?;
        Ok(result)
    }
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn read_entries(path: &Path) -> Result<Vec<Entry>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| SimError::Parse { what: "child ledger".into(), detail: e.to_string() })
}

/// Writes `entries` to `path` via a temp file in the same directory (so the
/// final `rename` is on one filesystem and therefore atomic) plus `fsync`, so
/// a crash between the write and the rename leaves the old file intact
/// instead of a truncated one.
fn write_entries_atomic(path: &Path, entries: &[Entry]) -> Result<()> {
    let json = serde_json::to_vec_pretty(entries)
        .map_err(|e| SimError::Parse { what: "child ledger".into(), detail: e.to_string() })?;
    let tmp_path = sibling_with_suffix(path, &format!(".tmp.{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// What [`reap_stale`] did with the ledger it found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Pids that were alive and matched their recorded `exe`: signalled (and
    /// force-killed if they outlasted the grace period).
    pub killed: Vec<u32>,
    /// Pids dropped without signalling: already dead, or alive under a
    /// different program (the kernel reused the pid).
    pub dropped: Vec<u32>,
}

/// Processes every entry left over from a previous run: kills the ones that
/// are still alive under the program they were recorded as, drops the rest,
/// and leaves the ledger empty either way — a fresh run starts from a clean
/// slate, which is the point of calling this once at startup.
///
/// Identity is decided by [`oximux_proc_tree::argv_of_pid`]'s `argv[0]`
/// against [`Entry::exe`], **never** by the process name: a name is the
/// *resolved* binary the kernel reports and has been observed stale, while a
/// pid can also simply have been reassigned by the kernel to an unrelated
/// program since the entry was recorded. Matching by name risks killing
/// whatever now happens to occupy that pid.
///
/// Entries whose `owner_pid` is another live process are kept untouched:
/// they belong to a running instance, not an orphaned one.
///
/// The claimed entries leave the ledger under the lock, but the kills (up to
/// 5 s each for a recording) happen after it is released, so a concurrent
/// `record` never waits on them.
///
/// Best-effort: a ledger I/O failure (an unreadable lock file, corrupt JSON)
/// is swallowed and reported as an empty [`ReapReport`] rather than
/// panicking a startup path over stale bookkeeping.
pub fn reap_stale(ledger: &Ledger) -> ReapReport {
    let me = std::process::id();
    let claimed = ledger
        .with_lock(|entries| {
            let (orphans, owned): (Vec<Entry>, Vec<Entry>) =
                entries.drain(..).partition(|e| !owner_is_alive(e.owner_pid, me));
            *entries = owned;
            orphans
        })
        .unwrap_or_default();
    let mut report = ReapReport::default();
    for entry in claimed {
        if matches_recorded_exe(entry.pid, &entry.exe) {
            kill_entry(&entry);
            report.killed.push(entry.pid);
        } else {
            report.dropped.push(entry.pid);
        }
    }
    report
}

/// Whether `owner` is a different, still-running process. Our own pid counts
/// as dead: anything we recorded in a previous life under a reused pid is
/// ours to reap, and this instance has spawned nothing yet at startup.
fn owner_is_alive(owner: u32, me: u32) -> bool {
    owner != 0 && owner != me && oximux_proc_tree::process(owner).is_some()
}

fn matches_recorded_exe(pid: u32, exe: &Path) -> bool {
    let Some(argv) = oximux_proc_tree::argv_of_pid(pid) else {
        return false;
    };
    argv.first().map(Path::new) == Some(exe)
}

#[cfg(unix)]
fn kill_entry(entry: &Entry) {
    use std::time::{Duration, Instant};

    let (first_signal, grace) = match entry.kind {
        Kind::Record => (libc::SIGINT, Duration::from_secs(5)),
        Kind::Helper => (libc::SIGTERM, Duration::from_secs(1)),
    };
    send_signal(entry.pid, first_signal);

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if oximux_proc_tree::process(entry.pid).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if oximux_proc_tree::process(entry.pid).is_some() {
        send_signal(entry.pid, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) {
    // SAFETY: `kill(2)` with a pid this ledger recorded and a standard
    // signal number. A failure here (the process already gone, or — should
    // pid reuse somehow slip past `matches_recorded_exe`'s check right
    // before this call — a permission error) is not fatal to reaping the
    // rest of the ledger.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// No `libc` dependency off Unix (see this crate's `Cargo.toml`): reaping on
/// other platforms is limited to pruning the ledger via
/// `matches_recorded_exe`, which is already `false` for every entry there
/// since [`oximux_proc_tree::argv_of_pid`] has no Windows implementation yet.
#[cfg(not(unix))]
fn kill_entry(_entry: &Entry) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("children.json")).unwrap();
        (dir, ledger)
    }

    fn entry(pid: u32, exe: &str) -> Entry {
        Entry { pid, kind: Kind::Helper, exe: PathBuf::from(exe), started_at_unix: 0, udid: None, owner_pid: 0 }
    }

    #[test]
    fn round_trips_record_remove_and_entries() {
        let (_dir, ledger) = temp_ledger();
        assert_eq!(ledger.entries().unwrap(), Vec::new());

        ledger.record(entry(111, "/bin/sleep")).unwrap();
        ledger.record(entry(222, "/bin/cat")).unwrap();
        let mut pids: Vec<u32> = ledger.entries().unwrap().iter().map(|e| e.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![111, 222]);

        ledger.remove(111).unwrap();
        assert_eq!(ledger.entries().unwrap(), vec![entry(222, "/bin/cat")]);

        // Removing an already-gone pid is a no-op, not an error.
        ledger.remove(111).unwrap();
    }

    #[test]
    fn record_replaces_an_existing_pid() {
        let (_dir, ledger) = temp_ledger();
        ledger.record(entry(111, "/bin/sleep")).unwrap();
        ledger.record(entry(111, "/bin/cat")).unwrap();
        let entries = ledger.entries().unwrap();
        assert_eq!(entries, vec![entry(111, "/bin/cat")]);
    }

    #[test]
    fn concurrent_record_from_two_threads_loses_nothing() {
        let (_dir, ledger) = temp_ledger();
        let ledger = Arc::new(ledger);
        let mut threads = Vec::new();
        for base in [0u32, 1000] {
            let ledger = Arc::clone(&ledger);
            threads.push(std::thread::spawn(move || {
                for i in 0..50 {
                    ledger.record(entry(base + i, "/bin/sleep")).unwrap();
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(ledger.entries().unwrap().len(), 100);
    }

    /// Spawns a real `/bin/sleep 30` (never a copied binary — see this
    /// repo's exec-of-a-copy trap) and confirms `reap_stale` kills it via the
    /// ledger, while an entry whose pid is alive but recorded under a
    /// different executable (this test process's own pid, under a bogus
    /// path) is left alone.
    #[test]
    #[cfg(unix)]
    fn reap_stale_leaves_entries_of_a_live_owner_alone() {
        let (_dir, ledger) = temp_ledger();
        // Stand-in for another running OxiMux instance sharing the data dir.
        let mut owner = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let child_of_other = Entry { owner_pid: owner.id(), ..entry(u32::MAX - 1, "/bin/sleep") };
        ledger.record(child_of_other.clone()).unwrap();

        let report = reap_stale(&ledger);
        assert!(report.killed.is_empty() && report.dropped.is_empty(), "{report:?}");
        assert_eq!(ledger.entries().unwrap(), vec![child_of_other]);

        let _ = owner.kill();
        let _ = owner.wait();
        // Owner gone: now it is an orphan (dead pid, so just pruned).
        assert_eq!(reap_stale(&ledger).dropped, vec![u32::MAX - 1]);
        assert!(ledger.entries().unwrap().is_empty());
    }

    #[test]
    fn reap_stale_kills_matching_orphans_and_spares_mismatches() {
        let (_dir, ledger) = temp_ledger();

        let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let sleep_pid = child.id();
        // We are `sleep`'s real parent in this test (unlike production,
        // where the orphan's original parent is a previous, now-dead, app
        // instance): reap it off a background thread so it doesn't linger as
        // a zombie and make the liveness poll below spin for no reason.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        ledger.record(entry(sleep_pid, "/bin/sleep")).unwrap();
        let our_pid = std::process::id();
        ledger.record(entry(our_pid, "/definitely/not/our/real/exe")).unwrap();

        let report = reap_stale(&ledger);
        assert_eq!(report.killed, vec![sleep_pid]);
        assert_eq!(report.dropped, vec![our_pid]);
        assert!(ledger.entries().unwrap().is_empty());

        // We must still be here to observe this.
        assert!(oximux_proc_tree::process(our_pid).is_some());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && oximux_proc_tree::process(sleep_pid).is_some() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(oximux_proc_tree::process(sleep_pid).is_none(), "sleep should have been killed");
    }
}
