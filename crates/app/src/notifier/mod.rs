//! Agent notification dispatch — platform-agnostic surface.
//!
//! The badge alone covers the in-app case; this module covers the
//! out-of-app case: a desktop notification fires when an agent transitions
//! into `NeedsApproval(_)` while the OxiMux window is not the active one.
//! Clicking the notification brings the window forward and switches the
//! workspace tab to the originating agent.
//!
//! Layout:
//!   - `Notifier` trait — synchronous surface so a `MockNotifier` in tests
//!     stays trivial; the macOS impl handles its own `spawn_blocking`.
//!   - `TabId` — opaque newtype routing a stable agent-session handle from
//!     the badge subscription through the notifier and back into the GPUI
//!     workspace-tab activation path.
//!   - `SuppressMap` — per-tab dedupe so an adapter that re-emits its
//!     approval prompt on every output chunk produces one notification.
//!   - `should_notify_transition` — pure edge detector; tested in isolation.
//!
//! Platform impls live in sibling modules: `mac` (active on
//! `target_os = "macos"`) and `null` (no-op fallback).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use oximux_core::{AgentSessionId, AgentStatus};

pub mod null;

#[cfg(target_os = "macos")]
pub mod mac;

/// Minimum interval between two notifications for the same tab.
/// An adapter that re-prints its approval prompt on every output chunk
/// (or one that races the status detector) must not be allowed to spam.
pub const SUPPRESS_WINDOW: Duration = Duration::from_secs(30);

/// Stable per-spawn identifier carried through the notification round-trip:
/// status watcher → notifier → OS click → workspace tab activation.
///
/// Wraps the runtime-minted `AgentSessionId` counter so the value survives
/// tab reorder and tab rename (both of which shift the strip index).
/// Inner `u64` is the raw counter; exposed so the click router on the
/// GPUI side can pack the value across the `mpsc::UnboundedSender<u64>`
/// bridge without leaking `TabId` into the cross-thread payload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

impl From<AgentSessionId> for TabId {
    fn from(s: AgentSessionId) -> Self {
        TabId(s.get())
    }
}

/// Synchronous notification dispatch surface.
///
/// Impls that talk to the OS (`MacNotifier`) handle blocking calls
/// internally via `spawn_blocking`; the trait stays sync-only so a
/// `MockNotifier` in tests is one call-counter struct with no async harness.
pub trait Notifier: Send + Sync {
    /// Post a "this agent needs your approval" notification. Implementations
    /// must be non-blocking from the caller's point of view (push work onto
    /// a background thread or task if the OS API blocks). `reason` is the
    /// short payload from `AgentStatus::NeedsApproval(_)` and may be empty.
    fn notify_needs_approval(&self, tab_id: TabId, agent_label: &str, reason: &str);
}

/// True when the (prev, new) pair represents an edge the user should be
/// notified about: any state → NeedsApproval is the only firing pattern.
///
/// Subsequent NeedsApproval → NeedsApproval polls produce no edge here
/// even when the payload string differs, because the user is already
/// being asked something and the dedupe window (`SuppressMap`) will catch
/// any badge re-emission noise.
pub fn should_notify_transition(prev: &AgentStatus, new: &AgentStatus) -> bool {
    !matches!(prev, AgentStatus::NeedsApproval(_)) && matches!(new, AgentStatus::NeedsApproval(_))
}

/// Per-tab rate limiter for notification dispatch. The status watcher
/// consults this map before calling `Notifier::notify_needs_approval`.
/// Owned by the per-tab `_status_task` closure — not shared across tabs.
pub struct SuppressMap {
    last_fired: HashMap<TabId, Instant>,
    window: Duration,
}

impl SuppressMap {
    pub fn new() -> Self {
        Self {
            last_fired: HashMap::new(),
            window: SUPPRESS_WINDOW,
        }
    }

    /// Returns true if a notification for `tab_id` may fire at `now`.
    /// On a positive return the map records `now` so subsequent calls
    /// within `window` return false. Pass `Instant::now()` in production
    /// and a synthetic Instant in tests; threading the clock through the
    /// parameter avoids a `Box<dyn Fn>` field for one production caller.
    pub fn should_fire(&mut self, tab_id: TabId, now: Instant) -> bool {
        match self.last_fired.get(&tab_id) {
            Some(prev) if now.duration_since(*prev) < self.window => false,
            _ => {
                self.last_fired.insert(tab_id, now);
                true
            }
        }
    }

    /// Forget the last-fired timestamp for `tab_id`. Called by the status
    /// task when an agent leaves `NeedsApproval` so the next entry into
    /// that state can fire immediately rather than waiting for the
    /// rate-limit window to elapse. Without this reset, a user who
    /// answers an approval at T=5 and faces a fresh approval at T=25
    /// would see no notification — the 30 s window would still be open.
    pub fn forget(&mut self, tab_id: TabId) {
        self.last_fired.remove(&tab_id);
    }
}

impl Default for SuppressMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn na(reason: &str) -> AgentStatus {
        AgentStatus::NeedsApproval(reason.into())
    }

    #[test]
    fn fires_on_idle_to_needs_approval() {
        assert!(should_notify_transition(&AgentStatus::Idle, &na("write?")));
    }

    #[test]
    fn fires_on_running_to_needs_approval() {
        assert!(should_notify_transition(
            &AgentStatus::Running,
            &na("write?")
        ));
    }

    #[test]
    fn fires_on_waiting_for_input_to_needs_approval() {
        // WaitingForInput is a generic prompt; NeedsApproval is the
        // dangerous-action prompt. The transition is meaningful.
        assert!(should_notify_transition(
            &AgentStatus::WaitingForInput,
            &na("rm -rf /?")
        ));
    }

    #[test]
    fn does_not_fire_when_already_in_needs_approval() {
        // Adapter re-emits the same prompt; payload may even change. The
        // dedupe-by-state guard prevents a second notification before the
        // SuppressMap window even gets consulted.
        assert!(!should_notify_transition(&na("first?"), &na("second?")));
    }

    #[test]
    fn does_not_fire_for_other_terminal_transitions() {
        assert!(!should_notify_transition(
            &AgentStatus::Running,
            &AgentStatus::Done { code: Some(0) }
        ));
        assert!(!should_notify_transition(
            &AgentStatus::Running,
            &AgentStatus::Failed("boom".into())
        ));
        assert!(!should_notify_transition(
            &AgentStatus::Running,
            &AgentStatus::WaitingForInput
        ));
    }

    #[test]
    fn fires_again_after_leaving_and_re_entering() {
        // User answers the prompt; agent resumes; new approval needed later.
        assert!(!should_notify_transition(&na("a"), &AgentStatus::Running));
        assert!(should_notify_transition(&AgentStatus::Running, &na("b")));
    }

    #[test]
    fn suppress_map_fires_first_call() {
        let mut sm = SuppressMap::new();
        assert!(sm.should_fire(TabId(7), Instant::now()));
    }

    #[test]
    fn suppress_map_blocks_within_window() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), t0));
        // 10 s later — inside the 30 s window.
        let t1 = t0 + Duration::from_secs(10);
        assert!(!sm.should_fire(TabId(7), t1));
    }

    #[test]
    fn suppress_map_fires_after_window_elapses() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), t0));
        let t2 = t0 + Duration::from_secs(31);
        assert!(sm.should_fire(TabId(7), t2));
    }

    #[test]
    fn suppress_map_is_per_tab() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(1), t0));
        // Same instant, different tab — must fire independently.
        assert!(sm.should_fire(TabId(2), t0));
    }

    #[test]
    fn suppress_map_forget_reopens_fire_window() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), t0));
        // Inside the window — would normally be suppressed.
        let t1 = t0 + Duration::from_secs(10);
        assert!(!sm.should_fire(TabId(7), t1));
        // Status task observed the tab leaving NeedsApproval and called
        // forget. A fresh approval now must fire even inside the window.
        sm.forget(TabId(7));
        assert!(sm.should_fire(TabId(7), t1));
    }

    #[test]
    fn tab_id_from_session_id_roundtrips() {
        let sid = AgentSessionId::new(42);
        assert_eq!(TabId::from(sid), TabId(42));
    }
}
