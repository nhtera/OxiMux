//! Running a project's `setup` script as part of worktree provisioning.
//!
//! This is the inverse of [`crate::run_cleanup_before_remove`] in the one way
//! that matters. Cleanup must never trap the user: a teardown script that
//! fails, hangs, or does not exist is logged and the removal proceeds anyway.
//! Setup is the opposite — a worktree whose dependencies did not install is not
//! a worktree the user can work in, and reporting it as created is the lie this
//! phase exists to stop telling. So a non-zero exit, a timeout, or a failure to
//! start each fail creation and roll the worktree back.
//!
//! Everything else is deliberately the same shape as cleanup — `sh -lc`, cwd at
//! the worktree, `no_window` so Windows shows no console flash, `kill_on_drop`
//! so the timeout escape actually kills the child rather than orphaning it.
//! The two differences from cleanup are both consequences of failure mattering:
//!
//! - **Output is captured, not discarded.** Cleanup throws its output away
//!   because nothing can act on it. Here the user needs to read the compiler
//!   error, so stdout and stderr stream into a transcript as they arrive.
//! - **stdin is closed.** A setup script that prompts (`npm login`, a
//!   passphrase) would otherwise block until [`SETUP_TIMEOUT`] with no
//!   indication why. With stdin at `/dev/null` it fails immediately and the
//!   transcript shows the prompt it died on.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::ProvisionEvent;

/// Max time to wait for a project's `setup` script.
///
/// Two orders of magnitude above `CLEANUP_TIMEOUT` (30s) because the two do
/// different work: teardown stops things, setup installs dependencies. Fifteen
/// minutes covers a cold `cargo build` or a full `pnpm install` on a slow link
/// and still bounds a script that is genuinely wedged.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Cap on captured output. A setup script is not required to be well-behaved:
/// a progress bar redrawing with `\r` never emits a newline, and a verbose
/// build can print hundreds of megabytes. The transcript is held in memory and
/// then moved into `CreateOutcome`, so an uncapped capture is an
/// out-of-memory bug wearing a log file's clothes. Past the cap the tail is
/// kept and the middle is dropped, because the last lines are the diagnosis.
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// Cap on one line. `read_until` returns on a newline, so output that only ever
/// emits `\r` would otherwise accumulate the entire run as a single unbounded
/// line — nothing reaching the sink and nothing reaching the transcript until
/// the process ended.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// How the setup script ended. Every variant except [`Self::Ok`] fails
/// worktree creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    Ok,
    /// Ran to completion and reported failure. `code` is `None` when the
    /// process was terminated by a signal.
    NonZero { code: Option<i32> },
    /// Exceeded [`SETUP_TIMEOUT`] and was killed.
    TimedOut,
    /// Could not be launched at all (no shell, bad cwd).
    FailedToStart(String),
}

impl SetupOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// One line for a toast or a log — the transcript carries the detail.
    pub fn summary(&self) -> String {
        match self {
            Self::Ok => "setup succeeded".to_string(),
            Self::NonZero { code: Some(c) } => format!("setup exited {c}"),
            Self::NonZero { code: None } => "setup was terminated by a signal".to_string(),
            Self::TimedOut => format!(
                "setup did not finish within {} minutes and was killed",
                SETUP_TIMEOUT.as_secs() / 60
            ),
            Self::FailedToStart(err) => format!("setup could not start: {err}"),
        }
    }
}

/// The script, everything it printed, and how it ended. Kept whole so the
/// desktop can re-open it after creation rather than only showing it live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupTranscript {
    pub script: String,
    pub output: String,
    pub outcome: SetupOutcome,
}

/// Run `script` to completion in `worktree`, bounded by `timeout`.
///
/// Lines are appended to the returned transcript *and* forwarded to `sink` as
/// they arrive, so a watching UI sees progress rather than a spinner. Output
/// captured before a timeout is kept — the last few lines are usually the whole
/// diagnosis of what wedged.
pub async fn run_setup_bounded(
    worktree: &Path,
    script: &str,
    timeout: Duration,
    sink: Option<&UnboundedSender<ProvisionEvent>>,
) -> SetupTranscript {
    let mut cmd = tokio::process::Command::new("sh");
    {
        use oximux_no_window::NoWindow as _;
        cmd.arg("-lc")
            .arg(script)
            .current_dir(worktree)
            // Closed on purpose: a prompt must fail fast, not hang for 15 min.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .no_window()
            .kill_on_drop(true);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            return SetupTranscript {
                script: script.to_string(),
                output: String::new(),
                outcome: SetupOutcome::FailedToStart(err.to_string()),
            };
        }
    };

    // Merge stdout and stderr into one ordered stream: the user reads a
    // transcript, and interleaving is what makes an error legible next to the
    // command that produced it.
    //
    // Merged with `select!` rather than a task per pipe so the whole read is
    // one future the timeout can cancel as a unit. Two `tokio::spawn`ed
    // forwarders would outlive that cancellation and keep draining a child the
    // caller has already given up on.
    //
    // **Both pipes must keep being read until EOF.** A pipe abandoned early
    // fills its ~64 KiB buffer, the child blocks in `write()`, and `wait()`
    // never returns — a script that actually exited in seconds is then
    // reported as a 15-minute timeout. That is why reading is byte-oriented:
    // `Lines` fails the whole pipe with `InvalidData` on one non-UTF-8 byte,
    // which is exactly how that deadlock gets in.
    let mut out_pipe = child.stdout.take().map(Pipe::new);
    let mut err_pipe = child.stderr.take().map(Pipe::new);

    // Accumulated outside the timed future so a timeout keeps what was printed
    // before the hang — the last lines are usually the whole diagnosis.
    let mut output = String::new();
    let mut truncated = false;
    let outcome = match tokio::time::timeout(timeout, async {
        loop {
            let line = tokio::select! {
                Some(line) = next_line(&mut out_pipe) => line,
                Some(line) = next_line(&mut err_pipe) => line,
                else => break,
            };
            if let Some(sink) = sink {
                let _ = sink.send(ProvisionEvent::SetupLine(line.clone()));
            }
            if output.len() + line.len() < MAX_OUTPUT_BYTES {
                output.push_str(&line);
                output.push('\n');
            } else if !truncated {
                truncated = true;
                output.push_str("[output truncated; the transcript file has the rest]\n");
            }
        }
        child.wait().await
    })
    .await
    {
        Ok(Ok(status)) if status.success() => SetupOutcome::Ok,
        Ok(Ok(status)) => SetupOutcome::NonZero { code: status.code() },
        Ok(Err(err)) => SetupOutcome::FailedToStart(err.to_string()),
        Err(_) => SetupOutcome::TimedOut,
    };

    SetupTranscript {
        script: script.to_string(),
        output,
        outcome,
    }
}

/// One captured pipe, plus the partial line read so far.
///
/// The buffer lives here rather than inside the read future because
/// `read_until` is not cancel-safe on its own, and `select!` drops the losing
/// branch's future on every iteration. Owned out here, a cancelled read simply
/// resumes into the same buffer instead of losing what it had.
struct Pipe<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Pipe<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buf: Vec::new(),
        }
    }
}

/// One line from a pipe that may already be exhausted or absent.
///
/// Returns `None` — which disables that `select!` branch — only once the pipe
/// is truly done, so the loop ends when *both* are drained.
///
/// Decoding is lossy and never fatal. A build tool emitting one stray byte of
/// latin-1 must not cost us the rest of its output, and must never cost us the
/// pipe: see the deadlock note at the call site.
async fn next_line<R>(pipe: &mut Option<Pipe<R>>) -> Option<String>
where
    R: AsyncRead + Unpin,
{
    let p = pipe.as_mut()?;
    // A line long past the cap is a progress bar redrawing with `\r`, not a
    // line. Cut it so it reaches the transcript instead of growing forever.
    if p.buf.len() >= MAX_LINE_BYTES {
        return Some(take_line(&mut p.buf));
    }
    match p.reader.read_until(b'\n', &mut p.buf).await {
        // EOF. Emit a trailing partial line if the child never wrote a final
        // newline, then retire the branch.
        Ok(0) => {
            let rest = take_line(&mut p.buf);
            *pipe = None;
            (!rest.is_empty()).then_some(rest)
        }
        Ok(_) => Some(take_line(&mut p.buf)),
        // A real I/O error (the pipe is gone). Nothing more will arrive on it,
        // so retiring it here cannot strand a writing child.
        Err(err) => {
            *pipe = None;
            Some(format!("[output stream ended: {err}]"))
        }
    }
}

/// Drain the buffer into a `String`, dropping the line terminator.
fn take_line(buf: &mut Vec<u8>) -> String {
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    let line = String::from_utf8_lossy(buf).into_owned();
    buf.clear();
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wt() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn a_successful_script_reports_ok_and_captures_stdout() {
        let dir = wt();
        let t = run_setup_bounded(dir.path(), "echo hello", SETUP_TIMEOUT, None).await;
        assert_eq!(t.outcome, SetupOutcome::Ok);
        assert!(t.output.contains("hello"), "{:?}", t.output);
    }

    /// The inversion of cleanup's policy, asserted directly: a non-zero exit is
    /// a failure the caller must act on, not a warning to log past.
    #[tokio::test]
    async fn a_non_zero_exit_is_reported_as_a_failure() {
        let dir = wt();
        let t = run_setup_bounded(dir.path(), "exit 3", SETUP_TIMEOUT, None).await;
        assert_eq!(t.outcome, SetupOutcome::NonZero { code: Some(3) });
        assert!(!t.outcome.is_ok());
    }

    #[tokio::test]
    async fn stderr_is_captured_alongside_stdout() {
        let dir = wt();
        let t = run_setup_bounded(dir.path(), "echo oops 1>&2; exit 1", SETUP_TIMEOUT, None).await;
        assert!(t.output.contains("oops"), "{:?}", t.output);
        assert_eq!(t.outcome, SetupOutcome::NonZero { code: Some(1) });
    }

    #[tokio::test]
    async fn the_script_runs_in_the_worktree() {
        let dir = wt();
        std::fs::write(dir.path().join("marker"), "").unwrap();
        let t = run_setup_bounded(dir.path(), "test -f marker", SETUP_TIMEOUT, None).await;
        assert_eq!(t.outcome, SetupOutcome::Ok);
    }

    /// A hang is reported as a failure at the bound, not as a hang.
    #[tokio::test]
    async fn a_hanging_script_times_out_and_is_killed() {
        let dir = wt();
        let start = Instant::now();
        let t = run_setup_bounded(dir.path(), "sleep 60", Duration::from_millis(300), None).await;
        assert_eq!(t.outcome, SetupOutcome::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "should return at the bound, took {:?}",
            start.elapsed()
        );
    }

    /// Output printed before the hang survives the timeout — it is usually the
    /// only evidence of what the script was doing when it wedged.
    ///
    /// The bound is the test's whole wall time, and it has to cover the
    /// script *reaching* the echo: the runner spawns a login shell (`sh -lc`),
    /// and on a cold Windows CI runner sourcing the profile alone has taken
    /// longer than the 500 ms this test used to allow — which failed an
    /// unrelated PR on the echo never having run. Five seconds is an order of
    /// magnitude over the slowest start seen; the sibling test above is the
    /// one that pins the timeout firing promptly.
    #[tokio::test]
    async fn output_before_a_timeout_is_kept() {
        let dir = wt();
        let start = Instant::now();
        let t = run_setup_bounded(
            dir.path(),
            "echo installing; sleep 60",
            Duration::from_secs(5),
            None,
        )
        .await;
        assert_eq!(t.outcome, SetupOutcome::TimedOut);
        assert!(t.output.contains("installing"), "{:?}", t.output);
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "should return at the bound, took {:?}",
            start.elapsed()
        );
    }

    /// stdin is closed, so a prompt fails immediately instead of consuming the
    /// whole timeout. With an inherited stdin this test would hang.
    #[tokio::test]
    async fn a_script_reading_stdin_fails_fast_rather_than_hanging() {
        let dir = wt();
        let start = Instant::now();
        let t = run_setup_bounded(dir.path(), "read line || exit 7", SETUP_TIMEOUT, None).await;
        assert_eq!(t.outcome, SetupOutcome::NonZero { code: Some(7) });
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "closed stdin should end it immediately, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn lines_stream_to_the_sink_as_they_arrive() {
        let dir = wt();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t = run_setup_bounded(dir.path(), "echo one; echo two", SETUP_TIMEOUT, Some(&tx)).await;
        assert_eq!(t.outcome, SetupOutcome::Ok);
        drop(tx);
        let mut seen = Vec::new();
        while let Ok(ProvisionEvent::SetupLine(line)) = rx.try_recv() {
            seen.push(line);
        }
        assert_eq!(seen, vec!["one".to_string(), "two".to_string()]);
    }

    /// The deadlock guard. Non-UTF-8 output used to fail the whole pipe, which
    /// retired it while the child was still writing — the buffer filled, the
    /// child blocked, and a script that exited in milliseconds was reported as
    /// a 15-minute timeout. The bytes must come through lossily instead.
    #[tokio::test]
    async fn non_utf8_output_does_not_abandon_the_pipe() {
        let dir = wt();
        let start = Instant::now();
        let t = run_setup_bounded(
            dir.path(),
            // A lone 0xFF, then plenty more output, then a clean exit.
            "printf 'start\\n'; printf '\\377'; printf '\\nafter\\n'; exit 0",
            Duration::from_secs(20),
            None,
        )
        .await;
        assert_eq!(t.outcome, SetupOutcome::Ok, "output: {:?}", t.output);
        assert!(t.output.contains("start"), "{:?}", t.output);
        assert!(
            t.output.contains("after"),
            "output after the bad byte must survive: {:?}",
            t.output
        );
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// A child that writes more than a pipe buffer must not wedge. Without both
    /// pipes being drained to EOF this blocks until the timeout.
    #[tokio::test]
    async fn a_child_writing_past_the_pipe_buffer_still_completes() {
        let dir = wt();
        let start = Instant::now();
        let t = run_setup_bounded(
            dir.path(),
            // ~600 KiB across both pipes — an order of magnitude over the
            // ~64 KiB pipe buffer that would strand a half-drained child.
            "i=0; while [ $i -lt 3000 ]; do echo \"stdout line $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; echo \"stderr line $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\" 1>&2; i=$((i+1)); done; exit 0",
            Duration::from_secs(30),
            None,
        )
        .await;
        assert_eq!(t.outcome, SetupOutcome::Ok);
        assert!(t.output.contains("stdout line 2999"), "stdout truncated early");
        assert!(t.output.contains("stderr line 2999"), "stderr truncated early");
        assert!(start.elapsed() < Duration::from_secs(25));
    }

    /// Output with no trailing newline still reaches the transcript.
    #[tokio::test]
    async fn a_final_line_without_a_newline_is_kept() {
        let dir = wt();
        let t = run_setup_bounded(dir.path(), "printf 'no-newline'", SETUP_TIMEOUT, None).await;
        assert_eq!(t.outcome, SetupOutcome::Ok);
        assert!(t.output.contains("no-newline"), "{:?}", t.output);
    }

    #[test]
    fn take_line_strips_both_terminators_and_clears() {
        let mut buf = b"hello\r\n".to_vec();
        assert_eq!(take_line(&mut buf), "hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn every_failure_has_a_readable_summary() {
        assert!(SetupOutcome::NonZero { code: Some(2) }.summary().contains('2'));
        assert!(SetupOutcome::TimedOut.summary().contains("15 minutes"));
        assert!(
            SetupOutcome::FailedToStart("no sh".into())
                .summary()
                .contains("no sh")
        );
    }
}
