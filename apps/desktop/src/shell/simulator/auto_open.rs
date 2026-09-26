//! Auto-open (P9): the Simulator panel shows itself when an agent starts
//! simulator work, with no command needed.
//!
//! Every way an agent can reach for a simulator ends here, and one policy
//! ([`AutoOpen`]) decides whether the panel may open:
//!
//! 1. **A chat agent's shell tool call** runs a simulator command
//!    ([`note_chat_event`], from the chat's event fold; judged by
//!    `oximux_simulator::classify`). The earliest signal: the panel is up while
//!    the build is still running.
//! 2. **A device boots** that no worktree attached, while the active worktree
//!    has an agent tab — how a terminal agent (whose commands OxiMux never
//!    sees) shows up. The device is attached there, then shown.
//! 3. **An `oximux sim` verb** (and a consent question, and `sim attach`).
//!
//! The panel only opens for the worktree the user is looking at (it shows the
//! active worktree's device, so opening it anywhere else would show the wrong
//! one), never takes keyboard focus, opens at most once a minute per worktree,
//! and stays shut once the user closes it — until an agent explicitly runs
//! `oximux sim attach`. All of it is off with Settings › iOS Simulator ›
//! "Open automatically".

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{App, Context, Global};
use oximux_agents::thread::ThreadEvent;
use oximux_simulator::DeviceId;
use oximux_simulator::classify;
use oximux_simulator::registry::WorktreeKey;

use super::agent_ops::same_worktree;
use crate::platform::window_registry;
use crate::shell::right_sidebar::tab::RightTab;
use crate::workspace_root::WorkspaceRoot;

/// At most one automatic open per worktree this often.
pub(crate) const RATE_LIMIT: Duration = Duration::from_secs(60);

/// ACP tool calls waiting for their kind (see [`note_chat_event`]).
const PENDING_ACP: usize = 16;

/// What asked for the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// A chat agent's shell tool call runs a simulator command.
    ToolCall,
    /// A device booted where an agent is working (and was attached there).
    DeviceBooted,
    /// An `oximux sim` control verb, or its consent question.
    Verb,
    /// An explicit `oximux sim attach`: the one trigger that reopens a panel
    /// the user closed.
    Attach,
}

/// The window's auto-open memory: when it last opened the panel for each
/// worktree, and which worktrees the user closed it on. Pure (the clock is
/// passed in), so the rules are unit tested.
#[derive(Debug, Default)]
pub(crate) struct AutoOpen {
    last_opened: HashMap<WorktreeKey, Instant>,
    closed: HashSet<WorktreeKey>,
    /// The worktree the panel is showing, and the sidebar showing it (its
    /// entity id), while it shows.
    shown_for: Option<(WorktreeKey, u64)>,
}

impl AutoOpen {
    /// Whether `trigger` may open the panel for `worktree` now. The caller has
    /// checked the setting, that `worktree` is active, and that the panel is
    /// not already showing. Records the open when it says yes.
    pub(crate) fn should_open(&mut self, worktree: &Path, trigger: Trigger, now: Instant) -> bool {
        let key = WorktreeKey::from_path(worktree);
        if trigger == Trigger::Attach {
            self.closed.remove(&key);
        } else if self.closed.contains(&key)
            || self.last_opened.get(&key).is_some_and(|at| now.saturating_duration_since(*at) < RATE_LIMIT)
        {
            return false;
        }
        self.last_opened.insert(key, now);
        true
    }

    /// The panel was shown or hidden in the sidebar `surface`, with `active`
    /// the active worktree (`None` for a hide the user did not make, such as
    /// turning the feature off). Hiding it in the same sidebar, on the
    /// worktree it was showing — closing the sidebar, picking another tab — is
    /// the user closing it; showing it again (by hand, or by an attach) takes
    /// that back. A project switch swaps the sidebar, so it is never a close.
    pub(crate) fn visibility_changed(&mut self, visible: bool, active: Option<&Path>, surface: u64) {
        let active = active.map(WorktreeKey::from_path);
        if visible {
            if let Some(key) = &active {
                self.closed.remove(key);
            }
            self.shown_for = active.map(|key| (key, surface));
        } else if let Some((shown, on)) = self.shown_for.take()
            && on == surface
            && active.as_ref() == Some(&shown)
        {
            self.closed.insert(shown);
        }
    }

    /// The active worktree changed while the panel shows (it now shows that
    /// one's device, in the same sidebar).
    pub(crate) fn follow(&mut self, active: Option<&Path>) {
        if let Some((_, surface)) = self.shown_for.take() {
            self.shown_for = active.map(|path| (WorktreeKey::from_path(path), surface));
        }
    }
}

/// ACP tool calls whose input runs a simulator command, waiting for the
/// follow-up that says the tool executes it (their name is a free-text title).
#[derive(Default)]
struct PendingAcp(VecDeque<String>);

impl Global for PendingAcp {}

/// From the chat's event fold (`agent_chat::apply_event`): a tool call that
/// runs a simulator command opens the panel for the chat's worktree.
pub(crate) fn note_chat_event(ev: &ThreadEvent, cwd: &Path, cx: &mut App) {
    // Most events are text: nothing to read, not even the settings.
    if !matches!(ev, ThreadEvent::ToolCallStarted { .. } | ThreadEvent::ToolKind { .. }) {
        return;
    }
    let settings = super::panel::settings(cx);
    if !settings.enabled || !settings.auto_open {
        return;
    }
    let fire = match ev {
        ThreadEvent::ToolCallStarted { id, name, input } => {
            if classify::is_shell_tool(name) {
                classify::is_simulator_input(input)
            } else {
                if classify::is_simulator_input(input) {
                    let pending = &mut cx.default_global::<PendingAcp>().0;
                    if pending.len() == PENDING_ACP {
                        pending.pop_front();
                    }
                    pending.push_back(id.clone());
                }
                false
            }
        }
        ThreadEvent::ToolKind { tool_call_id, kind } if kind == "execute" => {
            let pending = &mut cx.default_global::<PendingAcp>().0;
            let known = pending.iter().position(|id| id == tool_call_id);
            known.map(|at| pending.remove(at)).is_some()
        }
        _ => false,
    };
    if fire {
        request(cwd.to_path_buf(), Trigger::ToolCall, cx);
    }
}

/// Open the panel for `worktree` in the frontmost window showing it, if the
/// policy allows. Deferred: callers are mid-update of other entities.
pub(crate) fn request(worktree: PathBuf, trigger: Trigger, cx: &mut App) {
    cx.defer(move |cx| {
        let mut windows = window_registry::all_windows(cx);
        windows.retain(|(_, root)| root.read(cx).active_worktree.as_deref().is_some_and(|a| same_worktree(a, &worktree)));
        windows.sort_by_key(|(key, _)| window_registry::front_rank(cx, key));
        if let Some((_, root)) = windows.into_iter().next() {
            root.update(cx, |root, cx| root.auto_open_simulator(&worktree, trigger, cx));
        }
    });
}

impl WorkspaceRoot {
    /// Open the Simulator tab for `worktree` if auto-open allows it now.
    /// Returns whether the panel is showing `worktree` afterwards (opened now
    /// or already open), so a consent question knows its banner is seen.
    pub(crate) fn auto_open_simulator(&mut self, worktree: &Path, trigger: Trigger, cx: &mut Context<Self>) -> bool {
        let active = self.active_worktree.as_deref().is_some_and(|a| same_worktree(a, worktree));
        if !active {
            return false;
        }
        if self.simulator_showing(cx) {
            return true;
        }
        let settings = super::panel::settings(cx);
        if !settings.enabled || !settings.auto_open {
            return false;
        }
        if !self.simulator.auto_open.should_open(worktree, trigger, Instant::now()) {
            return false;
        }
        // Never focus: the user may be typing into the chat.
        self.show_simulator_tab(cx);
        true
    }

    /// Devices booted that no worktree has attached. When the user is looking
    /// at a worktree with an agent at work in it and no device, the boot is
    /// most likely that agent's: claim one (so no other window takes it too),
    /// attach it there, and show it.
    pub(crate) fn on_simulator_booted(&mut self, udids: &[DeviceId], cx: &mut Context<Self>) {
        let settings = super::panel::settings(cx);
        if !settings.enabled || !settings.auto_open {
            return;
        }
        let (Some(worktree), Some(hub)) = (self.active_worktree.clone(), super::hub(cx)) else { return };
        if hub.read(cx).device_for(&worktree).is_some() {
            return;
        }
        let has_agent = self.active_project_panes().is_some_and(|panes| panes.read(cx).has_live_agent_for(&worktree, cx));
        if !has_agent {
            return;
        }
        let claimed = hub.update(cx, |hub, _| udids.iter().find(|udid| hub.claim_booted(udid)).cloned());
        let Some(udid) = claimed else { return };
        hub.update(cx, |hub, cx| hub.attach(&worktree, Some(udid), None, cx));
        self.auto_open_simulator(&worktree, Trigger::DeviceBooted, cx);
    }

    /// The right sidebar is open on the Simulator tab.
    pub(crate) fn simulator_showing(&self, cx: &App) -> bool {
        self.right_sidebar.as_ref().is_some_and(|rs| {
            let rs = rs.read(cx);
            rs.open && rs.active_tab == RightTab::Simulator
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "/nonexistent/oximux-auto-open/w";
    /// A sidebar's entity id.
    const SIDEBAR: u64 = 7;

    #[test]
    fn opens_at_most_once_a_minute_per_worktree() {
        let (mut policy, t) = (AutoOpen::default(), Instant::now());
        assert!(policy.should_open(Path::new(W), Trigger::ToolCall, t));
        assert!(!policy.should_open(Path::new(W), Trigger::Verb, t + RATE_LIMIT / 2));
        assert!(policy.should_open(Path::new("/nonexistent/other"), Trigger::Verb, t), "per worktree");
        assert!(policy.should_open(Path::new(W), Trigger::DeviceBooted, t + RATE_LIMIT));
    }

    #[test]
    fn a_panel_the_user_closed_stays_closed_until_an_attach() {
        let (mut policy, t) = (AutoOpen::default(), Instant::now());
        let w = Path::new(W);
        assert!(policy.should_open(w, Trigger::ToolCall, t));
        policy.visibility_changed(true, Some(w), SIDEBAR);
        policy.visibility_changed(false, Some(w), SIDEBAR);
        let later = t + RATE_LIMIT * 10;
        for trigger in [Trigger::ToolCall, Trigger::DeviceBooted, Trigger::Verb] {
            assert!(!policy.should_open(w, trigger, later), "{trigger:?} must respect the close");
        }
        assert!(policy.should_open(w, Trigger::Attach, later), "an explicit attach reopens it");
        assert!(!policy.should_open(w, Trigger::Verb, later + RATE_LIMIT / 2), "and the rate limit runs from there");
        assert!(policy.should_open(w, Trigger::Verb, later + RATE_LIMIT), "the close is forgotten");
    }

    #[test]
    fn reopening_by_hand_forgets_the_close() {
        let (mut policy, t) = (AutoOpen::default(), Instant::now());
        let w = Path::new(W);
        policy.visibility_changed(true, Some(w), SIDEBAR);
        policy.visibility_changed(false, Some(w), SIDEBAR);
        policy.visibility_changed(true, Some(w), SIDEBAR);
        assert!(policy.should_open(w, Trigger::Verb, t));
    }

    /// Hides the user did not make are not closes: a project switch (the
    /// worktree moves first, then another project's sidebar takes over), and
    /// turning the feature off.
    #[test]
    fn hiding_it_by_moving_elsewhere_is_not_a_close() {
        let (mut policy, t) = (AutoOpen::default(), Instant::now());
        let (w, other) = (Path::new(W), Path::new("/nonexistent/other"));
        policy.visibility_changed(true, Some(w), SIDEBAR);
        policy.follow(Some(other));
        policy.visibility_changed(false, Some(other), SIDEBAR + 1);
        assert!(policy.should_open(other, Trigger::Verb, t), "a project switch");
        policy.visibility_changed(true, Some(w), SIDEBAR);
        policy.visibility_changed(false, None, SIDEBAR);
        assert!(policy.should_open(w, Trigger::Verb, t), "the feature turned off");
    }

    /// Closing it after it followed the active worktree is a close of that
    /// worktree, not of the one it first showed.
    #[test]
    fn a_close_is_for_the_worktree_it_was_showing() {
        let (mut policy, t) = (AutoOpen::default(), Instant::now());
        let (w, other) = (Path::new(W), Path::new("/nonexistent/other"));
        policy.visibility_changed(true, Some(w), SIDEBAR);
        policy.follow(Some(other));
        policy.visibility_changed(false, Some(other), SIDEBAR);
        assert!(!policy.should_open(other, Trigger::Verb, t));
        assert!(policy.should_open(w, Trigger::Verb, t));
    }
}
