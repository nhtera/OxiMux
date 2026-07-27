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
    let Some(path) = crate::shell::agent_chat::computer_use::grants_path() else {
        // No data dir is a broken install. Screen control still works against a
        // temp store; it is only the indicator that cannot find it, and a
        // missing dot is not worth failing a launch over.
        tracing::warn!("no app data dir; the screen-control indicator is off");
        return;
    };
    cx.set_global(Indicator(ScreenControlIndicator::new()));

    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut last = FileStamp::default();
        let mut showing = false;
        let mut stop = StopKey::default();
        loop {
            cx.background_executor().timer(TICK).await;

            if stop.pressed() {
                // Instant and app-wide: every screen-control call re-checks the
                // table, so clearing it is what actually takes the keys away.
                let aborted = path.clone();
                cx.background_executor()
                    .spawn(async move { GrantTable::at(&aborted).clear() })
                    .await;
                tracing::info!("Escape stopped screen control; all grants dropped");
                // Force the next pass to redraw rather than trust the stamp.
                last = FileStamp::default();
            }

            let probe = path.clone();
            let Some(driving) = cx
                .background_executor()
                .spawn(async move { tick(&probe, last, showing) })
                .await
            else {
                continue;
            };
            last = FileStamp::of(&path);
            showing = !driving.is_idle();
            let escape = stop.follow(showing);

            cx.update_global::<Indicator, _>(|indicator, _| {
                indicator.0.update(&driving, escape)
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
/// input method's candidate window, leaving a dialog, and vim, everywhere. Tied
/// to a held grant instead, the user loses Escape only while an agent actually
/// holds the right to click.
///
/// # The gap that costs
///
/// Arming happens on the tick that first *sees* a grant, so up to [`TICK`]
/// passes between an agent gaining the right to drive and Escape being able to
/// stop it. The provenance path grants inside the decision for the first tool
/// call, so that first action can land inside the gap. Closing it would mean
/// arming on "an agent could drive" rather than "an agent may drive" — which is
/// the always-on tap above, and a worse trade.
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
    /// Idle answers [`EscapeState::Unavailable`], which is simply true — there
    /// is no tap. It cannot mislead: the indicator hides on the same condition,
    /// so the permission warning has nowhere to appear.
    fn follow(&mut self, driving: bool) -> EscapeState {
        if !driving {
            self.tap = None;
            // Cleared, not latched: the user may have granted Accessibility
            // since the last run, and the next one should find out.
            self.blocked = false;
            return EscapeState::Unavailable;
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
        if self.tap.is_some() {
            EscapeState::Armed
        } else {
            EscapeState::Unavailable
        }
    }

    /// Has Escape been pressed since the last check? Clears the flag, so one
    /// press aborts once.
    fn pressed(&self) -> bool {
        self.tap.as_ref().is_some_and(EscapeTap::abort_requested)
    }
}

/// One pass' worth of file work, off the UI thread. `None` means nothing
/// changed and nothing is showing, so the tick has no work to do.
fn tick(path: &PathBuf, last: FileStamp, showing: bool) -> Option<Driving> {
    if !needs_full_read(FileStamp::of(path), last, showing) {
        return None;
    }
    Some(Driving::read(&GrantTable::at(path)))
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

    /// Whether the tap can be created depends on Accessibility permission,
    /// which CI does not have and a developer machine may not either. So these
    /// assert the *contract* rather than that arming succeeds — which is the
    /// stronger test anyway: the failure mode that matters is claiming Escape
    /// works when it does not.
    #[test]
    fn arming_is_tried_once_per_run_of_grants() {
        let mut stop = StopKey::default();
        let first = stop.follow(true);
        match first {
            EscapeState::Armed => {
                assert!(stop.tap.is_some());
                assert!(!stop.blocked, "a success must not latch a failure");
            }
            EscapeState::Unavailable => {
                assert!(stop.tap.is_none());
                assert!(stop.blocked, "a failure must latch, or every tick retries");
            }
        }
        assert_eq!(
            stop.follow(true),
            first,
            "a second tick must not change the answer"
        );
    }

    #[test]
    fn going_idle_drops_the_tap_and_forgets_a_failure() {
        // Dropping matters most: a tap left armed swallows every Escape on the
        // machine, including an input method's candidate dismissal. Forgetting
        // the failure matters because the user may have granted the permission
        // in between.
        let mut stop = StopKey::default();
        stop.follow(true);
        assert_eq!(stop.follow(false), EscapeState::Unavailable);
        assert!(stop.tap.is_none());
        assert!(!stop.blocked);
    }

    #[test]
    fn nothing_is_pending_until_a_key_is_pressed() {
        let mut stop = StopKey::default();
        assert!(!stop.pressed(), "no tap means nothing to drain");
        stop.follow(true);
        assert!(!stop.pressed());
    }

    #[test]
    fn a_tick_reads_a_real_store_when_something_is_showing() {
        // The exit-detection path: nothing changed on disk, but the read still
        // has to happen so a dead target stops being named.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("computer-use-grants.json");
        std::fs::write(&path, "{}").expect("write");
        let driving = tick(&path, FileStamp::of(&path), true).expect("a read was due");
        assert!(driving.is_idle());
    }
}
