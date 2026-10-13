//! Running short external commands (`xcrun simctl`, `xcode-select`, `sw_vers`)
//! with a timeout, behind a trait so parsing and gating logic is unit-tested
//! with scripted output instead of a real Xcode.
//!
//! Every call blocks until the child exits or the timeout fires, so callers run
//! it on a background executor, never on the UI thread.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use oximux_no_window::NoWindow;

use crate::{Result, SimError};

/// What a finished command produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code; `None` when the child was killed by a signal.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CmdOutput {
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        Self { status: Some(0), stdout: stdout.into(), stderr: Vec::new() }
    }

    pub fn failed(code: i32, stderr: impl Into<Vec<u8>>) -> Self {
        Self { status: Some(code), stdout: Vec::new(), stderr: stderr.into() }
    }

    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// `Ok(self)` on exit 0, else [`SimError::CommandFailed`] naming `program`.
    pub fn into_success(self, program: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        Err(SimError::CommandFailed {
            program: program.to_owned(),
            code: self.status,
            stderr: String::from_utf8_lossy(&self.stderr).trim().to_owned(),
        })
    }
}

/// Runs a program to completion.
pub trait Runner: Send + Sync {
    /// Run `program args…`, feeding `stdin` (or `/dev/null`), and wait at most
    /// `timeout`. A timeout kills the child and returns [`SimError::Timeout`].
    fn run(&self, program: &str, args: &[&str], stdin: Option<&[u8]>, timeout: Duration)
    -> Result<CmdOutput>;

    /// [`Runner::run`] without stdin, its stdout handed to `line` one line
    /// at a time as it comes, so a long output is never held whole. The
    /// returned output has the exit status and stderr; its stdout is empty.
    fn run_lines(&self, program: &str, args: &[&str], timeout: Duration, line: &mut dyn FnMut(&str)) -> Result<CmdOutput> {
        let out = self.run(program, args, None, timeout)?;
        out.stdout_str().lines().for_each(&mut *line);
        Ok(CmdOutput { stdout: Vec::new(), ..out })
    }
}

/// The real thing: `std::process` with a polling timeout.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run_lines(&self, program: &str, args: &[&str], timeout: Duration, line: &mut dyn FnMut(&str)) -> Result<CmdOutput> {
        Self::stream_lines(program, args, timeout, line)
    }

    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<CmdOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .no_window();
        let mut child = cmd.spawn()?;
        // Fed on its own thread, under the same deadline as everything else:
        // a wedged child that never reads must not block us on a full pipe.
        // A write error (the child exited early) is the child's problem to
        // report through its exit status, not ours.
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let input = input.to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
                // Dropping `pipe` closes the child's stdin.
            });
        }
        let out = drain(child.stdout.take());
        let err = drain(child.stderr.take());
        let deadline = Instant::now() + timeout;
        let status = loop {
            let polled = match child.try_wait() {
                Ok(polled) => polled,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e.into());
                }
            };
            if let Some(status) = polled {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SimError::Timeout {
                    what: redact_argv(program, args),
                    secs: timeout.as_secs(),
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Ok(CmdOutput {
            status: status.code(),
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    }
}

impl SystemRunner {
    /// [`Runner::run_lines`], streamed: stdout is read on its own thread and
    /// each line handed over as it arrives, under the one deadline.
    fn stream_lines(program: &str, args: &[&str], timeout: Duration, line: &mut dyn FnMut(&str)) -> Result<CmdOutput> {
        use std::io::BufRead as _;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .no_window()
            .spawn()?;
        let err = drain(child.stderr.take());
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(out);
                let mut buf = Vec::new();
                while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                    if tx.send(std::mem::take(&mut buf)).is_err() {
                        return;
                    }
                }
            });
        }
        let deadline = Instant::now() + timeout;
        let timeout_error = || SimError::Timeout { what: redact_argv(program, args), secs: timeout.as_secs() };
        let kill = |child: &mut std::process::Child| {
            let _ = child.kill();
            let _ = child.wait();
        };
        // The deadline is checked on every line, so a log that keeps coming
        // cannot outlast it.
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                kill(&mut child);
                return Err(timeout_error());
            }
            match rx.recv_timeout(left) {
                Ok(bytes) => line(String::from_utf8_lossy(&bytes).trim_end_matches(['\n', '\r'])),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    kill(&mut child);
                    return Err(timeout_error());
                }
            }
        }
        // Stdout closed; the child may still be running: waited for under
        // the same deadline.
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                kill(&mut child);
                return Err(timeout_error());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Ok(CmdOutput { status: status.code(), stdout: Vec::new(), stderr: err.join().unwrap_or_default() })
    }
}

/// What [`run_bounded`] got from a command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bounded {
    /// Exit code; `None` when it was killed (a signal, the cap, the deadline).
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Its output reached the cap: the rest was not read, and it was stopped.
    pub truncated: bool,
    /// It was still running at the deadline, and was stopped.
    pub timed_out: bool,
}

/// Run `program args…` (stdin closed), keeping at most `cap` bytes of its
/// stdout and stderr together. It is stopped when that fills or at
/// `timeout`, and what it wrote by then comes back: a command that never ends
/// on its own (a log that follows) still answers.
///
/// On Unix it runs in a process group of its own. On macOS the whole group is
/// stopped when the call ends, even after the command exited by itself (it
/// is watched without being reaped, so the group's id stays its own): a
/// child it left in the background cannot outlive the call. The readers stop
/// with the call too, whoever still holds the pipes.
pub fn run_bounded(program: &str, args: &[&str], timeout: Duration, cap: usize) -> Result<Bounded> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).no_window();
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn()?;
    // Both streams draw on one budget; a reader that finds it spent stops.
    let (left, full, stop) = (Arc::new(AtomicUsize::new(cap)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let capped = |pipe: Option<Pipe>| {
        let (left, full, stop, buf) = (left.clone(), full.clone(), stop.clone(), Arc::new(Mutex::new(Vec::new())));
        let kept = buf.clone();
        let reader = std::thread::spawn(move || {
            let Some(mut pipe) = pipe else { return };
            let mut chunk = [0u8; 8192];
            loop {
                if !pipe.readable(&stop) {
                    return;
                }
                let n = match pipe.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                let took = left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |l| Some(l.saturating_sub(n))).unwrap_or(0).min(n);
                kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend_from_slice(&chunk[..took]);
                if took < n {
                    full.store(true, Ordering::SeqCst);
                    return;
                }
            }
        });
        (reader, buf)
    };
    let out = capped(child.stdout.take().map(Pipe::new));
    let err = capped(child.stderr.take().map(Pipe::new));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match exited(&mut child) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                stop_group(&mut child);
                stop.store(true, Ordering::SeqCst);
                return Err(e.into());
            }
        }
        // Spent: it is writing more than anyone will read.
        if full.load(Ordering::SeqCst) {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = stop_group(&mut child);
    // The pipes close once the group is gone; a descendant that left it
    // could keep them open, so the readers get a moment, then are stopped.
    let settle = Instant::now() + Duration::from_millis(500);
    while !(out.0.is_finished() && err.0.is_finished()) && Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::SeqCst);
    let take = |buf: &Arc<Mutex<Vec<u8>>>| std::mem::take(&mut *buf.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    Ok(Bounded { status, stdout: take(&out.1), stderr: take(&err.1), truncated: full.load(Ordering::SeqCst), timed_out })
}

/// One of [`run_bounded`]'s output pipes.
struct Pipe {
    read: Box<dyn Read + Send>,
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

impl Pipe {
    #[cfg(unix)]
    fn new<P: Read + Send + std::os::fd::AsRawFd + 'static>(pipe: P) -> Self {
        Self { fd: pipe.as_raw_fd(), read: Box::new(pipe) }
    }

    #[cfg(not(unix))]
    fn new<P: Read + Send + 'static>(pipe: P) -> Self {
        Self { read: Box::new(pipe) }
    }

    /// Wait until there is something to read (or the pipe closed); `false`
    /// once `stop` is set, so a reader never outlives its call (on Unix;
    /// elsewhere the read itself waits).
    fn readable(&self, stop: &std::sync::atomic::AtomicBool) -> bool {
        #[cfg(not(unix))]
        return !stop.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(unix)]
        loop {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            let mut poll = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd for a descriptor this pipe owns, open while
            // it lives; a 50 ms wait.
            if unsafe { libc::poll(&mut poll, 1, 50) } != 0 {
                // Ready, closed, or an error the read reports.
                return true;
            }
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read.read(buf)
    }
}

/// Whether [`run_bounded`]'s command has ended. On macOS it stays unreaped
/// (a zombie keeps its group's id), so its group can still be stopped.
#[cfg(target_os = "macos")]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: `waitid` fills a zeroed siginfo for one pid this call spawned;
    // WNOWAIT leaves it to be reaped later.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, flags) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.si_pid != 0)
}

#[cfg(not(target_os = "macos"))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|done| done.is_some())
}

/// Stop what [`run_bounded`] started and reap it: on macOS its whole group,
/// its leader still unreaped (so the group's id is still its own), whether it
/// ended or not; elsewhere the command itself. Its exit code, `None` when it
/// was killed.
fn stop_group(child: &mut std::process::Child) -> Option<i32> {
    #[cfg(target_os = "macos")]
    // SAFETY: `kill(2)` on the group this call created; its leader is not
    // reaped yet, so the id is still this group's.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(target_os = "macos"))]
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    child.wait().ok().and_then(|status| status.code())
}

/// `program` and its first two arguments (after adb's `-s <serial>`), the
/// rest counted: what an error
/// or a log line may say about a command. A later argument can be a payload,
/// a path with a person's name in it, or a secret.
pub fn redact_argv(program: &str, args: &[&str]) -> String {
    // `adb -s <serial>` names the device, not the command: the two after it
    // say what failed.
    let keep = if args.first() == Some(&"-s") { 4 } else { 2 };
    let mut out = std::iter::once(program).chain(args.iter().take(keep).copied()).collect::<Vec<_>>().join(" ");
    match args.len().saturating_sub(keep) {
        0 => {}
        1 => out.push_str(" …(1 arg)"),
        rest => out.push_str(&format!(" …({rest} args)")),
    }
    out
}

/// `xcrun --find <tool>`: the tool's path inside the selected Xcode.
/// Never cached: the developer dir changes at runtime (Settings' Xcode
/// switch), and a stale path would run the old Xcode's tool. Long-lived
/// children (`record.rs`, `helper.rs`, the scrcpy server, the emulator,
/// the runner build and supervisor) spawn their program directly, as they
/// need the `Child`.
pub fn xcrun_find(runner: &dyn Runner, tool: &str, timeout: Duration) -> Result<std::path::PathBuf> {
    let out = runner.run("xcrun", &["--find", tool], None, timeout)?;
    let path = out.stdout_str().trim().to_owned();
    if !out.success() || path.is_empty() {
        return Err(SimError::CommandFailed {
            program: format!("xcrun --find {tool}"),
            code: out.status,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(std::path::PathBuf::from(path))
}

/// Read a pipe to EOF on its own thread, so a chatty child can never fill one
/// pipe while we block on the other.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

/// One scripted response for [`ScriptedRunner`].
#[derive(Clone, Debug)]
pub struct Scripted {
    /// `program` followed by its args, joined with single spaces.
    pub command: String,
    pub result: std::result::Result<CmdOutput, String>,
}

/// A [`Runner`] for tests: answers commands from a script, in order, and
/// records every call. An unexpected command fails the call loudly.
#[derive(Debug, Default)]
pub struct ScriptedRunner {
    script: Mutex<VecDeque<Scripted>>,
    calls: Mutex<Vec<String>>,
    stdins: Mutex<Vec<Option<Vec<u8>>>>,
}

impl ScriptedRunner {
    pub fn new(script: impl IntoIterator<Item = Scripted>) -> Self {
        Self { script: Mutex::new(script.into_iter().collect()), calls: Mutex::default(), stdins: Mutex::default() }
    }

    /// Expect `command` next and answer with `output`.
    pub fn expect(self, command: &str, output: CmdOutput) -> Self {
        self.push(command, Ok(output))
    }

    /// Expect `command` next and fail it as a spawn error (e.g. not installed).
    pub fn expect_spawn_error(self, command: &str, message: &str) -> Self {
        self.push(command, Err(message.to_owned()))
    }

    fn push(self, command: &str, result: std::result::Result<CmdOutput, String>) -> Self {
        self.script.lock().unwrap().push_back(Scripted { command: command.to_owned(), result });
        self
    }

    /// Every command run so far, as `program arg…`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// What call `n` (from 0) was fed on stdin.
    pub fn stdin_of(&self, n: usize) -> Option<Vec<u8>> {
        self.stdins.lock().unwrap().get(n).cloned().flatten()
    }
}

impl Runner for ScriptedRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<CmdOutput> {
        let command = std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ");
        self.calls.lock().unwrap().push(command.clone());
        self.stdins.lock().unwrap().push(stdin.map(<[u8]>::to_vec));
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(step) if step.command == command => step.result.map_err(|message| {
                SimError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, message))
            }),
            Some(step) => Err(SimError::Protocol(format!(
                "scripted runner: expected `{}`, got `{command}`",
                step.command
            ))),
            None => Err(SimError::Protocol(format!("scripted runner: unexpected `{command}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_names_the_program_and_two_arguments_only() {
        assert_eq!(redact_argv("xcrun", &["simctl", "push", "U", "com.x", "-"]), "xcrun simctl push …(3 args)");
        assert_eq!(redact_argv("adb", &["-s", "S", "shell", "pm", "list", "packages"]), "adb -s S shell pm …(2 args)");
        assert_eq!(redact_argv("xcrun", &["--find", "simctl"]), "xcrun --find simctl");
        assert_eq!(redact_argv("sw_vers", &[]), "sw_vers");
    }

    #[test]
    #[cfg(unix)]
    fn a_timeout_says_no_more_than_that() {
        let slow = SystemRunner.run("/bin/sh", &["-c", "sleep 5", "sh", "secret-token-value"], None, Duration::from_millis(200));
        let text = slow.unwrap_err().to_string();
        assert!(text.contains("/bin/sh -c sleep 5 …(2 args)"), "{text}");
        assert!(!text.contains("secret-token-value"), "{text}");
        let streamed = SystemRunner.run_lines("/bin/sh", &["-c", "sleep 5", "sh", "secret-token-value"], Duration::from_millis(200), &mut |_| {});
        assert!(!streamed.unwrap_err().to_string().contains("secret-token-value"));
    }

    #[test]
    fn xcrun_find_asks_every_time() {
        // The developer dir changed between the two calls (an Xcode switch):
        // the second answer is the one used.
        let runner = ScriptedRunner::default()
            .expect("xcrun --find simctl", CmdOutput::ok("/Applications/Xcode-26.0.app/Contents/Developer/usr/bin/simctl\n"))
            .expect("xcrun --find simctl", CmdOutput::ok("/Applications/Xcode.app/Contents/Developer/usr/bin/simctl\n"));
        let first = xcrun_find(&runner, "simctl", Duration::from_secs(1)).unwrap();
        let second = xcrun_find(&runner, "simctl", Duration::from_secs(1)).unwrap();
        assert_ne!(first, second);
        assert!(second.starts_with("/Applications/Xcode.app"));
        let missing = ScriptedRunner::default().expect("xcrun --find nope", CmdOutput::failed(1, "unable to find utility"));
        assert!(matches!(xcrun_find(&missing, "nope", Duration::from_secs(1)), Err(SimError::CommandFailed { .. })));
    }

    #[test]
    #[cfg(unix)]
    fn system_runner_streams_lines_under_its_deadline() {
        let mut lines = Vec::new();
        let out = SystemRunner
            .run_lines("/bin/sh", &["-c", "printf 'a\\nb\\r\\nc'; echo err >&2; exit 2"], Duration::from_secs(5), &mut |l| lines.push(l.to_owned()))
            .unwrap();
        assert_eq!(lines, ["a", "b", "c"]);
        assert_eq!((out.status, out.stdout.len(), out.stderr.as_slice()), (Some(2), 0, b"err\n".as_slice()));
        let slow = SystemRunner.run_lines("/bin/sh", &["-c", "echo first; sleep 5"], Duration::from_millis(300), &mut |_| {});
        assert!(matches!(slow, Err(SimError::Timeout { .. })), "{slow:?}");
        // A child that closes its stdout and keeps running.
        let started = Instant::now();
        let quiet = SystemRunner.run_lines("/bin/sh", &["-c", "exec >&-; sleep 5"], Duration::from_millis(300), &mut |_| {});
        assert!(matches!(quiet, Err(SimError::Timeout { .. })), "{quiet:?}");
        // An endless stream.
        let endless = SystemRunner.run_lines("/usr/bin/yes", &[], Duration::from_millis(300), &mut |_| {});
        assert!(matches!(endless, Err(SimError::Timeout { .. })), "{endless:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "both stopped at their deadline: {:?}", started.elapsed());
    }

    #[test]
    #[cfg(unix)]
    fn system_runner_captures_output_and_exit_code() {
        let out = SystemRunner.run("/bin/sh", &["-c", "echo hi; echo err >&2; exit 3"], None, Duration::from_secs(5)).unwrap();
        assert_eq!(out.status, Some(3));
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    #[cfg(unix)]
    fn system_runner_feeds_stdin() {
        let out = SystemRunner.run("/bin/cat", &[], Some(b"piped"), Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, b"piped");
    }

    #[test]
    #[cfg(unix)]
    fn system_runner_times_out_and_kills() {
        let start = Instant::now();
        let err = SystemRunner.run("/bin/sleep", &["5"], None, Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, SimError::Timeout { .. }), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn scripted_runner_answers_in_order_and_rejects_surprises() {
        let r = ScriptedRunner::default().expect("a b", CmdOutput::ok("x"));
        assert_eq!(r.run("a", &["b"], None, Duration::ZERO).unwrap().stdout, b"x");
        assert!(r.run("a", &["b"], None, Duration::ZERO).is_err());
        assert_eq!(r.calls(), vec!["a b", "a b"]);
    }

    #[test]
    #[cfg(unix)]
    fn a_bounded_run_keeps_at_most_the_cap_and_stops_at_the_deadline() {
        let done = run_bounded("sh", &["-c", "echo out; echo err >&2; exit 3"], Duration::from_secs(10), 1024).unwrap();
        assert_eq!((done.status, done.stdout.as_slice(), done.stderr.as_slice()), (Some(3), &b"out\n"[..], &b"err\n"[..]));
        assert!(!done.truncated && !done.timed_out);
        // Never ends on its own: cut at the cap, and stopped.
        let flood = run_bounded("yes", &[], Duration::from_secs(10), 4096).unwrap();
        assert!(flood.truncated && !flood.timed_out);
        assert_eq!(flood.stdout.len() + flood.stderr.len(), 4096);
        assert_eq!(flood.status, None);
        // Quiet and endless: what it wrote, at the deadline.
        let started = Instant::now();
        let slow = run_bounded("sh", &["-c", "echo first; sleep 30"], Duration::from_millis(300), 1024).unwrap();
        assert!(slow.timed_out && !slow.truncated);
        assert_eq!(slow.stdout, b"first\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A command that leaves a child in the background holding its pipes:
    /// the call still returns at once, and the child does not outlive it.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_background_child_does_not_outlive_the_call() {
        let started = Instant::now();
        let done = run_bounded("sh", &["-c", "sleep 30 & echo $!"], Duration::from_secs(10), 1024).unwrap();
        assert_eq!(done.status, Some(0), "its own exit, not the kill");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let pid: libc::pid_t = String::from_utf8_lossy(&done.stdout).trim().parse().unwrap();
        // SAFETY: signal 0 only asks whether the pid exists.
        let gone = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            (unsafe { libc::kill(pid, 0) }) == -1
        });
        assert!(gone, "the background sleep was stopped");
    }
}

