//! Agent notification dispatch — platform-agnostic surface.
//!
//! The badge alone covers the in-app case; this module covers the
//! out-of-app case: a desktop notification fires when a session starts
//! needing a person — by default only while the OxiMux window is not the
//! active one. Clicking the notification brings the window forward and
//! switches to the session it came from.
//!
//! **One owner.** Every agent-state banner comes from
//! [`attention_bridge::AttentionBridge`], which subscribes to the host's
//! attention stream. There used to be two independent paths — a per-tab
//! status watcher for cockpit agents and a view event for chats — and with a
//! third answer arriving from the host they would have fired next to each
//! other. The bridge owns the dedupe, so what dedupes and what decides are
//! the same thing. The bell, the terminal's own notifications and the
//! settings test button are separate sources and still call `notify`
//! directly.
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
//!   - `BurstGate` — per-workspace burst collapse, for the bell source only.
//!     Agent-state traffic is deduped by edge identity in the bridge, which
//!     is exact where a time window is a guess.
//!
//! Platform impls live in sibling modules: `mac` (active on
//! `target_os = "macos"`) and `null` (no-op fallback).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oximux_agents::attention::AttentionKind;
use oximux_core::AgentSessionId;

pub mod attention_bridge;
pub mod null;

#[cfg(target_os = "macos")]
pub mod mac;

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
#[derive(Debug, Clone, PartialEq)]
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
    /// The identifier to post under, when the caller owns one.
    ///
    /// A stable string, so posting again *replaces* the delivered banner in
    /// place instead of stacking a second one: `session:<handle>` for a
    /// session's own alert and `root:<handle>` for a root's coalesced child
    /// count. It is also what [`Notifier::retire`] takes, so whoever posted a
    /// banner can take it back when the thing it was about is over.
    ///
    /// `None` keeps the per-source identifier the bell and test paths mint,
    /// which carries a sequence number and so never replaces.
    pub replace_key: Option<String>,
    /// Post without a sound. For an update to a banner the person has already
    /// heard: a root's child count going from 2 to 3 is the same alert saying
    /// more, and a count going down is not news at all.
    pub quiet: bool,
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
    /// `session:{handle}` — the session one of the bridge's banners was
    /// about. The handle is minted by the bridge, which holds the mapping
    /// back to the namespaced target; a `u64` keeps this type `Copy`, and it
    /// is re-sent to every window on a click.
    NeedsYou(u64),
    /// `root:{handle}` — a root whose children need someone. Clicking it
    /// goes to the oldest child still waiting, the same place the
    /// jump-to-next hotkey would.
    NeedsYouRoot(u64),
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
    /// Nothing from the agent for long enough that it looks stuck.
    Stalled,
    /// Held by a rate limit. Informational: it retries itself, so this is
    /// news about *why* nothing is happening, not a request to act.
    Limited,
    /// Terminal bell (BEL) from a pane the user can't see. Only dispatched
    /// when the bell setting is `Notify`, so it has no per-kind toggle of
    /// its own — the source gate + that setting are the opt-ins.
    Bell,
    /// A notification a terminal asked for: a program's OSC 9 / 777 (opt-in,
    /// `terminal.osc_notifications`) or the user's `oximux notify`. No per-kind
    /// toggle, like `Bell`.
    TerminalNotify,
}

impl NotificationKind {
    /// The banner kind for one attention kind.
    ///
    /// The only mapping from attention to a banner, and the replacement for
    /// the status-based one this module used to own: a status cannot say
    /// whether anyone has dealt with the thing it describes, so deciding from
    /// it meant re-announcing a failure the person had already dismissed.
    pub fn from_attention(kind: AttentionKind) -> Self {
        match kind {
            AttentionKind::NeedsPermission => Self::NeedsApproval,
            AttentionKind::NeedsInput => Self::WaitingForInput,
            AttentionKind::Failed => Self::Failed,
            AttentionKind::Stalled => Self::Stalled,
            AttentionKind::Limited => Self::Limited,
            AttentionKind::FinishedUnread => Self::Done,
        }
    }

    /// Banner title for this edge, given the agent's tab label.
    pub fn title(self, agent_label: &str) -> String {
        match self {
            Self::NeedsApproval => format!("Agent needs approval — {agent_label}"),
            Self::WaitingForInput => format!("Agent waiting for input — {agent_label}"),
            Self::Done => format!("Agent finished — {agent_label}"),
            Self::Failed => format!("Agent failed — {agent_label}"),
            Self::Stalled => format!("Agent may be stuck — {agent_label}"),
            Self::Limited => format!("Agent is rate-limited — {agent_label}"),
            Self::Bell => format!("Terminal bell — {agent_label}"),
            // Fixed: a program may not choose the banner title, or it could
            // pose as an agent or the system. Its own title goes in the body.
            Self::TerminalNotify => format!("Terminal — {agent_label}"),
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
    /// Off by default: the stall threshold is a measurement this phase has
    /// not taken yet, and a banner for "nothing happened" is the easiest
    /// one to cry wolf with.
    pub stalled: AtomicBool,
    /// Off by default: a rate limit retries itself, so it is news rather
    /// than a request to act.
    pub limited: AtomicBool,
    pub sound: AtomicBool,
    /// When true (default), only notify while the window is unfocused.
    pub only_when_unfocused: AtomicBool,
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
    pub stalled: bool,
    pub limited: bool,
    pub sound: bool,
    pub only_when_unfocused: bool,
}

impl Default for NotifyPrefValues {
    /// Quiet defaults (honoring the design ethos): approval / done / failed
    /// banners on; waiting-input + sound off; only while unfocused. Master
    /// switch + sources on.
    fn default() -> Self {
        Self {
            enabled: true,
            source_agent_state: true,
            source_terminal_bell: true,
            needs_approval: true,
            waiting_input: false,
            done: true,
            failed: true,
            stalled: false,
            limited: false,
            sound: false,
            only_when_unfocused: true,
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
            stalled: AtomicBool::new(v.stalled),
            limited: AtomicBool::new(v.limited),
            sound: AtomicBool::new(v.sound),
            only_when_unfocused: AtomicBool::new(v.only_when_unfocused),
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
            stalled: read(keys::STALLED, d.stalled),
            limited: read(keys::LIMITED, d.limited),
            sound: read(keys::SOUND, d.sound),
            only_when_unfocused: read(keys::ONLY_WHEN_UNFOCUSED, d.only_when_unfocused),
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
            NotificationKind::Stalled => &self.stalled,
            NotificationKind::Limited => &self.limited,
            // Bell carries no per-kind toggle: the per-pane `Notify` bell
            // setting and the source gate above are the opt-ins.
            NotificationKind::Bell | NotificationKind::TerminalNotify => return true,
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

/// Longest program-supplied title / body shown in a terminal notification.
pub const TERMINAL_TITLE_MAX_CHARS: usize = 128;
pub const TERMINAL_BODY_MAX_CHARS: usize = 512;

/// The banner body for a terminal notification: the program's own title (if
/// any) quoted on the first line, then its text. Both are sanitized — control,
/// bidi and invisible characters stripped — and capped, since they are
/// whatever a program printed. `None` when nothing is left to show.
pub fn terminal_notification_body(title: Option<&str>, body: &str) -> Option<String> {
    let clean = |s: &str, max: usize| {
        let s = oximux_pty::program_status::sanitize_display_text(s);
        let s = s.trim();
        if s.chars().count() > max {
            let mut cut: String = s.chars().take(max - 1).collect();
            cut.push('…');
            cut
        } else {
            s.to_string()
        }
    };
    let title = title.map(|t| clean(t, TERMINAL_TITLE_MAX_CHARS)).unwrap_or_default();
    let body = clean(body, TERMINAL_BODY_MAX_CHARS);
    match (title.is_empty(), body.is_empty()) {
        (true, true) => None,
        (true, false) => Some(body),
        (false, true) => Some(format!("“{title}”")),
        (false, false) => Some(format!("“{title}”\n{body}")),
    }
}

/// Burst-collapse key for a terminal-sourced banner. Its own namespace, so a
/// program printing notifications in a loop collapses only its own banners
/// and can never use up the slot an agent's approval banner needs. The user's
/// own `oximux notify` (`explicit`) gets a slot of its own too, so a bell or a
/// program's OSC 9 just before it cannot swallow it.
pub fn terminal_burst_key(workspace: &str, explicit: bool) -> String {
    if explicit { format!("term-explicit:{workspace}") } else { format!("term:{workspace}") }
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
    pub const STALLED: &str = "notify.stalled";
    pub const LIMITED: &str = "notify.limited";
    pub const SOUND: &str = "notify.sound";
    pub const ONLY_WHEN_UNFOCUSED: &str = "notify.only_when_unfocused";
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

    /// Reset the "needs attention" dock badge. Called when the window regains
    /// focus — the user is back, so the accumulated count clears. Default no-op
    /// (the null notifier / mocks have no dock tile).
    fn clear_attention(&self) {}

    /// Take back the banner posted under `replace_key`, if it is still
    /// delivered.
    ///
    /// What makes a banner honest: a permission answered on a phone, a
    /// failure dismissed in the rail, a root whose last child was dealt with
    /// — the thing the banner was about is over, and a notification the
    /// person finds later should not still be asking. Default no-op, so the
    /// null notifier and mocks stay one-liners.
    fn retire(&self, replace_key: &str) {
        let _ = replace_key;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_notification_keeps_a_fixed_title_and_quotes_the_programs() {
        assert_eq!(NotificationKind::TerminalNotify.title("build"), "Terminal — build");
        assert_eq!(
            terminal_notification_body(Some("Agent needs approval"), "click me").as_deref(),
            Some("“Agent needs approval”\nclick me")
        );
        assert_eq!(terminal_notification_body(None, "done").as_deref(), Some("done"));
        assert_eq!(terminal_notification_body(Some("T"), "").as_deref(), Some("“T”"));
    }

    #[test]
    fn terminal_notification_text_is_sanitized_capped_and_may_vanish() {
        assert_eq!(
            terminal_notification_body(None, "ok\u{1b}[31m\u{202E}gnp\u{200B}\n").as_deref(),
            Some("ok[31mgnp")
        );
        assert_eq!(terminal_notification_body(Some("\u{200B}"), " \u{7} "), None);
        let long = terminal_notification_body(None, &"x".repeat(2000)).unwrap();
        assert_eq!(long.chars().count(), TERMINAL_BODY_MAX_CHARS);
        assert!(long.ends_with('…'));
        let title = terminal_notification_body(Some(&"t".repeat(500)), "b").unwrap();
        assert_eq!(title.lines().next().unwrap().chars().count(), TERMINAL_TITLE_MAX_CHARS + 2);
    }

    #[test]
    fn a_terminal_burst_never_uses_up_an_agents_slot() {
        let mut gate = BurstGate::new();
        let now = Instant::now();
        let key = terminal_burst_key("/wt", false);
        assert!(gate.should_fire(&key, now));
        // A program looping notifications in the same worktree…
        assert!(!gate.should_fire(&key, now + Duration::from_millis(10)));
        // …leaves the agent's own burst slot for that worktree untouched,
        assert!(gate.should_fire("/wt", now + Duration::from_millis(20)));
        // and the user's own `oximux notify` too.
        assert!(gate.should_fire(&terminal_burst_key("/wt", true), now + Duration::from_millis(30)));
    }

    #[test]
    fn every_attention_kind_has_a_banner_kind() {
        use oximux_agents::attention::AttentionKind;
        // The mapping is total on purpose: a kind with no banner would be an
        // alert the rail shows and the notifier silently drops.
        let pairs = [
            (AttentionKind::NeedsPermission, NotificationKind::NeedsApproval),
            (AttentionKind::NeedsInput, NotificationKind::WaitingForInput),
            (AttentionKind::Failed, NotificationKind::Failed),
            (AttentionKind::Stalled, NotificationKind::Stalled),
            (AttentionKind::Limited, NotificationKind::Limited),
            (AttentionKind::FinishedUnread, NotificationKind::Done),
        ];
        for (attention, banner) in pairs {
            assert_eq!(NotificationKind::from_attention(attention), banner);
        }
    }


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
            replace_key: None,
            quiet: false,
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
}
