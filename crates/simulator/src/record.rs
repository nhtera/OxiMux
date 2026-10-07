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
//!
//! A real iPhone: its capture helper records what it streams
//! (`record_start` / `record_stop`). It is a process of its own as far as
//! macOS privacy goes, so it writes into OxiMux's own folder — never the
//! Desktop, which would ask it for access — and the finished movie is moved
//! to where the user's recordings go.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::child_ledger::{Entry, Kind, Ledger};
use crate::ios_device::DeviceSession;
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
    /// The recorder process (`None` for an iPhone, whose helper records).
    child: Option<Child>,
    pub path: PathBuf,
    pub started: Instant,
    ledger: Option<Arc<Ledger>>,
    android: Option<OnDevice>,
    capture: Option<Capture>,
}

/// An iPhone recording: its helper writes `staging`, moved to `path` when
/// finished.
struct Capture {
    session: DeviceSession,
    staging: PathBuf,
}

/// How long the capture helper may take to start or finalize a movie.
const CAPTURE_REPLY: Duration = Duration::from_secs(15);

impl Recording {
    /// Start recording `udid` into `path` (overwritten if present).
    pub fn start(simctl: &Path, udid: &DeviceId, path: &Path, ledger: Option<Arc<Ledger>>) -> Result<Self> {
        // Only a simulator's id reaches `simctl`.
        let sim = udid.sim_udid().ok_or_else(|| SimError::Unsupported(format!("{udid} is not a simulator")))?;
        let path_arg = path.to_string_lossy().into_owned();
        let args = ["io", sim.as_str(), "recordVideo", "--codec=h264", "--force", path_arg.as_str()];
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

    /// Start recording a real iPhone through its capture helper, into
    /// `staging_dir` until it is finished (then it moves to `path`).
    /// Blocking: waits for the helper's answer.
    pub fn start_capture(session: DeviceSession, staging_dir: &Path, path: &Path) -> Result<Self> {
        std::fs::create_dir_all(staging_dir)?;
        let name = path.file_name().ok_or_else(|| SimError::Unsupported("a recording needs a file name".into()))?;
        let staging = staging_dir.join(name);
        session.record_start(&staging, CAPTURE_REPLY)?;
        Ok(Self {
            child: None,
            path: path.to_path_buf(),
            started: Instant::now(),
            ledger: None,
            android: None,
            capture: Some(Capture { session, staging }),
        })
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
                argv: Vec::new(),
                process_start: None,
            };
            if let Err(e) = ledger.record(entry) {
                tracing::warn!("could not record the screen recording in the child ledger: {e}");
            }
        }
        Self { child: Some(child), path: path.to_path_buf(), started: Instant::now(), ledger, android: None, capture: None }
    }

    /// The recorder process's id (0 for an iPhone's, recorded by its helper).
    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// Whether the recorder is still running (`simctl` exits on its own when
    /// the device shuts down; an iPhone's helper, when it is unplugged).
    pub fn is_running(&mut self) -> bool {
        match (&mut self.child, &self.capture) {
            (Some(child), _) => matches!(child.try_wait(), Ok(None)),
            (None, Some(capture)) => capture.session.video().exited().is_none(),
            (None, None) => false,
        }
    }

    /// Interrupt, wait up to `grace` for the movie to be finalized, then kill.
    /// Blocking: call from a background thread (or at quit). Returns the movie
    /// when `simctl` finished cleanly.
    pub fn stop(mut self, grace: Duration) -> Result<PathBuf> {
        if let Some(capture) = self.capture.take() {
            return finish_capture(&capture, &self.path);
        }
        let Some(mut child) = self.child.take() else { return Err(SimError::Unsupported("nothing was recording".into())) };
        let pid = child.id();
        if let Some(device) = &self.android {
            // The recorder runs on the device: interrupt it there.
            device.interrupt();
        }
        #[cfg(unix)]
        if self.android.is_none() && matches!(child.try_wait(), Ok(None)) {
            // SAFETY: plain kill(2) on our own child's pid, which we have not
            // reaped yet, so it cannot have been recycled.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
        }
        let deadline = Instant::now() + grace;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
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

/// Have the iPhone's helper finish the movie, then move it home. A helper
/// that is ending (the phone unplugged, its session closed) finishes the
/// movie on its way out and says so (`recorded`): wait for that instead.
fn finish_capture(capture: &Capture, path: &Path) -> Result<PathBuf> {
    if let Err(e) = capture.session.record_stop(CAPTURE_REPLY) {
        let video = capture.session.video();
        let deadline = Instant::now() + video.kind().exit_grace();
        while video.exited().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // One recording per helper: a movie it reports is this one.
        if video.recorded().is_none() || !capture.staging.exists() {
            return Err(e);
        }
    }
    move_file(&capture.staging, path)?;
    Ok(path.to_path_buf())
}

/// `rename`, or copy and delete when `to` is on another volume.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
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
        // An iPhone's helper finalizes the movie itself when it exits.
        #[cfg(unix)]
        if let Some(child) = self.child.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            // SAFETY: kill(2) on our own unreaped child.
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGINT);
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
    fn a_finished_movie_moves_home() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("staging.mov"), dir.path().join("home.mov"));
        std::fs::write(&from, b"movie").unwrap();
        move_file(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"movie");
        assert!(!from.exists());
        assert!(move_file(&from, &to).is_err(), "nothing left to move");
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
