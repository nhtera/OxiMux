//! Keeps the menu-bar indicator honest about what agents are driving.
//!
//! # Why this polls rather than being told
//!
//! Grants are not created in one place. OxiMux records one when the user
//! approves a consent card — but the `PreToolUse` hook is a separate short-lived
//! process, and it resolves build provenance itself, so an agent driving a
//! binary it just built gets a grant this process never sees. Anything wired to
//! the approval path would therefore go dark for the single most common case
//! the feature has.
//!
//! The grant table is the one thing both writers share, so it is what gets read.
//!
//! # What idle costs
//!
//! Screen control is off by default and stays off for most users forever, so the
//! idle path has to be genuinely free. It is one `metadata()` call per tick: if
//! the grants file has not changed and nothing is on screen, the tick ends
//! there — no lock, no parse, no pid resolution.
//!
//! The full read only runs when the file changed or the indicator is already
//! showing. That second condition matters: a granted process can exit without
//! touching the file, and the indicator has to stop naming an app that is gone.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{App, AsyncApp, Global};
use oximux_computer_use::{Driving, GrantTable};

use crate::platform::escape_tap::{self, EscapeTap};
use crate::platform::screen_control_indicator::{EscapeState, ScreenControlIndicator};

/// How often to look. A second is fast enough that the dot appears while the
/// user is still watching the action that caused it, and slow enough that the
/// idle cost is a rounding error.
const TICK: Duration = Duration::from_secs(1);

/// The live indicator, parked as a global because it holds an AppKit object
/// that is neither `Send` nor safe to touch off the main thread — a GPUI global
/// is only ever reachable from there.
struct Indicator(ScreenControlIndicator);

impl Global for Indicator {}

/// What the last tick saw of the grants file, so an unchanged one costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    /// A missing file stamps as the default, which is also what an empty table
    /// looks like — both mean "nothing granted", and neither should redraw.
    fn of(path: &std::path::Path) -> Self {
        let Ok(meta) = std::fs::metadata(path) else {
            return Self::default();
        };
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// Whether this tick has to do the expensive read.
///
/// Split out from the loop because the reasoning is the whole point and is
/// otherwise invisible: skipping a redraw while something *is* on screen would
/// leave the user looking at a stale claim about their own machine.
fn needs_full_read(stamp: FileStamp, last: FileStamp, showing: bool) -> bool {
    stamp != last || showing
}

/// Start the watch. Detached: it runs for the process's lifetime and has no
/// owner to hang it off.
pub fn install(cx: &mut App) {
    // The same store the chats and the hook use, including its temp-dir fallback
    // — which is the case that matters, since a broken install is exactly when
    // an indicator that quietly watched a different file would go dark while
    // agents kept driving.
    let path = crate::shell::agent_chat::computer_use::grants_path();
    cx.set_global(Indicator(ScreenControlIndicator::new()));

    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut last = FileStamp::default();
        let mut showing = false;
        let mut stop = StopKey::default();
        loop {
            cx.background_executor().timer(TICK).await;

            if stop.pressed() {
                // Instant and app-wide: every screen-control call re-checks the
                // table, so writing the abort is what actually takes the keys
                // away. Dropping grants alone would stop only input — reading
                // needs no grant, so the agent could go on photographing the
                // screen it was just stopped from touching.
                let aborted = path.clone();
                let dropped = cx
                    .background_executor()
                    .spawn(async move { GrantTable::at(&aborted).abort() })
                    .await;
                if dropped {
                    tracing::info!("Escape stopped screen control for every active turn");
                } else {
                    // The user pressed the kill switch and it did not take. Of
                    // everything here this is the one that must never be logged
                    // at info alongside the successes.
                    tracing::error!(
                        ?path,
                        "Escape could not drop screen-control grants; agents may still be driving"
                    );
                }
                // Force the next pass to redraw rather than trust the stamp.
                last = FileStamp::default();
            }

            let probe = path.clone();
            let Some(reading) = cx
                .background_executor()
                .spawn(async move { tick(&probe, last, showing) })
                .await
            else {
                continue;
            };
            last = FileStamp::of(&path);
            showing = !reading.driving.is_idle();
            let escape = stop.follow(showing, reading.secure_input);

            cx.update_global::<Indicator, _>(|indicator, _| {
                indicator.0.update(&reading.driving, escape)
            });
        }
    })
    .detach();
}

/// The Escape tap, armed only while something is actually being driven.
///
/// # Why it is not simply always on
///
/// The tap swallows *every* Escape on the machine. Armed for the life of a chat
/// that merely could drive — potentially hours — it would break dismissing an
/// input method's candidate window, leaving a dialog, and vim, everywhere.
///
/// # Why a held grant is not the signal
///
/// The obvious reading of "is an agent driving" is "does an agent hold a grant",
/// and it is wrong by a wide margin. A grant is *consent*, and consent is
/// answered once per chat so the user is not asked again every turn — so a chat
/// that clicked once at breakfast still holds its grant at lunch. Keyed on that,
/// this tap is armed for exactly the "potentially hours" the paragraph above
/// rejects, and the menu bar spends those hours claiming apps are being driven
/// while the machine sits idle.
///
/// So the tap follows a separate, turn-scoped mark: set by the hook when a call
/// is actually allowed, cleared at the turn boundary. The user loses Escape
/// while an agent is driving, and gets it back when the turn ends.
///
/// # The gap that costs
///
/// Arming happens on the tick that first *sees* the mark, so up to [`TICK`]
/// passes between an agent's first allowed call and Escape being able to stop
/// it. Closing it would mean arming on "an agent could drive" rather than "an
/// agent is driving" — which is the always-on tap above, and a worse trade.
#[derive(Default)]
struct StopKey {
    /// `None` while idle, and also when arming failed — the two are
    /// distinguished by `blocked`, because only one of them is worth telling
    /// the user about.
    tap: Option<EscapeTap>,
    /// Arming was tried for this run of grants and did not work. Latched so a
    /// missing permission is reported once, not once a second.
    blocked: bool,
}

impl StopKey {
    /// Match the tap to whether anything is being driven, and report what the
    /// indicator may now claim.
    ///
    /// `None` when nothing is being driven: there is no tap, and the question
    /// the state answers does not arise. Returning a failure reason there would
    /// name a cause for a machine on which nothing is happening.
    fn follow(&mut self, driving: bool, secure_input: bool) -> Option<EscapeState> {
        if !driving {
            self.tap = None;
            // Cleared, not latched: the user may have granted the permission
            // since the last run, and the next one should find out.
            self.blocked = false;
            return None;
        }
        if self.tap.is_none() && !self.blocked {
            match escape_tap::arm() {
                Ok(tap) => self.tap = Some(tap),
                Err(err) => {
                    self.blocked = true;
                    tracing::warn!(%err, "Escape cannot stop screen control");
                }
            }
        }
        if self.tap.is_none() {
            return Some(EscapeState::NotPermitted);
        }
        // Checked after arming, not instead of it: secure input comes and goes
        // with whatever has focus, so a tap held through it starts working the
        // moment it lifts. Only the claim changes, never the tap.
        if secure_input {
            return Some(EscapeState::SecureInput);
        }
        Some(EscapeState::Armed)
    }

    /// Has Escape been pressed since the last check? Clears the flag, so one
    /// press aborts once.
    fn pressed(&self) -> bool {
        self.tap.as_ref().is_some_and(EscapeTap::abort_requested)
    }
}

/// What one pass learned about the world.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    driving: Driving,
    /// Whether macOS is withholding keys from every tap. Only asked when
    /// something is being driven — it costs an IORegistry search, and the
    /// answer is meaningless when there is no tap to be starved.
    secure_input: bool,
}

/// One pass' worth of off-thread work. `None` means nothing changed and nothing
/// is showing, so the tick has no work to do.
///
/// Both reads happen here rather than at the call site because both touch the
/// system — a file lock and a registry walk — and the call site runs on the
/// thread that paints.
fn tick(path: &PathBuf, last: FileStamp, showing: bool) -> Option<Reading> {
    if !needs_full_read(FileStamp::of(path), last, showing) {
        return None;
    }
    let driving = Driving::read(&GrantTable::at(path));
    let secure_input = !driving.is_idle() && crate::platform::secure_input::active();
    Some(Reading {
        driving,
        secure_input,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(len: u64) -> FileStamp {
        FileStamp {
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(len)),
            len,
        }
    }

    #[test]
    fn an_unchanged_file_with_nothing_on_screen_is_skipped() {
        // The path every user who never turns this on takes, every second, for
        // as long as the app runs.
        assert!(!needs_full_read(stamp(1), stamp(1), false));
    }

    #[test]
    fn a_changed_file_is_always_read() {
        assert!(needs_full_read(stamp(2), stamp(1), false));
    }

    #[test]
    fn an_unchanged_file_is_still_read_while_something_is_showing() {
        // A granted process can exit without touching the file. Skipping here
        // would leave the menu bar claiming an app is being driven after it has
        // quit — a stale safety signal, which is the one kind that matters.
        assert!(needs_full_read(stamp(1), stamp(1), true));
    }

    #[test]
    fn a_missing_file_stamps_the_same_as_an_absent_one() {
        // Grants are cleared at boot, so "no file" and "file we have not seen
        // yet" must not read as a change and force a pointless first redraw.
        let missing = FileStamp::of(std::path::Path::new("/nonexistent/computer-use-grants.json"));
        assert_eq!(missing, FileStamp::default());
        assert!(!needs_full_read(missing, FileStamp::default(), false));
    }

    #[test]
    fn a_tick_over_a_missing_store_does_no_work() {
        let path = PathBuf::from("/nonexistent/computer-use-grants.json");
        assert_eq!(tick(&path, FileStamp::default(), false), None);
    }

    /// Whether the tap can be created depends on Input Monitoring permission,
    /// which CI does not have and a developer machine may not either. So these
    /// assert the *contract* rather than that arming succeeds — which is the
    /// stronger test anyway: the failure mode that matters is claiming Escape
    /// works when it does not.
    #[test]
    fn arming_is_tried_once_per_run_of_grants() {
        let mut stop = StopKey::default();
        let first = stop.follow(true, false);
        match first {
            Some(EscapeState::Armed) => {
                assert!(stop.tap.is_some());
                assert!(!stop.blocked, "a success must not latch a failure");
            }
            Some(EscapeState::NotPermitted) => {
                assert!(stop.tap.is_none());
                assert!(stop.blocked, "a failure must latch, or every tick retries");
            }
            other => panic!("secure input was not reported, so cannot be it: {other:?}"),
        }
        assert_eq!(
            stop.follow(true, false),
            first,
            "a second tick must not change the answer"
        );
    }

    #[test]
    fn secure_input_is_reported_without_giving_up_the_tap() {
        // It comes and goes with whatever has focus, so a tap torn down for it
        // would have to be rebuilt the moment a password field lost focus —
        // and would be missing in the gap. Only the claim changes.
        let mut stop = StopKey::default();
        if stop.follow(true, false) != Some(EscapeState::Armed) {
            return; // No tap permission here; covered by the test above.
        }
        assert_eq!(stop.follow(true, true), Some(EscapeState::SecureInput));
        assert!(stop.tap.is_some(), "the tap must survive being starved");
        assert_eq!(
            stop.follow(true, false),
            Some(EscapeState::Armed),
            "and must be claimed working again the moment keys flow"
        );
    }

    #[test]
    fn going_idle_drops_the_tap_and_forgets_a_failure() {
        // Dropping matters most: a tap left armed swallows every Escape on the
        // machine, including an input method's candidate dismissal. Forgetting
        // the failure matters because the user may have granted the permission
        // in between.
        let mut stop = StopKey::default();
        stop.follow(true, false);
        assert_eq!(
            stop.follow(false, false),
            None,
            "nothing driving means the question does not arise"
        );
        assert!(stop.tap.is_none());
        assert!(!stop.blocked);
    }

    #[test]
    fn nothing_is_pending_until_a_key_is_pressed() {
        let mut stop = StopKey::default();
        assert!(!stop.pressed(), "no tap means nothing to drain");
        stop.follow(true, false);
        assert!(!stop.pressed());
    }

    #[test]
    fn a_tick_reads_a_real_store_when_something_is_showing() {
        // The exit-detection path: nothing changed on disk, but the read still
        // has to happen so a dead target stops being named.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("computer-use-grants.json");
        std::fs::write(&path, "{}").expect("write");
        let reading = tick(&path, FileStamp::of(&path), true).expect("a read was due");
        assert!(reading.driving.is_idle());
        assert!(
            !reading.secure_input,
            "with nothing driving there is no tap to starve, so the registry \
             is not walked and the answer stays false"
        );
    }
}
