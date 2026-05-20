//! SIGTERM → grace window → SIGKILL dance for `PortablePtyBackend::close`.
//!
//! Extracted from `portable_pty_backend.rs` for two reasons:
//! 1. The grace logic is conceptually independent of the backend; the
//!    backend just plumbs the right `term_fn` / `kill_fn` closures in.
//! 2. The unit-test surface is wide (5 mock-watcher scenarios) and keeping
//!    the backend module under the 500 LOC warn threshold (xtask
//!    file-size-lint) matters more than module-locality.
//!
//! Public-to-the-crate surface:
//! - `TermResult` — outcome of the SIGTERM step.
//! - `term_step(pid)` — sends SIGTERM to the negative pid (process group)
//!   on Unix; returns `Skipped` on non-Unix or when `pid` is `None`.
//! - `WatcherHandle` trait + `JoinHandleWatcher` production newtype.
//! - `close_with_grace(...)` — the dance itself.

use std::thread::JoinHandle;
use std::time::Duration;

/// Outcome of the SIGTERM step. Drives whether `close_with_grace` waits
/// for the watcher to exit naturally (`Sent` / `AlreadyGone`) or escalates
/// to SIGKILL immediately (`Failed` / `Skipped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TermResult {
    /// SIGTERM delivered successfully — give the watcher the grace window
    /// to observe the child's graceful exit.
    Sent,
    /// `ESRCH` — process group is already gone. Still wait for the
    /// watcher to finish its `child.wait()` call so the thread reaps.
    AlreadyGone,
    /// Signal call failed for a non-recoverable reason (EPERM, etc).
    /// Skip the grace window and escalate to SIGKILL immediately.
    Failed,
    /// SIGTERM step skipped — no PID available (non-Unix backend or
    /// `process_id()` returned None). Escalate immediately.
    Skipped,
}

/// Send SIGTERM to the process group whose leader pid is `pid`.
/// Returns `Skipped` on non-Unix builds or when `pid` is `None`.
pub(crate) fn term_step(pid: Option<u32>) -> TermResult {
    let Some(pid) = pid else {
        return TermResult::Skipped;
    };
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        // M4 review-260521 — defensive cast: PIDs on Linux/macOS are
        // kernel-typed `pid_t = i32`, so `u32` returned by portable-pty
        // never exceeds `i32::MAX` in practice. Use `try_into` so any
        // future host that hands us a > i32::MAX value bails out as
        // `Failed` instead of wrapping to a tiny positive pgid (e.g. 1,
        // which would target init).
        let pid_i32: i32 = match i32::try_from(pid) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[oximux-pty] refusing SIGTERM: pid {pid} exceeds i32::MAX");
                return TermResult::Failed;
            }
        };
        // Negative pid → signal entire process group. pgid == pid because
        // portable-pty's child setsid()'s before exec (portable-pty
        // unix.rs:220), making the child its own session leader.
        match kill(Pid::from_raw(-pid_i32), Signal::SIGTERM) {
            Ok(()) => TermResult::Sent,
            Err(Errno::ESRCH) => TermResult::AlreadyGone,
            Err(err) => {
                eprintln!("[oximux-pty] SIGTERM to pgid {pid} failed: {err}");
                TermResult::Failed
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        TermResult::Skipped
    }
}

/// Test seam over `std::thread::JoinHandle`. Production wraps the real
/// handle; tests substitute a `Cell<bool>`-backed mock to exercise the
/// grace logic without spawning a process.
pub(crate) trait WatcherHandle {
    fn is_finished(&self) -> bool;
    fn join(self);
}

pub(crate) struct JoinHandleWatcher(pub JoinHandle<()>);

impl WatcherHandle for JoinHandleWatcher {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
    fn join(self) {
        let _ = self.0.join();
    }
}

/// SIGTERM → grace-window poll → SIGKILL fallback → watcher join.
///
/// Caller responsibilities (NOT enforced here):
/// 1. Drop the master fd before calling so the watcher's read() unblocks.
/// 2. Pass the SIGKILL fallback as `kill_fn` so the trait's existing
///    `ChildKiller` does not need to be passed through generic bounds.
///
/// On a healthy graceful shutdown, `kill_fn` is never called — the agent
/// exits inside the grace window and the watcher's `child.wait()` reaps
/// it; the join takes constant time.
pub(crate) fn close_with_grace<W: WatcherHandle>(
    watcher: W,
    term_fn: impl FnOnce() -> TermResult,
    kill_fn: impl FnOnce(),
    grace: Duration,
    poll: Duration,
) {
    let term_result = term_fn();
    let needs_grace = matches!(term_result, TermResult::Sent | TermResult::AlreadyGone);
    if needs_grace {
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if watcher.is_finished() {
                watcher.join();
                return;
            }
            std::thread::sleep(poll);
        }
    }
    // Grace expired (or was skipped) without the watcher finishing —
    // escalate. `kill_fn` is idempotent on an already-dead process so the
    // skipped-grace path is safe even when the child exited naturally.
    kill_fn();
    watcher.join();
}

#[cfg(test)]
mod tests {
    //! The watcher and signal primitives are mocked via `WatcherHandle`
    //! and the `TermResult`-returning closure surface — no real process
    //! spawn, no real signals delivered. Tests run in milliseconds.

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    struct MockWatcher {
        finished: Arc<AtomicBool>,
        joined: Arc<AtomicUsize>,
    }
    impl WatcherHandle for MockWatcher {
        fn is_finished(&self) -> bool {
            self.finished.load(Ordering::Relaxed)
        }
        fn join(self) {
            self.joined.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(finished flag, join counter, kill counter)`. Each scenario clones
    /// the flag into the watcher and the counters into the kill closure.
    fn fixture() -> (Arc<AtomicBool>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )
    }

    fn make_kill(counter: Arc<AtomicUsize>) -> impl FnOnce() {
        move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn t1_natural_exit_inside_grace_skips_sigkill() {
        let (finished, joined, killed) = fixture();
        let watcher = MockWatcher {
            finished: Arc::clone(&finished),
            joined: Arc::clone(&joined),
        };
        // Flip the finished flag well inside the grace window to simulate
        // the agent's clean SIGTERM-handler exit.
        let flag = Arc::clone(&finished);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::Relaxed);
        });
        close_with_grace(
            watcher,
            || TermResult::Sent,
            make_kill(Arc::clone(&killed)),
            Duration::from_millis(500),
            Duration::from_millis(10),
        );
        assert_eq!(killed.load(Ordering::Relaxed), 0, "SIGKILL must not fire");
        assert_eq!(joined.load(Ordering::Relaxed), 1, "watcher joined once");
    }

    #[test]
    fn t2_hung_watcher_escalates_to_sigkill_after_grace() {
        // M2 review-260521: previous version had a discarded `finished`
        // Arc from `fixture()`; reconstruct cleanly so the watcher's flag
        // is the only relevant state and the test reads top-to-bottom.
        let joined = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicUsize::new(0));
        let watcher = MockWatcher {
            finished: Arc::new(AtomicBool::new(false)),
            joined: Arc::clone(&joined),
        };
        let start = Instant::now();
        close_with_grace(
            watcher,
            || TermResult::Sent,
            make_kill(Arc::clone(&killed)),
            Duration::from_millis(50),
            Duration::from_millis(10),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(50),
            "must wait full grace before escalating; elapsed={elapsed:?}"
        );
        assert_eq!(killed.load(Ordering::Relaxed), 1, "SIGKILL fires once");
        assert_eq!(
            joined.load(Ordering::Relaxed),
            1,
            "watcher joined after kill"
        );
    }

    #[test]
    fn t3_skipped_term_kills_immediately_without_grace_wait() {
        let (finished, joined, killed) = fixture();
        let watcher = MockWatcher {
            finished: Arc::clone(&finished),
            joined: Arc::clone(&joined),
        };
        let start = Instant::now();
        close_with_grace(
            watcher,
            || TermResult::Skipped,
            make_kill(Arc::clone(&killed)),
            Duration::from_millis(500),
            Duration::from_millis(10),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "must NOT wait grace when SIGTERM was skipped; elapsed={elapsed:?}"
        );
        assert_eq!(killed.load(Ordering::Relaxed), 1);
        assert_eq!(joined.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn t4_esrch_still_polls_grace_and_skips_sigkill() {
        let (finished, joined, killed) = fixture();
        let watcher = MockWatcher {
            finished: Arc::clone(&finished),
            joined: Arc::clone(&joined),
        };
        // ESRCH means the process is gone; the watcher should reap quickly.
        let flag = Arc::clone(&finished);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            flag.store(true, Ordering::Relaxed);
        });
        close_with_grace(
            watcher,
            || TermResult::AlreadyGone,
            make_kill(Arc::clone(&killed)),
            Duration::from_millis(500),
            Duration::from_millis(10),
        );
        assert_eq!(killed.load(Ordering::Relaxed), 0, "SIGKILL must not fire");
        assert_eq!(joined.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn t5_failed_term_escalates_immediately_like_skipped() {
        let joined = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicUsize::new(0));
        let watcher = MockWatcher {
            finished: Arc::new(AtomicBool::new(false)),
            joined: Arc::clone(&joined),
        };
        let start = Instant::now();
        close_with_grace(
            watcher,
            || TermResult::Failed,
            make_kill(Arc::clone(&killed)),
            Duration::from_millis(500),
            Duration::from_millis(10),
        );
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(100));
        assert_eq!(killed.load(Ordering::Relaxed), 1);
    }
}
