//! `WorkspaceRoot`'s side of the simulator panel, kept here so the root's own
//! (large) files only gain a field, a render call and two action arms.
//!
//! The root owns the window's one [`SimulatorPanel`] (only where the feature
//! is supported), hands it to every sidebar, forwards active-worktree changes
//! to it, and pushes visibility — `open && active tab == Simulator` — since a
//! hidden entity never renders and so cannot notice it was hidden.

use gpui::{AppContext as _, Context, Entity, InteractiveElement, Subscription, Window, px};
use oximux_settings::{Density, Theme, Typography};

use std::path::Path;

use super::agent_ops::same_worktree;
use super::hub::NoticeKind;
use super::panel::{Outcome, PanelEvent, RootRequest, SimCommand, SimulatorPanel};
use super::widths;
use crate::actions::{
    SimAnnotate, SimDetach, SimHome, SimLock, SimOpenLogs, SimRotateCcw, SimRotateCw, SimScreenshot, SimShutdown,
    SimToggleKeyboard, SimToggleRecord,
};
use crate::shell::chrome::toast::{ToastAction, ToastKind};
use crate::shell::right_sidebar::tab::RightTab;
use crate::shell::workspace::focus_follow::ActiveWorktreeChanged;
use crate::workspace_root::WorkspaceRoot;

/// How long a consent toast's Review waits for its worktree switch to land.
const REVEAL_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) struct RootSimulator {
    panel: Option<Entity<SimulatorPanel>>,
    /// Last visibility pushed to the panel.
    visible: bool,
    /// The one-time width bump on first select has happened.
    bumped: bool,
    maximized: bool,
    /// Open the Simulator tab once this worktree is active and its project's
    /// sidebar is in place (a first visit builds it in the background), unless
    /// the moment passes first.
    reveal_for: Option<(std::path::PathBuf, std::time::Instant)>,
    _follow: Option<Subscription>,
    _notices: Option<Subscription>,
}

/// Apple silicon macOS, the feature enabled, and the hub installed.
fn supported(cx: &gpui::App) -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
        && super::panel::settings(cx).enabled
        && super::hub(cx).is_some()
}

impl RootSimulator {
    pub(crate) fn new(theme: Theme, density: Density, typography: Typography, cx: &mut Context<WorkspaceRoot>) -> Self {
        let panel = supported(cx).then(|| cx.new(|cx| SimulatorPanel::new(theme, density, typography, cx)));
        // The panel's notices (a saved screenshot, a failed paste) are toasts.
        let notices = panel.as_ref().map(|panel| {
            cx.subscribe(panel, |root: &mut WorkspaceRoot, _, event: &PanelEvent, cx| match event {
                PanelEvent::Notice(kind, text) => {
                    let kind = match kind {
                        NoticeKind::Success => ToastKind::Success,
                        NoticeKind::Error => ToastKind::Error,
                    };
                    root.push_toast(kind, text.clone(), cx);
                }
            })
        });
        Self { panel, visible: false, bumped: false, maximized: false, reveal_for: None, _follow: None, _notices: notices }
    }

    /// The panel, for handing to sidebars (`None` hides the tab).
    pub(crate) fn panel(&self) -> Option<Entity<SimulatorPanel>> {
        self.panel.clone()
    }

    /// Follow the active tab's worktree. Call once the root entity exists.
    pub(crate) fn follow_active_worktree(&mut self, cx: &mut Context<WorkspaceRoot>) {
        let Some(panel) = self.panel.clone() else { return };
        // Toolbar clicks run commands through the root directly: a click
        // focuses nothing, and an action dispatched with nothing focused never
        // reaches the root's handlers.
        let root = cx.weak_entity();
        panel.update(cx, |panel, _| {
            panel.set_command_sink(std::rc::Rc::new(move |request, window, cx| {
                let _ = root.update(cx, |root, cx| match request {
                    RootRequest::Run(command) => root.run_simulator_command(command, window, cx),
                    RootRequest::SendToAgent(pick) => root.send_pick_to_active_chat(&pick, cx),
                });
            }));
        });
        self._follow = Some(cx.subscribe_self(|root: &mut WorkspaceRoot, event: &ActiveWorktreeChanged, cx| {
            if let Some(panel) = root.simulator.panel.clone() {
                let worktree = event.0.clone();
                panel.update(cx, |panel, cx| panel.set_worktree(worktree, cx));
            }
        }));
    }
}

impl WorkspaceRoot {
    /// Push the panel's visibility when it changed. Called every render; the
    /// pushes are deferred, because updating another entity mid-render would
    /// drop its notify.
    ///
    /// Also owns the two width rules, since every way in (tab click, palette,
    /// action) and out (✕, another tab, a project switch) passes through here:
    /// the first time the tab shows, widen the sidebar for a phone; when it
    /// hides while maximized, restore the persisted width so other tabs never
    /// inherit the maximized one. Both are transient — a user's drag stays the
    /// one persisted width.
    ///
    /// Minimize / occlusion is the panel's own business: it observes its
    /// window's visibility (see `SimulatorPanel`).
    pub(crate) fn sync_simulator_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.simulator.panel.clone() else { return };
        let Some(rs) = self.right_sidebar.clone() else { return };
        self.apply_pending_reveal(&rs, cx);
        let visible = {
            let rs = rs.read(cx);
            rs.open && rs.active_tab == RightTab::Simulator
        };
        if visible == self.simulator.visible {
            return;
        }
        self.simulator.visible = visible;
        let (window_w, window_h) = (f32::from(window.viewport_size().width), f32::from(window.viewport_size().height));
        let bump = visible && !self.simulator.bumped;
        let restore = !visible && self.simulator.maximized;
        if bump {
            self.simulator.bumped = true;
        }
        if restore {
            self.simulator.maximized = false;
        }
        cx.defer(move |cx| {
            if bump || restore {
                rs.update(cx, |sidebar, cx| {
                    if restore {
                        sidebar.set_fill(false, cx);
                    }
                    if bump {
                        let width = widths::first_select_width(f32::from(sidebar.panel_width()), window_w, window_h);
                        sidebar.set_panel_width_transient(px(width), cx);
                    }
                });
            }
            panel.update(cx, |panel, cx| {
                if restore {
                    panel.set_maximized(false, cx);
                }
                panel.set_visible(visible, cx);
            });
        });
    }

    /// Open the sidebar on the Simulator tab (the width bump happens when it
    /// becomes visible, see [`Self::sync_simulator_visibility`]).
    pub(crate) fn select_simulator_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let (Some(rs), Some(_)) = (self.right_sidebar.clone(), self.simulator.panel.as_ref()) else { return };
        rs.update(cx, |sidebar, cx| {
            sidebar.open = true;
            sidebar.select_tab(RightTab::Simulator, cx);
        });
        cx.notify();
    }

    /// "Fill" takes the whole content area (the centre panes step aside,
    /// unmeasured, so terminals keep their size); "Split" gives it back.
    pub(crate) fn toggle_simulator_maximized(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let (Some(rs), Some(panel)) = (self.right_sidebar.clone(), self.simulator.panel.clone()) else { return };
        let fill = !self.simulator.maximized;
        self.simulator.maximized = fill;
        rs.update(cx, |sidebar, cx| sidebar.set_fill(fill, cx));
        panel.update(cx, |panel, cx| panel.set_maximized(fill, cx));
        cx.notify();
    }
}

impl WorkspaceRoot {
    /// An agent in `worktree` asked to control `device`. Ask where that
    /// worktree is: the panel's banner when it is this window's active
    /// worktree (opening the tab, if agents may open it), else a toast whose
    /// Review switches there. Never over another worktree's panel.
    pub(crate) fn ask_simulator_consent(
        &mut self,
        worktree: &Path,
        label: &str,
        device: &str,
        (project_id, workspace_id): (String, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.active_worktree.as_deref().is_some_and(|a| same_worktree(a, worktree));
        if active && super::panel::settings(cx).auto_open {
            self.select_simulator_tab(window, cx);
            return;
        }
        let (root, handle) = (cx.weak_entity(), window.window_handle());
        let path = worktree.to_string_lossy().into_owned();
        let review = ToastAction::new("Review", move |cx| {
            let (root, path, project_id, workspace_id) = (root.clone(), path.clone(), project_id.clone(), workspace_id.clone());
            // Deferred: this runs inside the toast layer's update, and a
            // workspace switch may toast.
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        let here = root.active_worktree.as_deref().is_some_and(|a| same_worktree(a, Path::new(&path)));
                        if here {
                            root.select_simulator_tab(window, cx);
                        } else {
                            // The tab opens once the switch lands (see
                            // `apply_pending_reveal`).
                            root.simulator.reveal_for = Some((path.clone().into(), std::time::Instant::now()));
                            root.activate_workspace_from_jump(workspace_id, project_id, path, window, cx);
                            cx.notify();
                        }
                    });
                });
            });
        });
        let text = format!("An agent in {label} wants to control {device}.");
        self.toast_layer.update(cx, |layer, cx| layer.push_with_actions(ToastKind::Info, text, vec![review], cx));
    }

    /// Open the Simulator tab for a worktree switched to from a consent
    /// toast, once the switch has landed: the worktree is active and the
    /// sidebar on screen is its project's. Deferred, as this runs in render.
    fn apply_pending_reveal(&mut self, rs: &Entity<crate::shell::right_sidebar::RightSidebar>, cx: &mut Context<Self>) {
        let Some((want, since)) = self.simulator.reveal_for.clone() else { return };
        if since.elapsed() > REVEAL_WITHIN {
            self.simulator.reveal_for = None;
            return;
        }
        let here = self.active_worktree.as_deref().is_some_and(|a| same_worktree(a, &want));
        let ready = self
            .active_project
            .as_ref()
            .and_then(|p| self.right_sidebar_by_project.get(&p.id))
            .is_some_and(|built| built.entity_id() == rs.entity_id());
        if here && ready {
            self.simulator.reveal_for = None;
            let rs = rs.clone();
            cx.defer(move |cx| {
                rs.update(cx, |sidebar, cx| {
                    sidebar.open = true;
                    sidebar.select_tab(RightTab::Simulator, cx);
                });
            });
        }
    }

    /// An agent attached a device for `worktree`: show it when the user is
    /// looking at that worktree and lets agents open the panel.
    pub(crate) fn reveal_simulator_for(&mut self, worktree: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active_worktree.as_deref().is_some_and(|a| same_worktree(a, worktree));
        if active && super::panel::settings(cx).auto_open {
            self.select_simulator_tab(window, cx);
        }
    }
}

/// The `Sim*` actions, handled at the root so a captured-keyboard shortcut,
/// a toolbar click and a palette entry all reach the panel the same way.
pub(crate) fn simulator_actions<E: InteractiveElement>(el: E, cx: &mut Context<WorkspaceRoot>) -> E {
    fn on<A: gpui::Action, E: InteractiveElement>(el: E, command: SimCommand, cx: &mut Context<WorkspaceRoot>) -> E {
        el.on_action(cx.listener(move |root, _: &A, window, cx| root.run_simulator_command(command, window, cx)))
    }
    let el = on::<SimHome, _>(el, SimCommand::Home, cx);
    let el = on::<SimLock, _>(el, SimCommand::Lock, cx);
    let el = on::<SimRotateCw, _>(el, SimCommand::RotateCw, cx);
    let el = on::<SimRotateCcw, _>(el, SimCommand::RotateCcw, cx);
    let el = on::<SimScreenshot, _>(el, SimCommand::Screenshot, cx);
    let el = on::<SimToggleRecord, _>(el, SimCommand::ToggleRecord, cx);
    let el = on::<SimAnnotate, _>(el, SimCommand::Annotate, cx);
    let el = on::<SimToggleKeyboard, _>(el, SimCommand::ToggleKeyboard, cx);
    let el = on::<SimOpenLogs, _>(el, SimCommand::OpenLogs, cx);
    let el = on::<SimShutdown, _>(el, SimCommand::Shutdown, cx);
    on::<SimDetach, _>(el, SimCommand::Detach, cx)
}

impl WorkspaceRoot {
    fn run_simulator_command(&mut self, command: SimCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.simulator.panel.clone() else { return };
        match panel.update(cx, |panel, cx| panel.run_command(command, window, cx)) {
            Outcome::Done => {}
            Outcome::NoDevice => {
                self.select_simulator_tab(window, cx);
                self.push_toast(ToastKind::Info, "Attach a simulator first.", cx);
            }
            Outcome::NotStreaming => {
                // Showing the panel brings the stream up.
                self.select_simulator_tab(window, cx);
                self.push_toast(ToastKind::Info, "The simulator is starting. Try again in a moment.", cx);
            }
            Outcome::OpenLogs { cwd, title, script } => {
                if let Some(panes) = self.active_project_panes() {
                    panes.update(cx, |panes, cx| {
                        panes.open_script_terminal_tab_in_active_group(cwd, title.into(), &script, window, cx)
                    });
                }
            }
        }
    }
}
