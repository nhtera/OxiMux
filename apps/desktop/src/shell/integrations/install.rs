//! Running a package manager on the user's behalf, and saying what happened.
//!
//! The shape is deliberately the same as [`crate::shell::driver_install`]: a
//! background thread does the work, the UI holds a small state enum and polls.
//! What is different is what can go wrong. The driver install talks to one
//! known server; this shells out to whatever package manager the machine has,
//! and the interesting failures are the boring ones — needs elevation, package
//! id moved, no network. So the failure state carries the manager's **own**
//! words rather than a category, and the pane always keeps the manual route
//! visible beside the button.
//!
//! One install at a time, per tool. A second click while one is running is a
//! no-op rather than a second process: package managers take a machine-wide
//! lock, and two racing installs produce an error that describes neither.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use oximux_no_window::NoWindow as _;

use super::catalog::Recipe;

/// Longest an install may run before it is abandoned.
///
/// Ten minutes is absurd for a CLI download and deliberately so: the cap is
/// here to stop a wedged manager holding a thread for the life of the app, not
/// to second-guess a slow link. A user on hotel wifi installing `git` should
/// finish; a `winget` blocked on an invisible UAC prompt should not wait
/// forever.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How much of the manager's output is kept for the failure message.
///
/// The tail, not the head: package managers print progress first and the
/// reason last, so the head of a failed install is a download bar.
const KEPT_OUTPUT_BYTES: usize = 2000;

/// What the pane renders for one tool's install affordance.
///
/// There is no `Idle` variant: idle is the *absence* of an entry in the pane's
/// map, which is the state almost every row is in almost always. A variant for
/// it would mean seeding four of them on open and remembering to clear them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallUi {
    Running,
    /// The manager exited non-zero, or could not be run. Carries its own text.
    Failed { message: String },
}

/// A running install this pane owns.
pub(crate) struct InstallHandle {
    rx: Receiver<Result<(), String>>,
    cancel: Arc<AtomicBool>,
}

impl InstallHandle {
    /// Ask the install to stop at its next checkpoint.
    ///
    /// Best-effort by nature: the child is killed, but a package manager that
    /// has already begun writing files will have written some of them. The
    /// pane says so rather than implying a clean rollback.
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Start `recipe` on a background thread.
///
/// Returns the handle to poll. The command is not shell-interpreted — program
/// and arguments go to the OS directly — so nothing here can be turned into a
/// second command by a package id containing a space or a semicolon.
pub(crate) fn begin(recipe: Recipe) -> InstallHandle {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = cancel.clone();
    std::thread::spawn(move || {
        let _ = tx.send(run(&recipe, &cancel_for_thread));
    });
    InstallHandle { rx, cancel }
}

/// Read the outcome if the install has finished.
///
/// `None` means still running. A disconnected channel with no value is
/// reported as a failure rather than silently treated as success: the thread
/// panicked, and calling that "installed" would leave the pane claiming a tool
/// is ready when nothing ran.
pub(crate) fn poll(handle: &InstallHandle) -> Option<Result<(), String>> {
    match handle.rx.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            Some(Err("the installer stopped without reporting a result".into()))
        }
    }
}

/// The blocking half. Runs on the spawned thread only.
fn run(recipe: &Recipe, cancel: &AtomicBool) -> Result<(), String> {
    let mut child = Command::new(recipe.manager)
        .args(&recipe.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .no_window()
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", recipe.manager))?;

    let deadline = std::time::Instant::now() + INSTALL_TIMEOUT;
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Install cancelled. Anything already written was left in place.".into());
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} did not finish within 10 minutes and was stopped.",
                        recipe.manager
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(err) => return Err(format!("lost track of {}: {err}", recipe.manager)),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("could not read {}'s output: {err}", recipe.manager))?;
    if output.status.success() {
        return Ok(());
    }
    Err(failure_message(
        recipe.manager,
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
        &String::from_utf8_lossy(&output.stdout),
    ))
}

/// Build the message a failed install shows.
///
/// Pure, because this is the part a user actually reads and the part most
/// likely to be wrong. Prefers stderr, falls back to stdout — winget in
/// particular reports its refusals on stdout — and if both are empty says so
/// plainly instead of rendering an empty red box.
fn failure_message(manager: &str, code: Option<i32>, stderr: &str, stdout: &str) -> String {
    let said = if !stderr.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    let said = tail(said.trim(), KEPT_OUTPUT_BYTES);
    if said.is_empty() {
        return match code {
            Some(code) => format!("{manager} exited with status {code} and said nothing."),
            None => format!("{manager} was stopped before it finished."),
        };
    }
    said
}

/// The last `limit` bytes of `text`, cut on a character boundary and marked as
/// truncated.
fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| text.len() - i <= limit)
        .unwrap_or(0);
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_is_preferred_but_stdout_is_not_ignored() {
        assert_eq!(
            failure_message("winget", Some(1), "needs elevation", "downloading…"),
            "needs elevation"
        );
        // winget reports several refusals on stdout with an empty stderr.
        assert_eq!(
            failure_message("winget", Some(1), "  \n ", "No package found matching input"),
            "No package found matching input"
        );
    }

    #[test]
    fn a_silent_failure_still_says_something() {
        let message = failure_message("brew", Some(3), "", "");
        assert!(message.contains("brew"));
        assert!(message.contains('3'), "the exit code is all we have: {message}");
    }

    #[test]
    fn a_killed_manager_is_not_reported_as_an_exit_code() {
        let message = failure_message("winget", None, "", "");
        assert!(
            message.contains("stopped"),
            "no exit code means it did not exit: {message}"
        );
    }

    #[test]
    fn long_output_keeps_the_end_where_the_reason_is() {
        let noise = "progress ".repeat(600);
        let text = format!("{noise}the actual reason");
        let cut = tail(&text, 100);
        assert!(cut.ends_with("the actual reason"));
        assert!(cut.starts_with('…'));
        assert!(cut.len() <= 110);
    }

    #[test]
    fn short_output_is_left_alone() {
        assert_eq!(tail("brief", 100), "brief");
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        // A byte-offset cut through a multi-byte character panics.
        let text = "é".repeat(400);
        let cut = tail(&text, 100);
        assert!(cut.chars().count() > 1);
    }

    #[test]
    fn a_manager_that_does_not_exist_fails_rather_than_hangs() {
        let recipe = Recipe {
            manager: "definitely-not-a-real-package-manager",
            args: vec!["install".to_string()],
        };
        let cancel = AtomicBool::new(false);
        let err = run(&recipe, &cancel).expect_err("nothing to run");
        assert!(err.contains("could not run"), "{err}");
    }

    #[test]
    fn a_disconnected_channel_is_a_failure_not_a_success() {
        // Simulates the installer thread dying without sending.
        let (tx, rx) = channel::<Result<(), String>>();
        drop(tx);
        let handle = InstallHandle {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let outcome = poll(&handle).expect("disconnected resolves immediately");
        assert!(
            outcome.is_err(),
            "claiming success here would report a tool ready that never installed"
        );
    }

    #[test]
    fn an_unfinished_install_reports_nothing_yet() {
        let (_tx, rx) = channel::<Result<(), String>>();
        let handle = InstallHandle {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        assert!(poll(&handle).is_none());
    }
}
