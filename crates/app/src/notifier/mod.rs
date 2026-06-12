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

/// Burst-collapse window: a second notification for the same workspace
/// (any tab, any kind) within this span is dropped, so two agents
/// finishing back-to-back in one worktree produce one banner.
pub const BURST_WINDOW: Duration = Duration::from_secs(5);

/// Where a notification originated. Drives the per-source enable gate and
/// the stable-ID namespace. `TerminalBell` is reserved for the bell→OS
/// routing that lands with the terminal-polish round; the dispatcher and
/// settings gates already understand it so that wiring is one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationSource {
    /// Agent lifecycle edge (approval / input / done / failed).
    AgentState,
    /// "Send test notification" button in settings.
    Test,
    /// Terminal BEL routing (not wired yet; gate exists now).
    TerminalBell,
}

/// One notification dispatch, bundled so the `Notifier` trait surface
/// stays stable as gating inputs grow.
#[derive(Debug, Clone)]
pub struct NotificationRequest {
    pub source: NotificationSource,
    pub kind: NotificationKind,
    pub tab_id: TabId,
    /// Burst-dedup key — the worktree path of the originating workspace
    /// (or a synthetic key for non-agent sources).
    pub workspace_key: String,
    /// Tab label shown in the banner title.
    pub label: String,
    /// Short payload (e.g. approval reason); may be empty.
    pub body: String,
    /// True when the OxiMux window is frontmost.
    pub window_active: bool,
    /// True when the originating pane is the visible tab of a rendered
    /// group. A visible pane in a frontmost window never banners — the
    /// user is already looking at it.
    pub pane_visible: bool,
}

/// Per-workspace burst collapse (see [`BURST_WINDOW`]). Owned by the
/// notifier impl behind a mutex — shared across every tab's watcher so
/// cross-tab bursts in one worktree collapse too.
pub struct BurstGate {
    last_fired: HashMap<String, Instant>,
    window: Duration,
}

impl BurstGate {
    pub fn new() -> Self {
        Self {
            last_fired: HashMap::new(),
            window: BURST_WINDOW,
        }
    }

    /// True when a banner for `workspace_key` may fire at `now`; records
    /// `now` on a positive answer so the next call inside the window is
    /// collapsed.
    pub fn should_fire(&mut self, workspace_key: &str, now: Instant) -> bool {
        match self.last_fired.get(workspace_key) {
            Some(prev) if now.duration_since(*prev) < self.window => false,
            _ => {
                self.last_fired.insert(workspace_key.to_string(), now);
                true
            }
        }
    }
}

impl Default for BurstGate {
    fn default() -> Self {
        Self::new()
    }
}

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

/// What a clicked banner points at, decoded from the banner identifier's
/// namespace by the OS click delegate and routed to the window's
/// navigation handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    /// `agent:{tab}:{seq}` — an agent tab, in [`TabId`] space.
    AgentTab(TabId),
    /// `bell:{session}:{seq}` — the raw terminal session id whose pane
    /// rang the bell.
    TerminalSession(u64),
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
    /// Terminal bell (BEL) from a pane the user can't see. Only dispatched
    /// when the bell setting is `Notify`, so it has no per-kind toggle of
    /// its own — the source gate + that setting are the opt-ins.
    Bell,
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

    /// Banner title for this edge, given the agent's tab label.
    pub fn title(self, agent_label: &str) -> String {
        match self {
            Self::NeedsApproval => format!("Agent needs approval — {agent_label}"),
            Self::WaitingForInput => format!("Agent waiting for input — {agent_label}"),
            Self::Done => format!("Agent finished — {agent_label}"),
            Self::Failed => format!("Agent failed — {agent_label}"),
            Self::Bell => format!("Terminal bell — {agent_label}"),
        }
    }
}

/// Live notification preferences shared between the settings pane and the
/// notifier. `AtomicBool` fields so the pane can flip a value while per-tab
/// status tasks / the notifier read it lock-free.
#[derive(Debug)]
pub struct AgentNotifySettings {
    /// Master switch — off silences every source.
    pub enabled: AtomicBool,
    /// Per-source enables. The `Test` source has no gate by design: a
    /// toggle that disables the test button would only confuse.
    pub source_agent_state: AtomicBool,
    pub source_terminal_bell: AtomicBool,
    pub needs_approval: AtomicBool,
    pub waiting_input: AtomicBool,
    pub done: AtomicBool,
    pub failed: AtomicBool,
    pub sound: AtomicBool,
    /// When true (default), only notify while the window is unfocused.
    pub only_when_unfocused: AtomicBool,
    /// Hold a sleep assertion while any agent is running (see
    /// `agent_awake`). Lives here so the settings pane persists it with
    /// the other notification prefs.
    pub agent_awake: AtomicBool,
}

/// Plain-bool snapshot used to construct [`AgentNotifySettings`] — keeps
/// the constructor readable as the field count grows.
#[derive(Debug, Clone, Copy)]
pub struct NotifyPrefValues {
    pub enabled: bool,
    pub source_agent_state: bool,
    pub source_terminal_bell: bool,
    pub needs_approval: bool,
    pub waiting_input: bool,
    pub done: bool,
    pub failed: bool,
    pub sound: bool,
    pub only_when_unfocused: bool,
    pub agent_awake: bool,
}

impl Default for NotifyPrefValues {
    /// Quiet defaults (honoring the design ethos): approval / done / failed
    /// banners on; waiting-input + sound off; only while unfocused. Master
    /// switch + sources + agent-awake on.
    fn default() -> Self {
        Self {
            enabled: true,
            source_agent_state: true,
            source_terminal_bell: true,
            needs_approval: true,
            waiting_input: false,
            done: true,
            failed: true,
            sound: false,
            only_when_unfocused: true,
            agent_awake: true,
        }
    }
}

impl AgentNotifySettings {
    pub fn from_values(v: NotifyPrefValues) -> Self {
        Self {
            enabled: AtomicBool::new(v.enabled),
            source_agent_state: AtomicBool::new(v.source_agent_state),
            source_terminal_bell: AtomicBool::new(v.source_terminal_bell),
            needs_approval: AtomicBool::new(v.needs_approval),
            waiting_input: AtomicBool::new(v.waiting_input),
            done: AtomicBool::new(v.done),
            failed: AtomicBool::new(v.failed),
            sound: AtomicBool::new(v.sound),
            only_when_unfocused: AtomicBool::new(v.only_when_unfocused),
            agent_awake: AtomicBool::new(v.agent_awake),
        }
    }

    pub fn defaults() -> Self {
        Self::from_values(NotifyPrefValues::default())
    }

    /// Build from a key→value getter (e.g. a `SettingsRepo` lookup), falling
    /// back to [`defaults`](Self::defaults) per field. Values parse as the
    /// strings `"true"` / `"false"`; keeps this type free of a storage dep.
    pub fn from_getter(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = NotifyPrefValues::default();
        let read = |key: &str, fallback: bool| match get(key).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => fallback,
        };
        Self::from_values(NotifyPrefValues {
            enabled: read(keys::ENABLED, d.enabled),
            source_agent_state: read(keys::SOURCE_AGENT_STATE, d.source_agent_state),
            source_terminal_bell: read(keys::SOURCE_TERMINAL_BELL, d.source_terminal_bell),
            needs_approval: read(keys::NEEDS_APPROVAL, d.needs_approval),
            waiting_input: read(keys::WAITING_INPUT, d.waiting_input),
            done: read(keys::DONE, d.done),
            failed: read(keys::FAILED, d.failed),
            sound: read(keys::SOUND, d.sound),
            only_when_unfocused: read(keys::ONLY_WHEN_UNFOCUSED, d.only_when_unfocused),
            agent_awake: read(keys::AGENT_AWAKE, d.agent_awake),
        })
    }

    /// Whether a banner should fire, walking the gates in order: master
    /// switch → per-source enable → (for real sources) the hard
    /// visible-pane rule, the focus gate, and the per-kind toggle. The
    /// `Test` source skips the focus/kind gates so the settings button
    /// always demonstrates delivery, but still respects the master switch.
    pub fn should_fire(&self, req: &NotificationRequest) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let source_on = match req.source {
            NotificationSource::AgentState => self.source_agent_state.load(Ordering::Relaxed),
            NotificationSource::TerminalBell => self.source_terminal_bell.load(Ordering::Relaxed),
            NotificationSource::Test => true,
        };
        if !source_on {
            return false;
        }
        if req.source == NotificationSource::Test {
            return true;
        }
        // A pane the user can currently see never banners, regardless of
        // the focus-gate preference.
        if req.window_active && req.pane_visible {
            return false;
        }
        if self.only_when_unfocused.load(Ordering::Relaxed) && req.window_active {
            return false;
        }
        let flag = match req.kind {
            NotificationKind::NeedsApproval => &self.needs_approval,
            NotificationKind::WaitingForInput => &self.waiting_input,
            NotificationKind::Done => &self.done,
            NotificationKind::Failed => &self.failed,
            // Bell carries no per-kind toggle: the per-pane `Notify` bell
            // setting and the source gate above are the opt-ins.
            NotificationKind::Bell => return true,
        };
        flag.load(Ordering::Relaxed)
    }

    pub fn sound_enabled(&self) -> bool {
        self.sound.load(Ordering::Relaxed)
    }

    pub fn agent_awake_enabled(&self) -> bool {
        self.agent_awake.load(Ordering::Relaxed)
    }
}

impl Default for AgentNotifySettings {
    fn default() -> Self {
        Self::defaults()
    }
}

/// `SettingsRepo` keys for the notification prefs (flat KV, `"true"`/`"false"`).
pub mod keys {
    pub const ENABLED: &str = "notify.enabled";
    pub const SOURCE_AGENT_STATE: &str = "notify.source.agent_state";
    pub const SOURCE_TERMINAL_BELL: &str = "notify.source.terminal_bell";
    pub const NEEDS_APPROVAL: &str = "notify.needs_approval";
    pub const WAITING_INPUT: &str = "notify.waiting_input";
    pub const DONE: &str = "notify.done";
    pub const FAILED: &str = "notify.failed";
    pub const SOUND: &str = "notify.sound";
    pub const ONLY_WHEN_UNFOCUSED: &str = "notify.only_when_unfocused";
    pub const AGENT_AWAKE: &str = "notify.agent_awake";
}

/// Synchronous notification dispatch surface.
///
/// Impls that talk to the OS (`MacNotifier`) handle blocking calls
/// internally via `spawn_blocking`; the trait stays sync-only so a
/// `MockNotifier` in tests is one call-counter struct with no async harness.
/// Whether the OS notification path can deliver at all — drives the
/// settings-pane hint next to the test button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifierAvailability {
    Available,
    /// Process is not a `.app` bundle (bare `cargo run` dev binary).
    Unbundled,
    /// The user denied notification authorization for this app.
    PermissionDenied,
}

pub trait Notifier: Send + Sync {
    /// Post a desktop notification. The impl consults its shared
    /// [`AgentNotifySettings`] (master switch, per-source + per-kind
    /// enables, focus gate) and its [`BurstGate`], so callers call this on
    /// a genuine edge and let the impl decide whether to surface it. Must
    /// be non-blocking from the caller's point of view.
    fn notify(&self, req: NotificationRequest);

    /// Delivery health for UI hints. Defaults to available so mocks and
    /// the null notifier stay one-liners.
    fn availability(&self) -> NotifierAvailability {
        NotifierAvailability::Available
    }
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

    /// Request fixture: agent-state source, given kind + focus/visibility.
    fn req(kind: NotificationKind, window_active: bool, pane_visible: bool) -> NotificationRequest {
        NotificationRequest {
            source: NotificationSource::AgentState,
            kind,
            tab_id: TabId(7),
            workspace_key: "/tmp/wt".into(),
            label: "test-agent".into(),
            body: String::new(),
            window_active,
            pane_visible,
        }
    }

    #[test]
    fn settings_focus_gate_blocks_when_active() {
        let s = AgentNotifySettings::defaults();
        // Approval is enabled, but the window is active and the gate is on.
        assert!(!s.should_fire(&req(NotificationKind::NeedsApproval, true, false)));
        assert!(s.should_fire(&req(NotificationKind::NeedsApproval, false, false)));
    }

    #[test]
    fn settings_per_kind_toggle() {
        let s = AgentNotifySettings::defaults();
        // Waiting-input is off by default; done is on.
        assert!(!s.should_fire(&req(NotificationKind::WaitingForInput, false, false)));
        assert!(s.should_fire(&req(NotificationKind::Done, false, false)));
    }

    #[test]
    fn settings_always_notify_when_gate_off() {
        let s = AgentNotifySettings::from_values(NotifyPrefValues {
            waiting_input: true,
            only_when_unfocused: false,
            ..NotifyPrefValues::default()
        });
        // only_when_unfocused = false → fire even with the window active,
        // as long as the pane itself is not on screen.
        assert!(s.should_fire(&req(NotificationKind::NeedsApproval, true, false)));
    }

    #[test]
    fn settings_visible_pane_never_fires() {
        let s = AgentNotifySettings::from_values(NotifyPrefValues {
            only_when_unfocused: false,
            ..NotifyPrefValues::default()
        });
        // Even with the focus gate off, a pane the user can see is silent.
        assert!(!s.should_fire(&req(NotificationKind::Done, true, true)));
        // Visible pane in a *backgrounded* window still fires — the user
        // is not looking at the app.
        assert!(s.should_fire(&req(NotificationKind::Done, false, true)));
    }

    /// Bell request fixture: terminal-bell source, Bell kind.
    fn bell_req(window_active: bool, pane_visible: bool) -> NotificationRequest {
        let mut r = req(NotificationKind::Bell, window_active, pane_visible);
        r.source = NotificationSource::TerminalBell;
        r
    }

    #[test]
    fn bell_fires_without_a_per_kind_toggle() {
        // Defaults: master + bell source on, focus gate on. A bell from a
        // backgrounded window fires even though Bell has no kind toggle.
        let s = AgentNotifySettings::defaults();
        assert!(s.should_fire(&bell_req(false, false)));
        // Focus gate still applies — active window stays quiet.
        assert!(!s.should_fire(&bell_req(true, false)));
        // Visible pane in an active window is always silent.
        assert!(!s.should_fire(&bell_req(true, true)));
    }

    #[test]
    fn bell_source_gate_blocks_bell_only() {
        let s = AgentNotifySettings::from_values(NotifyPrefValues {
            source_terminal_bell: false,
            ..NotifyPrefValues::default()
        });
        assert!(!s.should_fire(&bell_req(false, false)));
        // Agent banners are unaffected by the bell source gate.
        assert!(s.should_fire(&req(NotificationKind::Done, false, false)));
    }

    #[test]
    fn settings_master_switch_silences_everything() {
        let s = AgentNotifySettings::from_values(NotifyPrefValues {
            enabled: false,
            ..NotifyPrefValues::default()
        });
        assert!(!s.should_fire(&req(NotificationKind::Failed, false, false)));
        let mut test_req = req(NotificationKind::Done, false, false);
        test_req.source = NotificationSource::Test;
        // The master switch gates the test button too.
        assert!(!s.should_fire(&test_req));
    }

    #[test]
    fn settings_source_gate_blocks_agent_state() {
        let s = AgentNotifySettings::from_values(NotifyPrefValues {
            source_agent_state: false,
            ..NotifyPrefValues::default()
        });
        assert!(!s.should_fire(&req(NotificationKind::Done, false, false)));
    }

    #[test]
    fn settings_test_source_bypasses_focus_and_kind_gates() {
        // Everything that could block a real banner is in its blocking
        // state; the test button still fires.
        let s = AgentNotifySettings::from_values(NotifyPrefValues {
            needs_approval: false,
            waiting_input: false,
            done: false,
            failed: false,
            only_when_unfocused: true,
            ..NotifyPrefValues::default()
        });
        let mut r = req(NotificationKind::Done, true, true);
        r.source = NotificationSource::Test;
        assert!(s.should_fire(&r));
    }

    #[test]
    fn burst_gate_collapses_same_workspace_within_window() {
        let mut g = BurstGate::new();
        let t0 = Instant::now();
        assert!(g.should_fire("/wt/a", t0));
        // Different tab/kind, same workspace, 2s later — collapsed.
        assert!(!g.should_fire("/wt/a", t0 + Duration::from_secs(2)));
        // Different workspace at the same instant — independent.
        assert!(g.should_fire("/wt/b", t0 + Duration::from_secs(2)));
        // Same workspace after the window elapses — fires again.
        assert!(g.should_fire("/wt/a", t0 + Duration::from_secs(6)));
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
