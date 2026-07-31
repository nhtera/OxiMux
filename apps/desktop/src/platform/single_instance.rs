//! Single-instance guard for the GUI process.
//!
//! Two OxiMux GUI processes pointed at the same on-disk data directory race on
//! the per-window layout store (`terminal_tabs:*`, `pane_relay_ids`,
//! `open_windows`) and both attach the same relay PTYs, so the second launch
//! silently clobbers the first's persisted session — a terminal saved by one
//! instance vanishes when the other overwrites the snapshot. This guard makes a
//! second launch a no-op: it takes an exclusive advisory lock on a file in the
//! data directory and, if a live instance already holds it, brings that
//! instance to the foreground and bows out instead of booting a second racing
//! window set.
//!
//! The lock is an `flock(LOCK_EX | LOCK_NB)` held for the process lifetime by
//! keeping the open file alive inside the returned guard. The kernel releases
//! it automatically when the process exits — including a crash or SIGKILL — so
//! a dead instance never strands a stale lock (the failure mode a bare pidfile
//! has). The companion only short-circuits the GUI boot path; the `notify` /
//! `agent-status` helper CLIs and the dev spikes run before this check, so they
//! never contend with a live window.

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Lock file name under the app data directory. Sits alongside the relay
/// `relay-v7.{sock,pid,token}` files — same placement convention.
pub const LOCK_FILENAME: &str = "oximux-gui.lock";

/// Resolve the GUI lock path inside a given data directory.
pub fn lock_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCK_FILENAME)
}

/// Held for the whole process lifetime. Dropping it (or process exit) closes
/// the underlying descriptor, which releases the advisory lock.
pub struct SingleInstanceGuard {
    // The lock lives on the open file description; keeping the `File` alive is
    // the entire mechanism. The field is never read — it is held for its Drop.
    _file: std::fs::File,
}

/// Outcome of trying to take the single-instance lock.
pub enum AcquireOutcome {
    /// This process now owns the lock.
    Acquired(SingleInstanceGuard),
    /// Another live instance already owns it. `holder_pid` is the PID recorded
    /// in the lock file (when it was readable + parseable), used to bring that
    /// instance forward.
    AlreadyRunning { holder_pid: Option<u32> },
}

/// Try to take the exclusive lock at `lock_path` without blocking.
///
/// `Ok(Acquired)` — we own it; the returned guard must outlive the GUI.
/// `Ok(AlreadyRunning)` — a live holder has it; the caller should activate the
/// holder and exit. `Err` — an unexpected filesystem/lock error; the caller
/// should log and degrade to a normal boot rather than refuse to start.
#[cfg(unix)]
pub fn try_acquire(lock_path: &Path) -> std::io::Result<AcquireOutcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Do NOT truncate on open: a contender must read the holder's recorded
        // PID, and we only rewrite the file after winning the flock below.
        .truncate(false)
        .open(lock_path)?;

    // SAFETY: `flock` is a thin libc call on a valid descriptor we own; the
    // file stays open for the call's duration. `LOCK_NB` makes it return
    // immediately rather than blocking when another description holds the lock.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // We own the lock. Record our PID so a later contender can name (and
        // activate) us. Best-effort: a write failure here doesn't lose the
        // lock, it only leaves the contender without a PID to focus.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{}", std::process::id());
        let _ = file.flush();
        return Ok(AcquireOutcome::Acquired(SingleInstanceGuard { _file: file }));
    }

    let err = std::io::Error::last_os_error();
    // `EWOULDBLOCK` is the "lock is held" signal — the only non-error outcome
    // of a non-blocking `flock`. (It shares a value with `EAGAIN` on macOS, so
    // matching the one constant covers both.)
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(AcquireOutcome::AlreadyRunning {
            holder_pid: read_holder_pid(lock_path),
        });
    }
    Err(err)
}

/// No lock is taken off Unix yet, so this reports failure rather than success.
///
/// The caller's documented response to `Err` is to log and boot normally, which
/// is the honest state of affairs: `LockFileEx` and the window-message
/// activation that pairs with it are their own piece of work. Returning
/// `Acquired` instead would claim a guarantee — only one instance owns the
/// layout store — that nothing is enforcing, and the symptom (a second launch
/// silently overwriting the first's saved terminals) surfaces far from here.
#[cfg(not(unix))]
pub fn try_acquire(lock_path: &Path) -> std::io::Result<AcquireOutcome> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "single-instance lock is unimplemented on this platform ({})",
            lock_path.display()
        ),
    ))
}

/// Read the PID the current holder recorded in the lock file. `None` when the
/// file is unreadable or hasn't been written yet (a holder mid-acquire).
#[cfg(unix)]
fn read_holder_pid(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_path).ok()?.trim().parse().ok()
}

/// Bring the already-running instance to the foreground so the user sees their
/// existing window instead of a launch that appears to do nothing. Best-effort:
/// a no-op if the PID is gone or not a GUI app.
#[cfg(target_os = "macos")]
pub fn activate_existing_instance(pid: u32) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: `NSRunningApplication` is part of AppKit, already linked into the
    // process. `runningApplicationWithProcessIdentifier:` returns a (possibly
    // nil) autoreleased instance; we only message it when non-nil.
    unsafe {
        let app: *mut AnyObject = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid as i32
        ];
        if app.is_null() {
            return;
        }
        // NSApplicationActivateAllWindows | NSApplicationActivateIgnoringOtherApps:
        // raise every window of the holder and steal focus from the launcher.
        let options: u64 = (1 << 0) | (1 << 1);
        let _: bool = msg_send![app, activateWithOptions: options];
    }
}

/// No-op activation on platforms without AppKit.
#[cfg(not(target_os = "macos"))]
pub fn activate_existing_instance(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_lock_on_fresh_path_and_records_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path_in(dir.path());
        match try_acquire(&path).unwrap() {
            AcquireOutcome::Acquired(_guard) => {
                let recorded = std::fs::read_to_string(&path).unwrap();
                assert_eq!(
                    recorded.trim().parse::<u32>().unwrap(),
                    std::process::id(),
                    "holder must record its own PID"
                );
            }
            AcquireOutcome::AlreadyRunning { .. } => panic!("fresh path must acquire"),
        }
    }

    // flock locks attach to the open file *description*, so a second open of
    // the same path within one process contends exactly like a second process —
    // this exercises the real acquire/contend boundary without a subprocess.
    #[test]
    fn second_acquire_contends_while_first_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path_in(dir.path());
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

    #[test]
    fn lock_is_released_when_guard_drops() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path_in(dir.path());
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
