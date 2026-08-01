//! Bounded subprocess execution.
//!
//! Every call into the driver goes through here. The driver talks to a daemon
//! that is in turn talking to arbitrary GUI apps, and a hung or modal target is
//! an *expected* state rather than an edge case — a spinning beachball on the
//! app under test must not become a spinning beachball in OxiMux.
//!
//! Same reasoning as `relay-client`'s `REQUEST_TIMEOUT`: the caller may be the
//! GPUI main thread, so an unbounded wait freezes the whole UI. The timeout
//! converts that into a recoverable error.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::Error;

/// How often the wait loop checks whether the child has exited. Small enough
/// that a fast command (the common case — `status` and `--version` return in
/// milliseconds) is not padded by a whole poll interval.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A finished subprocess run.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `program` with `args`, killing it if it outruns `timeout`.
///
/// stdout and stderr are drained by dedicated threads rather than read after
/// the wait: a child that fills a pipe buffer blocks on write, and a parent
/// that is polling `try_wait` instead of reading would deadlock against it.
/// Draining concurrently means the child can always make progress, so the
/// timeout measures the child being slow rather than the two of us deadlocked.
pub fn run_bounded(program: &Path, args: &[&str], timeout: Duration) -> Result<Output, Error> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(source) => {
                return Err(Error::Spawn {
                    program: program.display().to_string(),
                    source,
                });
            }
        }
        if Instant::now() >= deadline {
            // Best-effort: if the kill fails the child is already gone, which
            // is the outcome we wanted anyway. Reap it so it does not linger
            // as a zombie.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Timeout {
                program: program.display().to_string(),
                timeout,
            });
        }
        thread::sleep(POLL_INTERVAL);
    };

    Ok(Output {
        code,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

/// Spawn a reader thread for one pipe, handing back the channel it reports on.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<mpsc::Receiver<String>> {
    let mut pipe = pipe?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        // A read error here is not worth surfacing: it means the pipe closed
        // under us, and whatever arrived before that is still the useful part.
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    Some(rx)
}

/// Collect a drained pipe. The child has already exited or been killed by the
/// time this runs, so the reader thread is finishing or finished; `recv` here
/// cannot outlive the pipe.
fn collect(rx: Option<mpsc::Receiver<String>>) -> String {
    rx.and_then(|rx| rx.recv().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform shell, and how to hand it one inline command.
    ///
    /// These tests exercise `run_bounded` itself — pipes, timeouts, exit codes
    /// — none of which is platform-specific, but every one of them needs *some*
    /// child process to drive. The shell is scaffolding, so it varies per
    /// platform while the assertions do not.
    ///
    /// `cmd` rather than PowerShell deliberately: it starts in milliseconds
    /// where `powershell.exe` takes a large fraction of a second, and
    /// `a_hung_child_times_out_instead_of_blocking` measures a 150 ms deadline.
    fn shell() -> (&'static Path, &'static str) {
        #[cfg(windows)]
        {
            (Path::new("cmd"), "/C")
        }
        #[cfg(not(windows))]
        {
            (Path::new("/bin/sh"), "-c")
        }
    }

    /// Run one inline shell command through `run_bounded`.
    fn sh(script: &str, timeout: Duration) -> Result<Output, Error> {
        let (program, flag) = shell();
        run_bounded(program, &[flag, script], timeout)
    }

    #[test]
    fn captures_stdout_and_exit_code() {
        let out = sh("echo hello", Duration::from_secs(5)).expect("runs");
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[test]
    fn captures_stderr_and_nonzero_exit() {
        // `echo boom>&2` without a space before `>`: cmd would otherwise take
        // the trailing space as part of the echoed text.
        #[cfg(windows)]
        let script = "echo boom>&2& exit 3";
        #[cfg(not(windows))]
        let script = "echo boom >&2; exit 3";

        let out = sh(script, Duration::from_secs(5)).expect("runs");
        assert!(!out.success());
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stderr.trim(), "boom");
    }

    #[test]
    fn a_hung_child_times_out_instead_of_blocking() {
        // The load-bearing case: a wedged driver must surface as an error, not
        // as a frozen caller.
        //
        // `pause` reads stdin, which `run_bounded` sets to null — that returns
        // EOF immediately instead of hanging. `timeout /T` refuses to run
        // without a console. Pinging loopback is the reliable way to make cmd
        // sit still for longer than the deadline.
        #[cfg(windows)]
        let script = "ping -n 30 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let script = "sleep 30";

        let started = Instant::now();
        let err = sh(script, Duration::from_millis(150)).expect_err("must time out");
        assert!(matches!(err, Error::Timeout { .. }), "got {err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "returned after {:?} — the timeout did not fire",
            started.elapsed()
        );
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        // Regression guard for the drain-concurrently design: 64 KiB+ of
        // output would wedge a try_wait loop that read the pipe afterwards.
        // Windows pipe buffers are the same order of magnitude as POSIX ones,
        // so this guards the same failure on both.
        //
        // Asserts a floor rather than an exact length: the two shells differ in
        // line endings, and what is being tested is that a large stream drains
        // at all, not how it is punctuated.
        #[cfg(windows)]
        let script = "for /L %i in (1,1,30000) do @echo abcdefghij";
        #[cfg(not(windows))]
        let script = "yes abcdefghij | head -c 300000";

        let out = sh(script, Duration::from_secs(30)).expect("runs");
        assert!(out.success());
        assert!(
            out.stdout.len() >= 300_000,
            "drained only {} bytes",
            out.stdout.len()
        );
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let err = run_bounded(
            Path::new("/nonexistent/cua-driver"),
            &[],
            Duration::from_secs(1),
        )
        .expect_err("must fail");
        assert!(matches!(err, Error::Spawn { .. }), "got {err:?}");
    }
}
