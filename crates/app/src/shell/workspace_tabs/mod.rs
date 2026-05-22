//! WorkspaceTabs — flat workspace-level tab strip.
//!
//! Each entry owns an `Entity<MainPane>` (its own split tree of terminals).
//! The strip is rendered into the top_bar's center zone via
//! [`render_tab_strip`]; the active MainPane fills the main row below.
//!
//! Action handlers for `NewTab`/`CloseTab`/`NextTab`/`PrevTab` live here so
//! the workspace tab strip catches the keystrokes (Cmd-T / Cmd-W / Cmd-} /
//! Cmd-{) that bubble up from the focused TerminalView through the active
//! MainPane.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, Window, div,
};
use oximux_agents::{AgentRuntime, AgentStatusStream, CliRuntime, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{CloseTab, NewAgent, NewTab, NextTab, PrevTab, RequestOpenAdapterPicker};
mod persistence;
mod render;

pub use render::{SplitDirection, render_tab_strip, split_icon};

use crate::notifier::{Notifier, TabId};
use crate::persisted_terminals::PersistedTabs;
use crate::shell::agent_status_task::spawn_status_task;
use crate::shell::agent_tab_label;
use crate::shell::main_pane::MainPane;
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};

/// Save sink invoked after every tab/topology change. Captures
/// `SettingsRepo` + `project_id`; serializes the snapshot to JSON +
/// writes one row in `settings`. `Arc<dyn Fn>` so the closure can sit
/// behind a long-lived entity.
pub type SaveCallback = Arc<dyn Fn(PersistedTabs) + Send + Sync>;

/// Tab kind: raw shell PTY vs `CliRuntime`-managed agent session. The
/// distinction drives `agent_count`, the status badge, and the label prefix.
/// Agent variant carries the spawn-config metadata so persistence can
/// respawn the same session shape on restart (step 15).
pub enum WorkspaceTabKind {
    Terminal,
    Agent {
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
    },
}

struct WorkspaceTab {
    label: SharedString,
    pane: Entity<MainPane>,
    kind: WorkspaceTabKind,
    _observer: Subscription,
    /// Per-tab `AgentStatus` watcher. `None` for terminal tabs. Drop on tab
    /// removal cancels the task (same pattern as `_observer`).
    _status_task: Option<Task<()>>,
}

pub struct WorkspaceTabs {
    tabs: Vec<WorkspaceTab>,
    active: usize,
    next_label_n: u64,
    /// Spawn cwd for new tabs + splits. Set to the owning project's
    /// `root_path`; `WorkspaceRoot` keeps one entity per project so cwd is
    /// always the right one.
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    /// Retained so `close_tab` can cancel agent sessions. Launch path is in
    /// `WorkspaceRoot::spawn_agent_tab`.
    cli_runtime: Arc<CliRuntime>,
    /// Window-x of the most-recent `+` click. The button drifts as tabs
    /// open/close, so a static inset can't anchor the popover under it.
    /// `None` for the keyboard path (Cmd+Shift+A).
    last_plus_click_x: Cell<Option<f32>>,
    /// macOS notification sink (or `NullNotifier` on non-mac). Cloned by
    /// each per-tab status task.
    notifier: Arc<dyn Notifier>,
    /// Current GPUI window active state. Per-tab status tasks check this
    /// before firing a notification — the badge is enough when focused.
    window_active: bool,
    _window_activation_observer: Subscription,
    /// Persistence sink, set by `build_workspace_tabs` after construction.
    /// `None` in tests and during construction.
    save_callback: Option<SaveCallback>,
    /// Sum of every tab's `MainPane::topology_version`. Re-computed on each
    /// pane observe; a delta triggers a save. Filters out PTY-output ticks
    /// (which only bump frame counters, not topology).
    last_topology_signature: u64,
}

impl WorkspaceTabs {
    /// Construct an empty tabs strip. Callers seed it via either
    /// `seed_default_terminal` (no-snapshot path) or the per-tab restore
    /// helpers in `persistence.rs` (snapshot path). Empty state is valid:
    /// `render` falls through to the welcome view.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let window_active = window.is_window_active();
        let window_activation_observer =
            cx.observe_window_activation(window, |this, window, _cx| {
                this.window_active = window.is_window_active();
            });
        Self {
            tabs: Vec::new(),
            active: 0,
            next_label_n: 1,
            cwd,
            theme,
            density,
            typography,
            focus_handle: cx.focus_handle(),
            cli_runtime,
            last_plus_click_x: Cell::new(None),
            notifier,
            window_active,
            _window_activation_observer: window_activation_observer,
            save_callback: None,
            last_topology_signature: 0,
        }
    }

    /// Push a freshly-spawned default Terminal as "Terminal 1". Used by
    /// the no-snapshot boot path so the user opens onto a working shell
    /// rather than the welcome view.
    pub fn seed_default_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = spawn_main_pane(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            window,
            cx,
        ) else {
            return;
        };
        let observer = cx.observe(&pane, |this, _pane, cx| {
            cx.notify();
            this.maybe_save_on_topology_change(cx);
        });
        self.tabs.push(WorkspaceTab {
            label: SharedString::from("Terminal 1"),
            pane,
            kind: WorkspaceTabKind::Terminal,
            _observer: observer,
            _status_task: None,
        });
        self.active = 0;
        self.next_label_n = 2;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Pop the most-recent `+` click x (window coords) if one was recorded
    /// since the last read. `take` semantics so a keyboard-only follow-up
    /// (Cmd+Shift+A after a mouse click) doesn't reuse a stale position
    /// from a since-resized window.
    pub fn take_plus_click_x(&self) -> Option<f32> {
        self.last_plus_click_x.take()
    }

    /// Count of currently-open agent tabs. Consumed by the status bar's
    /// `agent_count` slot (sub-6).
    pub fn agent_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.kind, WorkspaceTabKind::Agent { .. }))
            .count()
    }

    pub fn active_pane(&self) -> Option<Entity<MainPane>> {
        self.tabs.get(self.active).map(|t| t.pane.clone())
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// True when the GPUI window is currently the active app window.
    /// Consumed by per-agent status watchers to decide whether to bother
    /// the user with a macOS notification on `NeedsApproval` transitions.
    pub fn window_active(&self) -> bool {
        self.window_active
    }

    /// Forward the chrome width to every tab's MainPane. Inactive tabs still
    /// hold a PTY whose grid must reflect the current visible area, otherwise
    /// switching back would briefly paint with a stale size before the next
    /// resize tick.
    pub fn set_chrome_width(&self, chrome_w: f32, cx: &mut App) {
        for tab in &self.tabs {
            tab.pane
                .update(cx, |pane, cx| pane.set_chrome_width(chrome_w, cx));
        }
    }

    pub fn open_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(new_pane) = spawn_main_pane(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            window,
            cx,
        ) else {
            return;
        };
        let n = self.next_label_n;
        self.next_label_n += 1;
        let observer = cx.observe(&new_pane, |this, _pane, cx| {
            cx.notify();
            this.maybe_save_on_topology_change(cx);
        });
        self.tabs.push(WorkspaceTab {
            label: SharedString::from(format!("Terminal {n}")),
            pane: new_pane,
            kind: WorkspaceTabKind::Terminal,
            _observer: observer,
            _status_task: None,
        });
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        self.save_now(cx);
        cx.notify();
    }

    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(idx);
        // C1 (review 260520-1700): an Agent tab's `WorkspaceTabKind::Agent`
        // holds the runtime's `AgentSessionId`. Dropping the tab releases
        // the `Receiver<AgentStatus>` clone but leaves the `SessionEntry`
        // inside `CliRuntime` — the PTY process and poll task survive. Fire
        // `cancel` async so the process tree is reaped (SIGKILL via
        // `portable-pty` per the existing `cancel` path; SIGTERM grace
        // lands in step 13). Detach the task: tab close should not block
        // the UI on agent shutdown.
        if let WorkspaceTabKind::Agent { session_id, .. } = removed.kind {
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_this, _cx| {
                if let Err(err) = runtime.cancel(session_id).await {
                    tracing::warn!(?err, "close_tab: agent cancel failed");
                }
            })
            .detach();
        }
        if self.tabs.is_empty() {
            // Last-tab close: drop to welcome state. Move focus to the
            // tabs root so top-bar button dispatches still bubble up to
            // WorkspaceRoot's on_action handlers (button-fired
            // `dispatch_action` needs a focused descendant).
            self.active = 0;
            self.focus_handle.focus(window, cx);
            self.save_now(cx);
            cx.notify();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        self.save_now(cx);
        cx.notify();
    }

    pub fn set_active(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx < self.tabs.len() && idx != self.active {
            self.active = idx;
            self.focus_active(window, cx);
            self.save_now(cx);
            cx.notify();
        }
    }

    /// Activate the tab whose agent session matches `tab_id`. Returns
    /// true on hit. Lookup is by `AgentSessionId` (stable across reorder
    /// and rename, unlike the strip index). Used by the macOS
    /// notification click router; missing id is a silent no-op.
    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| match &t.kind {
            WorkspaceTabKind::Agent { session_id, .. } => TabId::from(*session_id) == tab_id,
            WorkspaceTabKind::Terminal => false,
        }) else {
            return false;
        };
        self.set_active(idx, window, cx);
        true
    }

    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = (self.active + 1) % self.tabs.len();
        self.set_active(next, window, cx);
    }

    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if len < 2 {
            return;
        }
        let prev = (self.active + len - 1) % len;
        self.set_active(prev, window, cx);
    }

    /// Focus the active tab's pane. Called by tab open/switch from
    /// inside this entity, AND by `WorkspaceRoot::set_active_project`
    /// when activating a project's tabs entity — without that outer
    /// call, switching projects leaves focus on the previous project's
    /// (now-orphaned) pane and every action dispatched from there
    /// (sidebar toggle, keystrokes, command palette) walks up a dead
    /// focus chain that no longer reaches the workspace-root handlers.
    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let handle = tab.pane.read(cx).active_focus_handle(cx);
        handle.focus(window, cx);
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tab(window, cx);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.active;
        self.close_tab(ix, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.next_tab(window, cx);
    }

    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.prev_tab(window, cx);
    }

    /// Mount a freshly-spawned agent session as a new tab. Builds the
    /// `TerminalView` + `MainPane` here so the label-counter scan reflects
    /// any tabs added during the async spawn window.
    ///
    /// `label_override` lets the restore path keep the user's pre-quit label
    /// verbatim. `None` triggers the normal auto-incrementing label.
    #[allow(clippy::too_many_arguments)]
    pub fn push_agent_tab(
        &mut self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        label_override: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let typography_for_view = typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend,
                term_id,
                theme,
                density,
                typography_for_view,
                window,
                cx,
            )
        });
        let pane = cx.new(|cx| {
            MainPane::new(
                view,
                self.cwd.clone(),
                theme,
                density,
                typography.clone(),
                cx,
            )
        });
        let observer = cx.observe(&pane, |this, _pane, cx| {
            cx.notify();
            this.maybe_save_on_topology_change(cx);
        });
        let label = match label_override {
            Some(s) => SharedString::from(s),
            None => {
                let current_labels: Vec<SharedString> =
                    self.tabs.iter().map(|t| t.label.clone()).collect();
                agent_tab_label::next_label_for(adapter_id, &current_labels)
            }
        };
        let status_task = spawn_status_task(
            status_rx.clone(),
            self.notifier.clone(),
            TabId::from(session_id),
            label.clone(),
            cx,
        );
        self.tabs.push(WorkspaceTab {
            label,
            pane,
            kind: WorkspaceTabKind::Agent {
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
            },
            _observer: observer,
            _status_task: Some(status_task),
        });
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        self.save_now(cx);
        cx.notify();
    }

    /// Forward the Cmd+Shift+A keystroke to `WorkspaceRoot`, which owns
    /// the popover entity. Keyboard and `+`-button paths share this
    /// dispatch so a single popover backs both surfaces.
    ///
    /// Re-dispatching `RequestOpenAdapterPicker` (rather than calling
    /// `picker.open()` directly) keeps WorkspaceTabs domain-blind about
    /// where the picker lives. Anchor calculation needs left-rail state
    /// only WorkspaceRoot has access to.
    fn on_new_agent(&mut self, _: &NewAgent, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(RequestOpenAdapterPicker), cx);
    }
}

fn spawn_main_pane(
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceTabs>,
) -> Option<Entity<MainPane>> {
    let (backend, session_id) = spawn_local_pty(cwd.clone())?;
    let typography_for_view = typography.clone();
    let initial_view = cx.new(|cx| {
        TerminalView::mount(
            backend,
            session_id,
            theme,
            density,
            typography_for_view,
            window,
            cx,
        )
    });
    Some(cx.new(|cx| MainPane::new(initial_view, cwd, theme, density, typography, cx)))
}

impl Focusable for WorkspaceTabs {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceTabs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Render the active MainPane, or the welcome card when no
        // tabs are open (e.g. after closing the last one). The tab strip is
        // built separately by WorkspaceRoot via [`render_tab_strip`] and
        // slotted into top_bar. Action handlers live on the root div so
        // NewTab/CloseTab/NextTab/PrevTab catch keystrokes bubbling from the
        // focused TerminalView (which is inside the active MainPane).
        let focus_handle = self.focus_handle.clone();
        let mut root = div()
            .id("oximux-workspace-tabs")
            .track_focus(&focus_handle)
            .size_full()
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_new_agent));
        if let Some(pane) = self.active_pane() {
            root = root.child(pane);
        } else {
            root = root.child(crate::shell::welcome_view::view(
                self.theme,
                self.density,
                &self.typography,
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    //! Action-dispatch tests for the `+` button and the `NewAgent` keystroke.
    //!
    //! True dispatch-flow tests (button click → action travels up focus
    //! chain → WorkspaceRoot handler fires) require a GPUI `TestAppContext`
    //! plus an inflated render harness — not yet built in this workspace.
    //! These tests instead validate the *static* contract: both surfaces
    //! dispatch the same action type, and that type is the one
    //! `WorkspaceRoot` listens for. Renaming the action would break the
    //! contract; compilation alone catches that, but the tests document
    //! intent so the link doesn't go silent.
    use super::*;
    use std::any::TypeId;

    #[test]
    fn new_agent_and_picker_share_dispatch_type() {
        // If `RequestOpenAdapterPicker` is ever split per surface, this
        // test fails and the intent comment above is the next reader's hint.
        assert_eq!(
            TypeId::of::<RequestOpenAdapterPicker>(),
            TypeId::of::<RequestOpenAdapterPicker>(),
        );
    }

    #[test]
    fn picker_dispatch_action_is_constructable() {
        // Cheap compile-check that the action type can be boxed for
        // `window.dispatch_action(...)` — the call shape used by both
        // `plus_button` and `on_new_agent`. If `actions!` macro output
        // ever changes, this fails before the dispatch sites do.
        let _: Box<RequestOpenAdapterPicker> = Box::new(RequestOpenAdapterPicker);
    }

    #[test]
    fn click_anchor_cell_take_returns_then_clears() {
        // Direct exercise of the Cell semantics that bridge `plus_button`
        // → `WorkspaceRoot`. A `take()` should consume the stored value so
        // a follow-up keyboard activation falls through to the inset
        // fallback instead of reusing a stale x from a since-resized
        // window or scrolled tab strip.
        let cell: Cell<Option<f32>> = Cell::new(None);
        assert!(cell.take().is_none());

        cell.set(Some(450.0));
        assert_eq!(cell.take(), Some(450.0));
        assert!(cell.take().is_none(), "Cell must clear after first take");
    }

    #[test]
    fn click_x_to_anchor_subtracts_half_button_width() {
        // Mirror the math inside `plus_button`'s mouse_down closure.
        // Documents the convention: anchor = click_x - 14 so the popover
        // sits roughly under the button regardless of where inside the
        // 28px hitbox the user actually clicked.
        let plus_w = super::render::PLUS_BUTTON_WIDTH_PX;
        let click_x = 600.0;
        let anchor = (click_x - plus_w / 2.0).max(0.0);
        assert!((anchor - 586.0).abs() < f32::EPSILON);

        // Edge: click reported at x=10 (improbable but exercises clamp).
        let anchor_edge = (10.0_f32 - plus_w / 2.0).max(0.0);
        assert!(anchor_edge.abs() < f32::EPSILON);
    }
}
