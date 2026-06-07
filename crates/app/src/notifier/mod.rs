//! Agent notification dispatch — platform-agnostic surface.
//!
//! The badge alone covers the in-app case; this module covers the
//! out-of-app case: a desktop notification fires when an agent transitions
//! into a notify-worthy state (`NeedsApproval`, `WaitingForInput`, `Done`,
//! `Failed`) — by default only while the OxiMux window is not the active
//! one. Clicking the notification brings the window forward and switches
//! the workspace tab to the originating agent.
//!
//! Layout:
//!   - `Notifier` trait — synchronous surface so a `MockNotifier` in tests
//!     stays trivial; the macOS impl handles its own `spawn_blocking`.
//!   - `NotificationKind` — which lifecycle edge fired; maps to title + sound.
//!   - `AgentNotifySettings` — live (atomic) per-kind enable + sound + focus
//!     gate, shared between the settings pane and the notifier.
//!   - `TabId` — opaque newtype routing a stable agent-session handle from
//!     the badge subscription through the notifier and back into the GPUI
//!     workspace-tab activation path.
//!   - `SuppressMap` — per-(tab, kind) dedupe so an adapter that re-emits
//!     its prompt on every output chunk produces one notification.
//!   - `notification_kind_for_transition` — pure edge detector; tested.
//!
//! Platform impls live in sibling modules: `mac` (active on
//! `target_os = "macos"`) and `null` (no-op fallback).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oximux_core::{AgentSessionId, AgentStatus};

pub mod null;

#[cfg(target_os = "macos")]
pub mod mac;

/// Minimum interval between two notifications for the same (tab, kind).
/// An adapter that re-prints its prompt on every output chunk (or one that
/// races the status detector) must not be allowed to spam.
pub const SUPPRESS_WINDOW: Duration = Duration::from_secs(30);

/// Stable per-spawn identifier carried through the notification round-trip:
/// status watcher → notifier → OS click → workspace tab activation.
///
/// Wraps the runtime-minted `AgentSessionId` counter so the value survives
/// tab reorder and tab rename (both of which shift the strip index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

impl From<AgentSessionId> for TabId {
    fn from(s: AgentSessionId) -> Self {
        TabId(s.get())
    }
}

/// Which agent lifecycle edge a notification represents. Drives the banner
/// title, the chosen system sound, and the per-kind enable check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    /// Agent paused on a dangerous-action approval prompt.
    NeedsApproval,
    /// Agent paused on a generic input prompt.
    WaitingForInput,
    /// Agent finished successfully.
    Done,
    /// Agent exited with an error.
    Failed,
}

impl NotificationKind {
    /// The notify-worthy kind for a status, or `None` for transient states
    /// (`Idle`, `Running`, `Interrupted`).
    pub fn from_status(status: &AgentStatus) -> Option<Self> {
        match status {
            AgentStatus::NeedsApproval(_) => Some(Self::NeedsApproval),
            AgentStatus::WaitingForInput => Some(Self::WaitingForInput),
            AgentStatus::Done { .. } => Some(Self::Done),
            AgentStatus::Failed(_) => Some(Self::Failed),
            _ => None,
        }
    }

    /// macOS system sound name, used only when sound is enabled.
    pub fn sound_name(self) -> &'static str {
        match self {
            Self::NeedsApproval | Self::WaitingForInput => "Ping",
            Self::Done => "Glass",
            Self::Failed => "Basso",
        }
    }

    /// Banner title for this edge, given the agent's tab label.
    pub fn title(self, agent_label: &str) -> String {
        match self {
            Self::NeedsApproval => format!("Agent needs approval — {agent_label}"),
            Self::WaitingForInput => format!("Agent waiting for input — {agent_label}"),
            Self::Done => format!("Agent finished — {agent_label}"),
            Self::Failed => format!("Agent failed — {agent_label}"),
        }
    }
}

/// Live notification preferences shared between the settings pane and the
/// notifier. `AtomicBool` fields so the pane can flip a value while per-tab
/// status tasks / the notifier read it lock-free.
#[derive(Debug)]
pub struct AgentNotifySettings {
    pub needs_approval: AtomicBool,
    pub waiting_input: AtomicBool,
    pub done: AtomicBool,
    pub failed: AtomicBool,
    pub sound: AtomicBool,
    /// When true (default), only notify while the window is unfocused.
    pub only_when_unfocused: AtomicBool,
}

impl AgentNotifySettings {
    pub fn new(
        needs_approval: bool,
        waiting_input: bool,
        done: bool,
        failed: bool,
        sound: bool,
        only_when_unfocused: bool,
    ) -> Self {
        Self {
            needs_approval: AtomicBool::new(needs_approval),
            waiting_input: AtomicBool::new(waiting_input),
            done: AtomicBool::new(done),
            failed: AtomicBool::new(failed),
            sound: AtomicBool::new(sound),
            only_when_unfocused: AtomicBool::new(only_when_unfocused),
        }
    }

    /// Quiet defaults (honoring the design ethos): approval / done / failed
    /// banners on; waiting-input + sound off; only while unfocused.
    pub fn defaults() -> Self {
        Self::new(true, false, true, true, false, true)
    }

    /// Build from a key→value getter (e.g. a `SettingsRepo` lookup), falling
    /// back to [`defaults`](Self::defaults) per field. Values parse as the
    /// strings `"true"` / `"false"`; keeps this type free of a storage dep.
    pub fn from_getter(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::defaults();
        let read = |key: &str, fallback: &AtomicBool| match get(key).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => fallback.load(Ordering::Relaxed),
        };
        Self::new(
            read(keys::NEEDS_APPROVAL, &d.needs_approval),
            read(keys::WAITING_INPUT, &d.waiting_input),
            read(keys::DONE, &d.done),
            read(keys::FAILED, &d.failed),
            read(keys::SOUND, &d.sound),
            read(keys::ONLY_WHEN_UNFOCUSED, &d.only_when_unfocused),
        )
    }

    /// Whether a banner should fire for `kind`, accounting for the per-kind
    /// toggle and the focus gate.
    pub fn should_fire(&self, kind: NotificationKind, window_active: bool) -> bool {
        if self.only_when_unfocused.load(Ordering::Relaxed) && window_active {
            return false;
        }
        let flag = match kind {
            NotificationKind::NeedsApproval => &self.needs_approval,
            NotificationKind::WaitingForInput => &self.waiting_input,
            NotificationKind::Done => &self.done,
            NotificationKind::Failed => &self.failed,
        };
        flag.load(Ordering::Relaxed)
    }

    pub fn sound_enabled(&self) -> bool {
        self.sound.load(Ordering::Relaxed)
    }
}

impl Default for AgentNotifySettings {
    fn default() -> Self {
        Self::defaults()
    }
}

/// `SettingsRepo` keys for the notification prefs (flat KV, `"true"`/`"false"`).
pub mod keys {
    pub const NEEDS_APPROVAL: &str = "notify.needs_approval";
    pub const WAITING_INPUT: &str = "notify.waiting_input";
    pub const DONE: &str = "notify.done";
    pub const FAILED: &str = "notify.failed";
    pub const SOUND: &str = "notify.sound";
    pub const ONLY_WHEN_UNFOCUSED: &str = "notify.only_when_unfocused";
}

/// Synchronous notification dispatch surface.
///
/// Impls that talk to the OS (`MacNotifier`) handle blocking calls
/// internally via `spawn_blocking`; the trait stays sync-only so a
/// `MockNotifier` in tests is one call-counter struct with no async harness.
pub trait Notifier: Send + Sync {
    /// Post a desktop notification for an agent lifecycle edge. The impl
    /// consults its shared [`AgentNotifySettings`] (per-kind enable, sound,
    /// and the focus gate via `window_active`), so callers always call this
    /// on a genuine edge and let the impl decide whether to surface it.
    /// Must be non-blocking from the caller's point of view. `body` is the
    /// short payload (e.g. `NeedsApproval` reason); may be empty.
    fn notify(
        &self,
        tab_id: TabId,
        kind: NotificationKind,
        agent_label: &str,
        body: &str,
        window_active: bool,
    );
}

/// The notification kind to fire for a `(prev → new)` status edge, or `None`
/// when `new` is not notify-worthy or is the same kind as `prev` (a no-edge
/// repeat — e.g. `NeedsApproval → NeedsApproval` even if the payload differs).
pub fn notification_kind_for_transition(
    prev: &AgentStatus,
    new: &AgentStatus,
) -> Option<NotificationKind> {
    let kind = NotificationKind::from_status(new)?;
    if NotificationKind::from_status(prev) == Some(kind) {
        return None;
    }
    Some(kind)
}

/// Per-(tab, kind) rate limiter for notification dispatch. The status
/// watcher consults this before calling [`Notifier::notify`]. Owned by the
/// per-tab `_status_task` closure — not shared across tabs.
pub struct SuppressMap {
    last_fired: HashMap<(TabId, NotificationKind), Instant>,
    window: Duration,
}

impl SuppressMap {
    pub fn new() -> Self {
        Self {
            last_fired: HashMap::new(),
            window: SUPPRESS_WINDOW,
        }
    }

    /// Returns true if a notification for `(tab_id, kind)` may fire at `now`.
    /// On a positive return the map records `now` so subsequent calls within
    /// `window` return false. Pass `Instant::now()` in production and a
    /// synthetic Instant in tests.
    pub fn should_fire(&mut self, tab_id: TabId, kind: NotificationKind, now: Instant) -> bool {
        match self.last_fired.get(&(tab_id, kind)) {
            Some(prev) if now.duration_since(*prev) < self.window => false,
            _ => {
                self.last_fired.insert((tab_id, kind), now);
                true
            }
        }
    }

    /// Forget the last-fired timestamp for `(tab_id, kind)`. Called when an
    /// agent leaves that state so the next entry can fire immediately rather
    /// than waiting out the rate-limit window.
    pub fn forget(&mut self, tab_id: TabId, kind: NotificationKind) {
        self.last_fired.remove(&(tab_id, kind));
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
        assert_eq!(
            notification_kind_for_transition(&AgentStatus::Idle, &na("write?")),
            Some(NotificationKind::NeedsApproval)
        );
    }

    #[test]
    fn fires_on_running_to_done_and_failed() {
        assert_eq!(
            notification_kind_for_transition(
                &AgentStatus::Running,
                &AgentStatus::Done { code: Some(0) }
            ),
            Some(NotificationKind::Done)
        );
        assert_eq!(
            notification_kind_for_transition(
                &AgentStatus::Running,
                &AgentStatus::Failed("boom".into())
            ),
            Some(NotificationKind::Failed)
        );
    }

    #[test]
    fn fires_on_running_to_waiting_for_input() {
        assert_eq!(
            notification_kind_for_transition(&AgentStatus::Running, &AgentStatus::WaitingForInput),
            Some(NotificationKind::WaitingForInput)
        );
    }

    #[test]
    fn does_not_fire_when_already_in_needs_approval() {
        // Adapter re-emits the same prompt; payload may even change. The
        // dedupe-by-kind guard prevents a second notification.
        assert_eq!(
            notification_kind_for_transition(&na("first?"), &na("second?")),
            None
        );
    }

    #[test]
    fn does_not_fire_for_transient_states() {
        assert_eq!(
            notification_kind_for_transition(&AgentStatus::Idle, &AgentStatus::Running),
            None
        );
    }

    #[test]
    fn fires_again_after_leaving_and_re_entering() {
        assert_eq!(
            notification_kind_for_transition(&na("a"), &AgentStatus::Running),
            None
        );
        assert_eq!(
            notification_kind_for_transition(&AgentStatus::Running, &na("b")),
            Some(NotificationKind::NeedsApproval)
        );
    }

    #[test]
    fn settings_focus_gate_blocks_when_active() {
        let s = AgentNotifySettings::defaults();
        // Approval is enabled, but the window is active and the gate is on.
        assert!(!s.should_fire(NotificationKind::NeedsApproval, true));
        assert!(s.should_fire(NotificationKind::NeedsApproval, false));
    }

    #[test]
    fn settings_per_kind_toggle() {
        let s = AgentNotifySettings::defaults();
        // Waiting-input is off by default; done is on.
        assert!(!s.should_fire(NotificationKind::WaitingForInput, false));
        assert!(s.should_fire(NotificationKind::Done, false));
    }

    #[test]
    fn settings_always_notify_when_gate_off() {
        let s = AgentNotifySettings::new(true, true, true, true, false, false);
        // only_when_unfocused = false → fire even with the window active.
        assert!(s.should_fire(NotificationKind::NeedsApproval, true));
    }

    #[test]
    fn suppress_map_fires_first_call() {
        let mut sm = SuppressMap::new();
        assert!(sm.should_fire(TabId(7), NotificationKind::NeedsApproval, Instant::now()));
    }

    #[test]
    fn suppress_map_blocks_within_window() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), NotificationKind::NeedsApproval, t0));
        let t1 = t0 + Duration::from_secs(10);
        assert!(!sm.should_fire(TabId(7), NotificationKind::NeedsApproval, t1));
    }

    #[test]
    fn suppress_map_independent_per_kind() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), NotificationKind::WaitingForInput, t0));
        // Same tab + instant, different kind — must fire independently so a
        // waiting-input banner never masks a later approval banner.
        assert!(sm.should_fire(TabId(7), NotificationKind::NeedsApproval, t0));
    }

    #[test]
    fn suppress_map_fires_after_window_elapses() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), NotificationKind::Done, t0));
        let t2 = t0 + Duration::from_secs(31);
        assert!(sm.should_fire(TabId(7), NotificationKind::Done, t2));
    }

    #[test]
    fn suppress_map_forget_reopens_fire_window() {
        let mut sm = SuppressMap::new();
        let t0 = Instant::now();
        assert!(sm.should_fire(TabId(7), NotificationKind::NeedsApproval, t0));
        let t1 = t0 + Duration::from_secs(10);
        assert!(!sm.should_fire(TabId(7), NotificationKind::NeedsApproval, t1));
        sm.forget(TabId(7), NotificationKind::NeedsApproval);
        assert!(sm.should_fire(TabId(7), NotificationKind::NeedsApproval, t1));
    }

    #[test]
    fn tab_id_from_session_id_roundtrips() {
        let sid = AgentSessionId::new(42);
        assert_eq!(TabId::from(sid), TabId(42));
    }
}
