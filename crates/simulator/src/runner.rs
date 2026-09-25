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
}

/// The real thing: `std::process` with a polling timeout.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
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
                    what: format!("{program} {}", args.join(" ")),
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
}

impl ScriptedRunner {
    pub fn new(script: impl IntoIterator<Item = Scripted>) -> Self {
        Self { script: Mutex::new(script.into_iter().collect()), calls: Mutex::default() }
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
}

impl Runner for ScriptedRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _stdin: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<CmdOutput> {
        let command = std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ");
        self.calls.lock().unwrap().push(command.clone());
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
}
