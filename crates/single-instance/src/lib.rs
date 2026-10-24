//! Which process owns a per-data-directory role, decided by an advisory lock.
//!
//! Two OxiMux processes pointed at the same on-disk data directory must not
//! both perform certain roles: two GUI instances race the per-window layout
//! store and both attach the same relay PTYs, and two schedule tickers would
//! double-fire every due schedule. This crate answers "who owns the role" with
//! a non-blocking exclusive lock (`flock` on unix, `LockFileEx` on Windows,
//! both via `fd-lock`) on a role-named file in the data directory, held for
//! the process lifetime by keeping the open file alive inside the returned
//! guard. The OS releases it when the process exits — including a crash or
//! SIGKILL — so a dead holder never strands a stale lock, which is the failure
//! mode a bare pidfile has.
//!
//! What a contender does about a live holder is the caller's business: the GUI
//! brings the holder's window forward and exits, the schedule ticker declines
//! to tick and keeps serving everything else.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// Where the holder records its PID, beside the lock it holds.
///
/// A separate file rather than the lock file's contents, because `LockFileEx`
/// byte-range locks on Windows are **mandatory**, not advisory like `flock`:
/// while the holder's lock is live, any read through a different handle fails
/// with `ERROR_LOCK_VIOLATION`. A contender reading the PID is by definition
/// reading while someone holds the lock, so writing it inside the locked file
/// made `holder_pid` unconditionally `None` on Windows — the lock worked,
/// nothing could be told who owned it, and a second launch went silent instead
/// of raising the running window.
///
/// This does not reintroduce the stale-pidfile problem the crate doc rejects.
/// Liveness is still decided entirely by the lock; the PID is only consulted on
/// the path where the lock was already lost, which means a holder is alive to
/// have written it. A PID left behind by a crash is unreachable, because the
/// next launch takes the lock that crash released and rewrites the file.
fn pid_path_for(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("pid")
}

/// Seconds two readings of one process's start time may differ by and still
/// name the same process (the kernels report whole seconds; allow rounding).
const START_SLACK_S: u64 = 2;

/// Seconds two readings of the boot instant may differ by within one boot
/// (exact on macOS and Linux; Windows derives it from the uptime counter).
const BOOT_SLACK_S: u64 = 5;

/// What a holder writes beside its lock: who it is, when it started, which
/// boot it ran in, and the role it holds the lock for.
///
/// The lock alone answers "is someone holding this?". The record answers the
/// questions the lock cannot: whether a pid read later still names the
/// process that wrote it (pids are recycled), and, after this process wins a
/// lock someone else held, whether that holder crashed or the machine
/// rebooted in between. It carries no paths and no secrets.
///
/// On disk: four lines, `pid` first so a reader that only knows the old
/// one-line format still gets the pid. An unknown field is an empty line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolderRecord {
    pub pid: u32,
    /// When the holder process started, whole seconds since the Unix epoch.
    pub started_at_unix_s: Option<u64>,
    /// When the machine booted, as the holder saw it.
    pub boot_time_unix_s: Option<u64>,
    /// The caller-chosen role name (for example `desktop` or `serve`).
    pub role: Option<String>,
}

impl HolderRecord {
    /// The record this process writes on winning a lock.
    fn current(role: Option<&str>) -> Self {
        let pid = std::process::id();
        Self {
            pid,
            started_at_unix_s: oximux_proc_tree::start_time_of_pid(pid),
            boot_time_unix_s: oximux_proc_tree::boot_time_of_system(),
            role: role.map(str::to_owned),
        }
    }

    fn encode(&self) -> String {
        let opt = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_default();
        format!(
            "{}\n{}\n{}\n{}\n",
            self.pid,
            opt(self.started_at_unix_s),
            opt(self.boot_time_unix_s),
            self.role.as_deref().unwrap_or_default()
        )
    }

    /// Parse a record. Only the pid line is required, so a one-line file from
    /// an older build reads as a record with every other field unknown.
    fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines().map(str::trim);
        let pid = lines.next()?.parse().ok()?;
        let mut num = || lines.next().and_then(|l| l.parse().ok());
        let started_at_unix_s = num();
        let boot_time_unix_s = num();
        let role = lines.next().filter(|l| !l.is_empty()).map(str::to_owned);
        Some(Self { pid, started_at_unix_s, boot_time_unix_s, role })
    }

    /// Whether this record was written during the machine's current boot.
    /// `None` when either side is unknown, which a caller must treat as
    /// "can't tell", never as "same boot".
    pub fn same_boot(&self) -> Option<bool> {
        let then = self.boot_time_unix_s?;
        let now = oximux_proc_tree::boot_time_of_system()?;
        Some(then.abs_diff(now) <= BOOT_SLACK_S)
    }

    /// Whether the process that wrote this record is still running: its pid
    /// is alive, started when the record says, in this boot. Any unknown
    /// field answers `false`, because a recycled pid looks alive too.
    pub fn is_live(&self) -> bool {
        let Some(recorded) = self.started_at_unix_s else {
            return false;
        };
        let Some(actual) = oximux_proc_tree::start_time_of_pid(self.pid) else {
            return false;
        };
        recorded.abs_diff(actual) <= START_SLACK_S && self.same_boot() == Some(true)
    }
}

/// Held for the whole process lifetime. Dropping it (or process exit) closes
/// the underlying descriptor, which releases the advisory lock.
pub struct SingleInstanceGuard {
    // The lock lives on the open file description (unix) / handle (Windows);
    // keeping the `File` alive is the entire mechanism. The field is never read
    // — it is held for its Drop, which closes the file and frees the lock.
    _lock: fd_lock::RwLock<std::fs::File>,
    previous: Option<HolderRecord>,
}

impl SingleInstanceGuard {
    /// The record the previous holder left beside the lock, read before this
    /// process overwrote it. `None` for a fresh data directory or an
    /// unreadable record.
    ///
    /// Winning the lock proves that holder is gone (the OS released its lock),
    /// so this is the evidence for "the previous host died" and, through
    /// [`HolderRecord::same_boot`], for "the machine rebooted".
    pub fn previous(&self) -> Option<&HolderRecord> {
        self.previous.as_ref()
    }
}

/// Outcome of trying to take the single-instance lock.
pub enum AcquireOutcome {
    /// This process now owns the lock.
    Acquired(SingleInstanceGuard),
    /// Another live instance already owns it. `holder_pid` is the PID the holder
    /// recorded beside the lock (when readable + parseable), so the contender
    /// can name — or, for the GUI role, activate — the process that has it.
    AlreadyRunning { holder_pid: Option<u32> },
}

/// Try to take the exclusive lock at `lock_path` without blocking.
///
/// `Ok(Acquired)` — we own it; the returned guard must outlive the role's use.
/// `Ok(AlreadyRunning)` — a live holder has it; the caller should defer to the
/// holder. `Err` — an unexpected filesystem/lock error; the caller should log
/// and degrade rather than refuse to start.
pub fn try_acquire(lock_path: &Path) -> std::io::Result<AcquireOutcome> {
    acquire(lock_path, None)
}

/// [`try_acquire`], recording `role` in the holder record so a later reader
/// can tell which kind of process held the lock.
pub fn try_acquire_as(lock_path: &Path, role: &str) -> std::io::Result<AcquireOutcome> {
    acquire(lock_path, Some(role))
}

fn acquire(lock_path: &Path, role: Option<&str>) -> std::io::Result<AcquireOutcome> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;

    let mut lock = fd_lock::RwLock::new(file);
    // The result is reduced to a bool inside this statement so the borrow of
    // `lock` ends with it — the guard borrows `lock`, and `lock` has to be
    // movable into the returned guard below.
    let mut previous = None;
    let won = match lock.try_write() {
        Ok(guard) => {
            // We own the lock. Keep the record the last holder left (it is the
            // only evidence of who that was), then record ourselves beside it —
            // see `pid_path_for` for why beside and not inside — so a later
            // contender can name (and activate) us. Best-effort: a write failure
            // here doesn't lose the lock, it only leaves the contender without a
            // record to report.
            previous = read_holder_record(lock_path);
            write_record(lock_path, &HolderRecord::current(role));

            // Deliberate: the guard's `Drop` unlocks, and we want the lock held
            // for the life of the process. Forgetting it leaves the lock on the
            // still-open file, so the OS drops it when we exit — which is what
            // makes a crash or SIGKILL release it too, rather than stranding a
            // lock no future launch can clear. `lock` owns the `File`, so the
            // handle stays open until `SingleInstanceGuard` is dropped.
            std::mem::forget(guard);
            true
        }
        // `fd-lock` normalises "someone else holds it" to `WouldBlock` on both
        // platforms — `EWOULDBLOCK` from `flock`, `ERROR_LOCK_VIOLATION` from
        // `LockFileEx` — so this is the one contention arm, not a unix-shaped
        // errno check that would quietly fall through to `Err` on Windows.
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(err) => return Err(err),
    };

    if won {
        Ok(AcquireOutcome::Acquired(SingleInstanceGuard { _lock: lock, previous }))
    } else {
        Ok(AcquireOutcome::AlreadyRunning {
            holder_pid: read_holder_pid(lock_path),
        })
    }
}

/// Read the PID the current holder recorded beside the lock. `None` when the
/// file is unreadable or hasn't been written yet (a holder mid-acquire).
///
/// Public so a caller can ask "who holds this role?" without contending for
/// it — taking the lock to find out would itself be a side effect. The file
/// outlives its writer, so the pid may name a process that has since exited:
/// check it is alive before trusting it.
pub fn read_holder_pid(lock_path: &Path) -> Option<u32> {
    read_holder_record(lock_path).map(|r| r.pid)
}

/// Read the whole record the current (or last) holder wrote beside the lock.
/// Like [`read_holder_pid`] it may name a process that has since exited; use
/// [`HolderRecord::is_live`] before trusting it.
pub fn read_holder_record(lock_path: &Path) -> Option<HolderRecord> {
    HolderRecord::decode(&std::fs::read_to_string(pid_path_for(lock_path)).ok()?)
}

/// Write `record` beside the lock atomically (temp file, then rename), so a
/// reader never sees a half-written record. Best-effort, like the lock's
/// bookkeeping in general.
fn write_record(lock_path: &Path, record: &HolderRecord) {
    let path = pid_path_for(lock_path);
    let tmp = path.with_extension(format!("pid.tmp{}", std::process::id()));
    if std::fs::write(&tmp, record.encode()).is_err() || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_lock_on_fresh_path_and_records_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("role.lock");
        match try_acquire(&path).unwrap() {
            AcquireOutcome::Acquired(_guard) => {
                // Reads the PID file, not the lock file. Reading the lock file
                // here is what caught the Windows bug this pairs with: the read
                // goes through a fresh handle, and `LockFileEx` denies that while
                // the lock is held (os error 33).
                assert_eq!(
                    read_holder_pid(&path),
                    Some(std::process::id()),
                    "holder must record its own PID"
                );
            }
            AcquireOutcome::AlreadyRunning { .. } => panic!("fresh path must acquire"),
        }
    }

    #[test]
    fn the_pid_is_recorded_outside_the_locked_file() {
        // The lock file must stay empty. If a future change writes the PID back
        // into it, this fails on every platform rather than only on the one
        // where mandatory locking makes it unreadable — the bug was invisible on
        // macOS, where `flock` is advisory and the read just works.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("role.lock");
        let _guard = match try_acquire(&path).unwrap() {
            AcquireOutcome::Acquired(g) => g,
            AcquireOutcome::AlreadyRunning { .. } => panic!("fresh path must acquire"),
        };
        assert_eq!(
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or_default(),
            0,
            "lock file must carry no payload"
        );
        assert!(pid_path_for(&path).exists(), "PID must be recorded beside it");
    }

    // flock locks attach to the open file *description*, so a second open of
    // the same path within one process contends exactly like a second process —
    // this exercises the real acquire/contend boundary without a subprocess.
    #[test]
    fn second_acquire_contends_while_first_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("role.lock");
        let first = match try_acquire(&path).unwrap() {
            AcquireOutcome::Acquired(g) => g,
            AcquireOutcome::AlreadyRunning { .. } => panic!("first acquire must win"),
        };
        match try_acquire(&path).unwrap() {
            AcquireOutcome::AlreadyRunning { holder_pid } => {
                assert_eq!(
                    holder_pid,
                    Some(std::process::id()),
                    "contender must read the holder's recorded PID"
                );
            }
            AcquireOutcome::Acquired(_) => panic!("second acquire must contend"),
        }
        drop(first);
    }

    // Two role files in one directory are independent locks — holding the GUI
    // role must not block the ticker role, or a desktop would stop a serve
    // process from scheduling merely by existing.
    #[test]
    fn distinct_roles_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let _gui = match try_acquire(&dir.path().join("gui.lock")).unwrap() {
            AcquireOutcome::Acquired(g) => g,
            AcquireOutcome::AlreadyRunning { .. } => panic!("gui role must acquire"),
        };
        match try_acquire(&dir.path().join("ticker.lock")).unwrap() {
            AcquireOutcome::Acquired(_) => {}
            AcquireOutcome::AlreadyRunning { .. } => panic!("ticker role must acquire"),
        }
    }

    fn acquired(outcome: AcquireOutcome) -> SingleInstanceGuard {
        match outcome {
            AcquireOutcome::Acquired(g) => g,
            AcquireOutcome::AlreadyRunning { .. } => panic!("must acquire"),
        }
    }

    #[test]
    fn the_record_is_four_lines_with_the_pid_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        let _g = acquired(try_acquire_as(&path, "desktop").unwrap());
        let text = std::fs::read_to_string(pid_path_for(&path)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "record: {text:?}");
        assert_eq!(lines[0], std::process::id().to_string(), "pid stays on line one");
        assert_eq!(lines[3], "desktop");
        // The pid-only reader still works against the longer record.
        assert_eq!(read_holder_pid(&path), Some(std::process::id()));
        let rec = read_holder_record(&path).unwrap();
        assert!(rec.is_live(), "our own fresh record names a live process: {rec:?}");
        assert_eq!(rec.same_boot(), Some(true));
    }

    #[test]
    fn a_fresh_directory_has_no_previous_holder() {
        let dir = tempfile::tempdir().unwrap();
        let g = acquired(try_acquire(&dir.path().join("host.lock")).unwrap());
        assert_eq!(g.previous(), None);
    }

    #[test]
    fn winning_a_released_lock_reports_the_previous_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        drop(acquired(try_acquire_as(&path, "serve").unwrap()));
        let g = acquired(try_acquire_as(&path, "desktop").unwrap());
        let prev = g.previous().expect("the last holder's record");
        assert_eq!(prev.role.as_deref(), Some("serve"));
        assert_eq!(prev.pid, std::process::id());
        assert_eq!(prev.same_boot(), Some(true));
        // And ours replaced it.
        assert_eq!(read_holder_record(&path).unwrap().role.as_deref(), Some("desktop"));
    }

    #[test]
    fn a_stale_one_line_pid_file_is_read_then_rewritten() {
        // An older build wrote only "<pid>\n" and crashed; the lock is free.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        std::fs::write(pid_path_for(&path), "4242\n").unwrap();
        let g = acquired(try_acquire(&path).unwrap());
        let prev = g.previous().unwrap();
        assert_eq!(prev.pid, 4242);
        assert_eq!(prev.started_at_unix_s, None);
        assert_eq!(prev.same_boot(), None, "no boot time recorded means unknown, not same");
        assert!(!prev.is_live(), "an old record without a start time is never trusted");
        assert_eq!(read_holder_pid(&path), Some(std::process::id()), "rewritten with ours");
    }

    #[test]
    fn a_held_lock_with_an_unreadable_record_is_still_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        let _g = acquired(try_acquire(&path).unwrap());
        std::fs::write(pid_path_for(&path), "garbage\n").unwrap();
        match try_acquire(&path).unwrap() {
            AcquireOutcome::AlreadyRunning { holder_pid } => assert_eq!(holder_pid, None),
            AcquireOutcome::Acquired(_) => panic!("an unreadable record never frees the lock"),
        }
    }

    #[test]
    fn a_record_from_another_boot_is_neither_same_boot_nor_live() {
        let mut rec = HolderRecord::current(Some("serve"));
        rec.boot_time_unix_s = rec.boot_time_unix_s.map(|b| b - 3600);
        assert_eq!(rec.same_boot(), Some(false));
        assert!(!rec.is_live(), "a reboot ended every process from before it");
    }

    #[test]
    fn a_recycled_pid_is_not_live() {
        // The pid is alive (it is us) but started at a different time than
        // the record says, which is what a recycled pid looks like.
        let mut rec = HolderRecord::current(None);
        rec.started_at_unix_s = rec.started_at_unix_s.map(|t| t - 600);
        assert!(!rec.is_live());
        assert!(HolderRecord::current(None).is_live());
    }

    #[test]
    fn decode_round_trips_and_tolerates_missing_fields() {
        let rec = HolderRecord {
            pid: 7,
            started_at_unix_s: Some(100),
            boot_time_unix_s: None,
            role: Some("serve".into()),
        };
        assert_eq!(HolderRecord::decode(&rec.encode()), Some(rec));
        assert_eq!(HolderRecord::decode("not-a-pid\n"), None);
        assert_eq!(HolderRecord::decode("9").map(|r| r.pid), Some(9));
    }

    #[test]
    fn lock_is_released_when_guard_drops() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("role.lock");
        {
            let _g = match try_acquire(&path).unwrap() {
                AcquireOutcome::Acquired(g) => g,
                AcquireOutcome::AlreadyRunning { .. } => panic!("first acquire must win"),
            };
        } // guard dropped here → descriptor closed → lock released

        // Re-acquisition must succeed now the guard is gone. We poll briefly
        // rather than assert instantly: in a multithreaded process another
        // thread can `fork` (spawning a subprocess) in the window between our
        // `open` and its `exec`, transiently duplicating our lock descriptor
        // into the child. CLOEXEC drops it at the child's `exec` a moment
        // later, so the lock frees within milliseconds. Production never races
        // its own re-acquire (the guard is held for the whole process), so this
        // is a test-only scheduling wrinkle, not a guard defect.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match try_acquire(&path).unwrap() {
                AcquireOutcome::Acquired(_) => break,
                AcquireOutcome::AlreadyRunning { .. } if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                AcquireOutcome::AlreadyRunning { .. } => {
                    panic!("lock must free after guard drop (still held past deadline)")
                }
            }
        }
    }
}
