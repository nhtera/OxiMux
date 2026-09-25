//! `WorkspaceRoot`'s side of the simulator panel, kept here so the root's own
//! (large) files only gain a field, a render call and two action arms.
//!
//! The root owns the window's one [`SimulatorPanel`] (only where the feature
//! is supported), hands it to every sidebar, forwards active-worktree changes
//! to it, and pushes visibility — `open && active tab == Simulator` — since a
//! hidden entity never renders and so cannot notice it was hidden.

use gpui::{AppContext as _, Context, Entity, Subscription, Window, px};
use oximux_settings::{Density, Theme, Typography};

use super::panel::SimulatorPanel;
use super::widths;
use crate::shell::right_sidebar::tab::RightTab;
use crate::shell::workspace::focus_follow::ActiveWorktreeChanged;
use crate::workspace_root::WorkspaceRoot;

pub(crate) struct RootSimulator {
    panel: Option<Entity<SimulatorPanel>>,
    /// Last visibility pushed to the panel.
    visible: bool,
    /// The one-time width bump on first select has happened.
    bumped: bool,
    maximized: bool,
    _follow: Option<Subscription>,
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
        Self { panel, visible: false, bumped: false, maximized: false, _follow: None }
    }

    /// The panel, for handing to sidebars (`None` hides the tab).
    pub(crate) fn panel(&self) -> Option<Entity<SimulatorPanel>> {
        self.panel.clone()
    }

    /// Follow the active tab's worktree. Call once the root entity exists.
    pub(crate) fn follow_active_worktree(&mut self, cx: &mut Context<WorkspaceRoot>) {
        if self.panel.is_none() {
            return;
        }
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
    /// Known gap: GPUI exposes no "minimized" state and a minimized window
    /// stops rendering, so minimize does not pause the helper here (P6's
    /// screen view can pause when its frames stop being painted).
    pub(crate) fn sync_simulator_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.simulator.panel.clone() else { return };
        let Some(rs) = self.right_sidebar.clone() else { return };
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
