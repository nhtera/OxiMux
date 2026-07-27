//! The menu-bar dot that appears while an agent can drive the screen.
//!
//! # Why the menu bar and not a panel in the window
//!
//! The one moment this indicator has to work is the moment OxiMux is *not* what
//! the user is looking at — an agent driving Safari means Safari is frontmost.
//! Anything drawn inside our own window is invisible exactly then, which makes
//! it an indicator for the case that did not need one.
//!
//! The menu bar is the cheapest surface that is visible over another app's
//! full-screen window, and it is where macOS itself puts its screen-recording
//! and microphone indicators — so it reads as a system-level signal rather than
//! as one app's chrome, which is the right register for this.
//!
//! # What it deliberately is not
//!
//! No state machine, no ticker of its own, no click handling. It is a mirror:
//! the caller hands it the current [`Driving`] and it makes the menu bar match.
//! [`update`](ScreenControlIndicator::update) is idempotent and cheap enough to
//! call on every poll, so nothing has to track whether it is already showing.
//!
//! The menu's rows are informational and therefore disabled — deliberately.
//! Stopping an agent is Escape's job, and a clickable "Stop" here would be a
//! second, slower path to the same thing that only works when the user can find
//! the mouse.

use oximux_computer_use::Driving;

/// The line that has to survive being the only thing the user reads.
const STOP_HINT: &str = "Press Esc to stop";
/// Said in the same breath as the stop hint, always.
///
/// Escape drops every grant instantly, so nothing *new* can be driven — but the
/// driver is a separate process and a call already sent to it runs to
/// completion. Letting the user believe an in-flight click was recalled is the
/// one way a working kill switch still gets someone hurt.
const IN_FLIGHT_CAVEAT: &str = "An action already sent may still finish";

/// Whether Escape can actually stop anything, and if not, why not.
///
/// Carried into the copy rather than assumed, because both failures are silent
/// from the tap's own side and neither is the user's fault to guess at.
/// Promising a panic key that does nothing is worse than admitting there is
/// none — and a reason the user can act on is better than either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeState {
    Armed,
    /// No Accessibility permission, so no tap could be created at all.
    NotPermitted,
    /// A tap exists, but macOS is withholding keyboard events from every tap on
    /// the machine. Nothing about the tap reveals this, which is why it is
    /// asked about separately.
    SecureInput,
}

impl EscapeState {
    fn hint(self) -> &'static str {
        match self {
            EscapeState::Armed => STOP_HINT,
            EscapeState::NotPermitted => {
                "Esc cannot stop this — allow OxiMux under Privacy & Security › Accessibility"
            }
            // Names the cause rather than a fix, because the fix depends on who
            // is holding it: closing a password field, or locking and
            // unlocking the screen when something has left it stuck on.
            EscapeState::SecureInput => {
                "Esc cannot stop this while macOS secure input is on"
            }
        }
    }
}

/// What the menu bar should say for a given state, split out from AppKit so the
/// copy is testable — the strings are the entire user-facing product here, and
/// an indicator that says the wrong thing is worse than none.
struct Wording {
    /// Next to the dot. Short: this sits in a bar competing with the clock.
    title: String,
    /// Hover text, and the fallback for anyone who never opens the menu.
    tooltip: String,
    /// Menu rows, top to bottom. Ends with how to stop, and what stopping does
    /// not undo.
    rows: Vec<String>,
}

impl Wording {
    fn for_driving(driving: &Driving, escape: EscapeState) -> Self {
        let summary = driving.summary();
        let apps: Vec<&str> = driving
            .sessions
            .iter()
            .flat_map(|session| session.apps.iter().map(String::as_str))
            .collect();

        // One app is worth naming outright — it is the whole answer, and a bare
        // dot would make the user open the menu to learn it.
        let title = match distinct(&apps).as_slice() {
            [one] => format!("● {one}"),
            other => format!("● {} apps", other.len()),
        };

        let mut rows = vec![summary.clone()];
        rows.extend(driving.detail_lines());
        rows.push(escape.hint().to_string());
        rows.push(IN_FLIGHT_CAVEAT.to_string());

        Self {
            title,
            tooltip: format!("{summary}. {}. {IN_FLIGHT_CAVEAT}.", escape.hint()),
            rows,
        }
    }
}

fn distinct<'a>(apps: &[&'a str]) -> Vec<&'a str> {
    let mut apps = apps.to_vec();
    apps.sort_unstable();
    apps.dedup();
    apps
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
    use objc2_foundation::NSString;

    use super::Wording;

    /// The live status item, if one is showing.
    ///
    /// Held rather than looked up because `NSStatusBar` has no "find my item"
    /// call — dropping the handle without removing it leaks a permanent dot in
    /// the user's menu bar, which for a *safety* indicator is the worst possible
    /// leak: it would claim an agent is driving long after none is.
    #[derive(Default)]
    pub struct ScreenControlIndicator {
        item: Option<Retained<NSStatusItem>>,
        /// The copy currently on screen, so an unchanged poll does no AppKit
        /// work at all. The poll runs about once a second for as long as an
        /// agent drives.
        showing: Option<Vec<String>>,
    }

    impl ScreenControlIndicator {
        pub fn new() -> Self {
            Self::default()
        }

        /// Make the menu bar match `driving`. Idempotent.
        ///
        /// Silently does nothing off the main thread rather than panicking:
        /// this is an indicator, and taking the app down to complain about
        /// where it was called from would be a far worse outcome than a missing
        /// dot.
        pub fn update(&mut self, driving: &super::Driving, escape: Option<super::EscapeState>) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            // Two guards that must agree: no state means nothing is being
            // driven, which is also what an idle `driving` means. Taking either
            // as reason to hide is what keeps a stale claim off the menu bar.
            let (false, Some(escape)) = (driving.is_idle(), escape) else {
                self.hide();
                return;
            };

            let copy = Wording::for_driving(driving, escape);
            if self.showing.as_ref() == Some(&copy.rows) {
                return;
            }

            let item = self.item.get_or_insert_with(|| {
                NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength)
            });

            if let Some(button) = item.button(mtm) {
                button.setTitle(&NSString::from_str(&copy.title));
                button.setToolTip(Some(&NSString::from_str(&copy.tooltip)));
            }

            let menu = NSMenu::new(mtm);
            for row in &copy.rows {
                let entry = NSMenuItem::new(mtm);
                entry.setTitle(&NSString::from_str(row));
                // Informational, so never clickable — see the module docs.
                entry.setEnabled(false);
                menu.addItem(&entry);
            }
            item.setMenu(Some(&menu));

            self.showing = Some(copy.rows);
        }

        /// Take the dot away. Safe to call when nothing is showing.
        pub fn hide(&mut self) {
            if let Some(item) = self.item.take() {
                NSStatusBar::systemStatusBar().removeStatusItem(&item);
            }
            self.showing = None;
        }
    }

    impl Drop for ScreenControlIndicator {
        fn drop(&mut self) {
            self.hide();
        }
    }
}

/// Everywhere else the indicator is a no-op that still compiles, so callers do
/// not grow `cfg` branches around a safety signal.
#[cfg(not(target_os = "macos"))]
mod imp {
    #[derive(Default)]
    pub struct ScreenControlIndicator;

    impl ScreenControlIndicator {
        pub fn new() -> Self {
            Self
        }
        pub fn update(&mut self, _driving: &super::Driving, _escape: Option<super::EscapeState>) {}
        pub fn hide(&mut self) {}
    }
}

pub use imp::ScreenControlIndicator;

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_computer_use::DrivingSession;

    fn driving(sessions: &[(&str, &[&str])]) -> Driving {
        Driving {
            sessions: sessions
                .iter()
                .map(|(label, apps)| DrivingSession {
                    label: (*label).to_string(),
                    apps: apps.iter().map(|a| (*a).to_string()).collect(),
                })
                .collect(),
        }
    }

    fn armed(sessions: &[(&str, &[&str])]) -> Wording {
        Wording::for_driving(&driving(sessions), EscapeState::Armed)
    }

    #[test]
    fn one_app_is_named_in_the_bar_itself() {
        // The whole answer fits, so making the user open a menu to get it would
        // be a wasted interaction at the moment they are most alarmed.
        assert_eq!(armed(&[("chat-1", &["Safari"])]).title, "● Safari");
    }

    #[test]
    fn several_apps_are_counted_because_the_bar_has_no_room() {
        assert_eq!(armed(&[("chat-1", &["Safari", "Notes"])]).title, "● 2 apps");
    }

    #[test]
    fn the_same_app_under_two_agents_counts_once_in_the_bar() {
        // Two agents each driving their own build of one program is the shape
        // this feature exists for; "2 apps" would name something untrue.
        let copy = armed(&[("chat-1", &["my-app"]), ("chat-2", &["my-app"])]);
        assert_eq!(copy.title, "● my-app");
    }

    #[test]
    fn how_to_stop_is_always_said_and_always_qualified() {
        // The two rows that must survive the user reading only the end of the
        // menu: what stops it, and what stopping does not undo.
        for state in [
            vec![("chat-1", &["Safari"][..])],
            vec![("chat-1", &["Safari"][..]), ("chat-2", &["Notes"][..])],
        ] {
            let copy = armed(&state);
            assert_eq!(copy.rows.last().map(String::as_str), Some(IN_FLIGHT_CAVEAT));
            assert!(copy.rows.iter().any(|r| r == STOP_HINT), "{:?}", copy.rows);
            assert!(copy.tooltip.contains(STOP_HINT), "{}", copy.tooltip);
            assert!(copy.tooltip.contains(IN_FLIGHT_CAVEAT), "{}", copy.tooltip);
        }
    }

    #[test]
    fn an_unarmed_escape_says_so_rather_than_promising_a_key_that_does_nothing() {
        // Both failures are silent from the tap's side. Claiming otherwise is
        // the one that gets someone hurt while they are pressing a key they
        // were told would work.
        for state in [EscapeState::NotPermitted, EscapeState::SecureInput] {
            let copy = Wording::for_driving(&driving(&[("chat-1", &["Safari"])]), state);
            assert!(
                !copy.rows.iter().any(|r| r == STOP_HINT),
                "{state:?}: {:?}",
                copy.rows
            );
            assert!(!copy.tooltip.contains(STOP_HINT), "{state:?}: {}", copy.tooltip);
        }
    }

    #[test]
    fn each_reason_escape_is_dead_is_named_distinctly() {
        // "Esc does not work" without a cause is unactionable, and the two
        // causes need opposite responses — one is a permission the user grants
        // once, the other is transient and not theirs to fix.
        assert!(
            EscapeState::NotPermitted.hint().contains("Accessibility"),
            "{}",
            EscapeState::NotPermitted.hint()
        );
        assert!(
            EscapeState::SecureInput.hint().contains("secure input"),
            "{}",
            EscapeState::SecureInput.hint()
        );
        assert_ne!(
            EscapeState::NotPermitted.hint(),
            EscapeState::SecureInput.hint()
        );
    }

    #[test]
    fn several_agents_each_get_a_row() {
        let copy = armed(&[("chat-1", &["Safari"]), ("chat-2", &["Notes"])]);
        assert!(copy.rows.iter().any(|r| r.contains("chat-1")), "{:?}", copy.rows);
        assert!(copy.rows.iter().any(|r| r.contains("chat-2")), "{:?}", copy.rows);
    }

    #[test]
    fn the_tooltip_names_the_app_without_opening_the_menu() {
        let copy = armed(&[("chat-1", &["Safari"])]);
        assert!(copy.tooltip.contains("Safari"), "{}", copy.tooltip);
    }

    /// Constructing and tearing down the real status item, which is the part
    /// that leaks a permanent dot in the user's menu bar if `Drop` is wrong.
    /// Off the main thread `update` is a no-op by design, so this asserts the
    /// contract holds either way rather than that a dot appeared.
    #[test]
    fn showing_and_hiding_never_panics() {
        let mut indicator = ScreenControlIndicator::new();
        let live = driving(&[("chat-1", &["Safari"])]);
        indicator.update(&live, Some(EscapeState::Armed));
        indicator.update(&live, Some(EscapeState::Armed));
        indicator.update(&live, Some(EscapeState::SecureInput));
        // No state and no grants: both ways of saying nothing is happening.
        indicator.update(&live, None);
        indicator.update(&Driving::default(), Some(EscapeState::Armed));
        indicator.hide();
    }
}
