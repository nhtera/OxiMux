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

    #[test]
    fn captures_stdout_and_exit_code() {
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "echo hello"],
            Duration::from_secs(5),
        )
        .expect("runs");
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[test]
    fn captures_stderr_and_nonzero_exit() {
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "echo boom >&2; exit 3"],
            Duration::from_secs(5),
        )
        .expect("runs");
        assert!(!out.success());
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stderr.trim(), "boom");
    }

    #[test]
    fn a_hung_child_times_out_instead_of_blocking() {
        // The load-bearing case: a wedged driver must surface as an error, not
        // as a frozen caller.
        let started = Instant::now();
        let err = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "sleep 30"],
            Duration::from_millis(150),
        )
        .expect_err("must time out");
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
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "yes abcdefghij | head -c 300000"],
            Duration::from_secs(10),
        )
        .expect("runs");
        assert!(out.success());
        assert_eq!(out.stdout.len(), 300_000);
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
