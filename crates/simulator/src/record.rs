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
//!
//! Android (P10): `adb shell screenrecord` writes the movie on the device and
//! finalizes it on `SIGINT` there — interrupting the local `adb` does not
//! reach it — so stopping sends `pkill -INT screenrecord` over adb, waits for
//! the recorder to exit, then pulls the movie and deletes it from the device.
//! Android caps a recording at [`ANDROID_MAX_LENGTH`].

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

/// `screenrecord`'s own limit.
pub const ANDROID_MAX_LENGTH: Duration = Duration::from_secs(180);

/// Where an Android recording lives until it is pulled.
struct OnDevice {
    adb: PathBuf,
    serial: String,
    remote: String,
    /// The recorder's pid on the device (so stopping interrupts this
    /// recording, not every `screenrecord` there).
    pid: u32,
}

/// adb calls around an Android recording (interrupt, remove): short.
const ADB_QUICK: Duration = Duration::from_secs(5);
/// Pulling the movie (up to ~90 MB for three minutes).
const ADB_PULL: Duration = Duration::from_secs(60);

impl OnDevice {
    fn adb(&self, args: &[&str], timeout: Duration) -> Result<crate::runner::CmdOutput> {
        let mut all = vec!["-s", self.serial.as_str()];
        all.extend_from_slice(args);
        crate::runner::Runner::run(&crate::runner::SystemRunner, &self.adb.to_string_lossy(), &all, None, timeout)
    }

    /// Interrupt the recorder (it finalizes the movie on SIGINT).
    fn interrupt(&self) {
        let _ = self.adb(&["shell", "kill", "-INT", &self.pid.to_string()], ADB_QUICK);
    }

    /// Stop it outright and delete what it wrote (a recording given up on).
    fn discard(&self) {
        let _ = self.adb(&["shell", "kill", "-KILL", &self.pid.to_string()], ADB_QUICK);
        let _ = self.adb(&["shell", "rm", "-f", &self.remote], ADB_QUICK);
    }
}

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
    android: Option<OnDevice>,
}

impl Recording {
    /// Start recording `udid` into `path` (overwritten if present).
    pub fn start(simctl: &Path, udid: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Result<Self> {
        let path_arg = path.to_string_lossy().into_owned();
        let args = ["io", udid.as_str(), "recordVideo", "--codec=h264", "--force", path_arg.as_str()];
        Self::spawn(simctl, &args, udid, path, ledger)
    }

    /// Start recording Android device `serial` (named `id`) into `path`,
    /// through a temporary movie on the device.
    pub fn start_android(adb: &Path, serial: &str, id: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Result<Self> {
        let remote = format!("/sdcard/oximux-recording-{}.mp4", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
        // `echo $$` first: the shell's pid is the recorder's once it `exec`s.
        let script = format!("echo $$; exec screenrecord --time-limit {} {remote}", ANDROID_MAX_LENGTH.as_secs());
        let mut child = Command::new(adb)
            .args(["-s", serial, "shell", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| SimError::Protocol("no recorder output".into()))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("oximux-screenrecord".into()).spawn(move || {
            use std::io::BufRead as _;
            let mut lines = std::io::BufReader::new(stdout).lines();
            let _ = tx.send(lines.next().and_then(|l| l.ok()));
            // Keep draining: a recorder whose output backs up would stall.
            for _ in lines {}
        })?;
        let pid = rx.recv_timeout(Duration::from_secs(10)).ok().flatten().and_then(|l| l.trim().parse::<u32>().ok());
        let Some(pid) = pid else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SimError::HelperFailed("the device's screen recorder did not start".into()));
        };
        let mut recording = Self::track(child, adb, id, path, ledger);
        recording.android = Some(OnDevice { adb: adb.to_path_buf(), serial: serial.to_owned(), remote, pid });
        Ok(recording)
    }

    fn spawn(exe: &Path, args: &[&str], udid: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Result<Self> {
        let child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self::track(child, exe, udid, path, ledger))
    }

    /// Record `child` in the ledger (so a crash does not leave it running).
    fn track(child: Child, exe: &Path, udid: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Self {
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
        Self { child, path: path.to_path_buf(), started: Instant::now(), ledger, android: None }
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
        if let Some(device) = &self.android {
            // The recorder runs on the device: interrupt it there.
            device.interrupt();
        }
        #[cfg(unix)]
        if self.android.is_none() && matches!(self.child.try_wait(), Ok(None)) {
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
        if let Some(device) = self.android.take() {
            // Pull only a movie the recorder finished: one killed mid-write
            // would not play.
            if status.is_none() {
                device.discard();
                return Err(SimError::Timeout { what: "finalizing the recording".into(), secs: grace.as_secs() });
            }
            return pull(&device, &path);
        }
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

/// Bring an Android movie home and delete it from the device.
fn pull(device: &OnDevice, path: &Path) -> Result<PathBuf> {
    // screenrecord closes the file a moment after it exits.
    std::thread::sleep(Duration::from_millis(300));
    let pulled = device.adb(&["pull", &device.remote, &path.to_string_lossy()], ADB_PULL).and_then(|out| out.into_success("adb pull"));
    let _ = device.adb(&["shell", "rm", "-f", &device.remote], ADB_QUICK);
    pulled.map(|_| path.to_path_buf())
}

/// A recording dropped without [`Recording::stop`] (a lost handle, a start
/// that landed too late) is still interrupted, so `simctl` does not record
/// on forever; the ledger entry stays for the next launch to reap.
impl Drop for Recording {
    fn drop(&mut self) {
        // Android: the recorder runs on the device, which interrupting the
        // local adb does not reach. Best effort, off this thread.
        if let Some(device) = self.android.take() {
            let _ = std::thread::Builder::new().name("oximux-screenrecord-stop".into()).spawn(move || device.discard());
        }
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
