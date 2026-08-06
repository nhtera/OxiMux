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

/// Held for the whole process lifetime. Dropping it (or process exit) closes
/// the underlying descriptor, which releases the advisory lock.
pub struct SingleInstanceGuard {
    // The lock lives on the open file description (unix) / handle (Windows);
    // keeping the `File` alive is the entire mechanism. The field is never read
    // — it is held for its Drop, which closes the file and frees the lock.
    _lock: fd_lock::RwLock<std::fs::File>,
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
    let won = match lock.try_write() {
        Ok(guard) => {
            // We own the lock. Record our PID beside it — see `pid_path_for` for
            // why beside and not inside — so a later contender can name (and
            // activate) us. Best-effort: a write failure here doesn't lose the
            // lock, it only leaves the contender without a PID to report.
            let _ = std::fs::write(
                pid_path_for(lock_path),
                format!("{}\n", std::process::id()),
            );

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
        Ok(AcquireOutcome::Acquired(SingleInstanceGuard { _lock: lock }))
    } else {
        Ok(AcquireOutcome::AlreadyRunning {
            holder_pid: read_holder_pid(lock_path),
        })
    }
}

/// Read the PID the current holder recorded beside the lock. `None` when the
/// file is unreadable or hasn't been written yet (a holder mid-acquire).
fn read_holder_pid(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path_for(lock_path))
        .ok()?
        .trim()
        .parse()
        .ok()
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
