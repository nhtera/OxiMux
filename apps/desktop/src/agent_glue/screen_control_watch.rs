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

use crate::platform::screen_control_indicator::ScreenControlIndicator;

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
        loop {
            cx.background_executor().timer(TICK).await;

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

            cx.update_global::<Indicator, _>(|indicator, _| indicator.0.update(&driving));
        }
    })
    .detach();
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
