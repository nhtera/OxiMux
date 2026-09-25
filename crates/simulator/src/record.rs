//! Screen recording: `simctl io <udid> recordVideo`, stopped with `SIGINT`.
//!
//! `simctl` finalizes the movie only when it is interrupted the way `Ctrl-C`
//! does it; a `SIGKILL` leaves a file no player opens. So [`Recording::stop`]
//! sends `SIGINT`, waits up to [`FINALIZE_GRACE`], and kills only after that.
//! `simctl` is spawned directly (resolved once with `xcrun --find`), never
//! through `xcrun`, so the signal reaches the process that writes the file
//! and the child ledger's `argv[0]` check matches.
//!
//! Every recording is written to the [`crate::child_ledger`] while it runs, so
//! a crash does not leave `simctl` recording forever.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::child_ledger::{Entry, Kind, Ledger};
use crate::runner::Runner;
use crate::{DeviceId, Result, SimError};

/// How long a stopped recording gets to finalize its movie before it is
/// killed.
pub const FINALIZE_GRACE: Duration = Duration::from_secs(5);

/// Recordings stop on their own after this long.
pub const MAX_LENGTH: Duration = Duration::from_secs(10 * 60);

/// `simctl`'s path inside the selected Xcode (`xcrun --find simctl`).
pub fn simctl_path(runner: &dyn Runner, timeout: Duration) -> Result<PathBuf> {
    let out = runner.run("xcrun", &["--find", "simctl"], None, timeout)?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.success() || path.is_empty() {
        return Err(SimError::CommandFailed {
            program: "xcrun --find simctl".into(),
            code: out.status,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(PathBuf::from(path))
}

/// A recording in progress.
pub struct Recording {
    child: Child,
    pub path: PathBuf,
    pub started: Instant,
    ledger: Option<Arc<Ledger>>,
}

impl Recording {
    /// Start recording `udid` into `path` (overwritten if present).
    pub fn start(simctl: &Path, udid: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Result<Self> {
        let path_arg = path.to_string_lossy().into_owned();
        let args = ["io", udid.as_str(), "recordVideo", "--codec=h264", "--force", path_arg.as_str()];
        Self::spawn(simctl, &args, udid, path, ledger)
    }

    fn spawn(exe: &Path, args: &[&str], udid: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Result<Self> {
        let child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(ledger) = &ledger {
            let entry = Entry {
                pid: child.id(),
                kind: Kind::Record,
                exe: exe.to_path_buf(),
                started_at_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                udid: Some(udid.0.clone()),
                owner_pid: std::process::id(),
            };
            if let Err(e) = ledger.record(entry) {
                tracing::warn!("could not record the screen recording in the child ledger: {e}");
            }
        }
        Ok(Self { child, path: path.to_path_buf(), started: Instant::now(), ledger })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether `simctl` is still running (it exits on its own when the
    /// device shuts down).
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Interrupt, wait up to `grace` for the movie to be finalized, then kill.
    /// Blocking: call from a background thread (or at quit). Returns the movie
    /// when `simctl` finished cleanly.
    pub fn stop(mut self, grace: Duration) -> Result<PathBuf> {
        let pid = self.child.id();
        #[cfg(unix)]
        if matches!(self.child.try_wait(), Ok(None)) {
            // SAFETY: plain kill(2) on our own child's pid, which we have not
            // reaped yet, so it cannot have been recycled.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
        }
        let deadline = Instant::now() + grace;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break None;
                }
            }
        };
        if let Some(Err(e)) = self.ledger.as_ref().map(|l| l.remove(pid)) {
            tracing::warn!("could not drop the screen recording {pid} from the child ledger: {e}");
        }
        let path = std::mem::take(&mut self.path);
        match status {
            Some(status) if status.success() => Ok(path),
            Some(status) => Err(SimError::CommandFailed {
                program: "simctl io recordVideo".into(),
                code: status.code(),
                stderr: String::new(),
            }),
            None => Err(SimError::Timeout { what: "finalizing the recording".into(), secs: grace.as_secs() }),
        }
    }
}

/// A recording dropped without [`Recording::stop`] (a lost handle, a start
/// that landed too late) is still interrupted, so `simctl` does not record
/// on forever; the ledger entry stays for the next launch to reap.
impl Drop for Recording {
    fn drop(&mut self) {
        #[cfg(unix)]
        if matches!(self.child.try_wait(), Ok(None)) {
            // SAFETY: kill(2) on our own unreaped child.
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str, path: &Path, ledger: Option<Arc<Ledger>>) -> Recording {
        Recording::spawn(Path::new("/bin/sh"), &["-c", script], &DeviceId("U".into()), path, ledger).unwrap()
    }

    #[test]
    fn stop_interrupts_and_returns_the_movie_once_finalized() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(Ledger::open(dir.path().join("children.json")).unwrap());
        let movie = dir.path().join("a.mov");
        // Stand-in for simctl: finalizes on SIGINT, like the real one.
        let rec = sh("trap 'exit 0' INT; while :; do sleep 0.05; done", &movie, Some(ledger.clone()));
        let pid = rec.pid();
        assert!(ledger.entries().unwrap().iter().any(|e| e.pid == pid && e.kind == Kind::Record));
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        assert_eq!(rec.stop(FINALIZE_GRACE).unwrap(), movie);
        assert!(started.elapsed() < Duration::from_secs(2), "stopped by the interrupt, not the grace");
        assert!(ledger.entries().unwrap().is_empty(), "ledger cleaned");
    }

    #[test]
    fn a_recorder_that_ignores_the_interrupt_is_killed_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let rec = sh("trap '' INT; while :; do sleep 0.05; done", &dir.path().join("b.mov"), None);
        std::thread::sleep(Duration::from_millis(150));
        let err = rec.stop(Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, SimError::Timeout { .. }), "{err}");
    }

    #[test]
    fn is_running_returns_false_after_the_process_exits() {
        let dir = tempfile::tempdir().unwrap();
        // Process exits immediately.
        let mut rec = sh("exit 0", &dir.path().join("c.mov"), None);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!rec.is_running(), "process exited");
    }
}
