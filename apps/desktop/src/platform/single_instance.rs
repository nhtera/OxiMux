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
//! The lock is a non-blocking exclusive lock (`flock` on unix, `LockFileEx` on
//! Windows, both via `fd-lock`) held for the process lifetime by keeping the
//! open file alive inside the returned guard. The OS releases it when the
//! process exits — including a crash or SIGKILL — so a dead instance never
//! strands a stale lock, which is the failure mode a bare pidfile has. The
//! companion only short-circuits the GUI boot path; the `notify` /
//! `agent-status` helper CLIs and the dev spikes run before this check, so they
//! never contend with a live window.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Lock file name under the app data directory. Sits alongside the relay
/// `relay-v8.{sock,pid,token}` files — same placement convention.
pub const LOCK_FILENAME: &str = "oximux-gui.lock";

/// Resolve the GUI lock path inside a given data directory.
pub fn lock_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCK_FILENAME)
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
pub fn try_acquire(lock_path: &Path) -> std::io::Result<AcquireOutcome> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Do NOT truncate on open: a contender must read the holder's recorded
        // PID, and we only rewrite the file after winning the lock below.
        .truncate(false)
        .open(lock_path)?;

    let mut lock = fd_lock::RwLock::new(file);
    // The result is reduced to a bool inside this statement so the borrow of
    // `lock` ends with it — the guard borrows `lock`, and `lock` has to be
    // movable into the returned guard below.
    let won = match lock.try_write() {
        Ok(mut guard) => {
            // We own the lock. Record our PID so a later contender can name
            // (and activate) us. Best-effort: a write failure here doesn't lose
            // the lock, it only leaves the contender without a PID to focus.
            let _ = guard.set_len(0);
            let _ = guard.seek(SeekFrom::Start(0));
            let _ = writeln!(*guard, "{}", std::process::id());
            let _ = guard.flush();

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

/// Read the PID the current holder recorded in the lock file. `None` when the
/// file is unreadable or hasn't been written yet (a holder mid-acquire).
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

/// Name of the broadcast message a second launch uses to ask the live instance
/// to come forward. `RegisterWindowMessage` maps this string to the same
/// message id for every process on the desktop, which is what lets two
/// unrelated instances agree on one without sharing anything else.
#[cfg(windows)]
pub const ACTIVATE_MESSAGE: &str = "OxiMuxActivate";

/// Ask the running instance to raise its windows.
///
/// Deliberately not a socket, a pipe, or any other channel: the only capability
/// being conveyed is "come to the front", and a broadcast window message can
/// carry nothing else. Anything richer would be new attack surface — reachable
/// by every process on the desktop — in exchange for a feature nobody asked
/// for.
///
/// Windows normally refuses focus changes from a process the user is not
/// interacting with. `AllowSetForegroundWindow` is how the launching process
/// hands its own (just-granted, since the user launched us) foreground right to
/// the holder, so the holder's own `SetForegroundWindow` is honoured rather
/// than flashing its taskbar button.
#[cfg(windows)]
pub fn activate_existing_instance(pid: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, HWND_BROADCAST, PostMessageW, RegisterWindowMessageW,
    };

    let Ok(name) = widestring::U16CString::from_str(ACTIVATE_MESSAGE) else {
        return;
    };
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let msg = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if msg == 0 {
        return;
    }

    // Best-effort, and failure is survivable: the worst case is the holder
    // blinks in the taskbar instead of coming forward.
    // SAFETY: plain Win32 calls with no pointer arguments.
    unsafe {
        AllowSetForegroundWindow(pid);
        // Broadcast rather than target a window: the holder's HWND is not
        // knowable from here, and every process filters on the registered id,
        // which no other application has registered.
        PostMessageW(HWND_BROADCAST, msg, 0, 0);
    }
}

/// No-op activation on platforms with neither AppKit nor Win32.
#[cfg(not(any(target_os = "macos", windows)))]
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
