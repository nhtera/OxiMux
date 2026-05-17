//! RightSidebar — tab-switchable activity-bar panel replacing the fixed git column.
//!
//! Owns the StatusPoller lifetime, mirrors poll state for the status bar,
//! and dispatches to Explorer / Search / Source Control tab bodies.

pub mod activity_bar;
pub mod layout;
pub mod tab;

use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Task, Window, div, px,
};
use oximux_git::{PollState, Repository, StatusPoller};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::GitPanel;
use crate::shell::right_sidebar::activity_bar::{render_collapsed_rail, render_top_tab_bar};
use crate::shell::right_sidebar::layout::DEFAULT_PANEL_WIDTH;
use crate::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};

/// Tab-switchable right panel that replaces the old fixed `GitMount` column.
pub struct RightSidebar {
    pub open: bool,
    pub active_tab: RightTab,

    // Source Control sub-panels (unchanged from old GitMount).
    // `pub(crate)`: tests in this package may read state; external callers should not.
    pub(crate) git_panel: Entity<GitPanel>,
    pub(crate) diff_view: Entity<DiffView>,

    // Poll state mirrored for the status bar (avoids borrowing through entity tree).
    pub latest_poll_state: PollState,

    // Held to keep the poller alive; drop aborts the background task.
    // `None` only in tests injecting a watch channel directly (no live repo).
    _poller: Option<Arc<StatusPoller>>,
    _poll_observer: Task<()>,

    theme: Theme,
    density: Density,
    typography: Typography,
}

impl RightSidebar {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let poller = Arc::new(StatusPoller::spawn(repo.clone()));
        let panel_rx = poller.subscribe();
        let bar_rx = poller.subscribe();
        let initial = poller.current();

        let diff_view =
            cx.new(|cx| DiffView::new(repo.clone(), theme, density, typography.clone(), cx));
        let diff_view_for_panel = diff_view.clone();
        let git_panel = cx.new(|cx| {
            GitPanel::new(
                repo,
                panel_rx,
                Some(diff_view_for_panel),
                theme,
                density,
                typography.clone(),
                cx,
            )
        });

        let poll_observer = Self::start_poll_observer(bar_rx, cx);

        Self {
            open: true,
            active_tab: RightTab::SourceControl,
            git_panel,
            diff_view,
            latest_poll_state: initial,
            _poller: Some(poller),
            _poll_observer: poll_observer,
            theme,
            density,
            typography,
        }
    }

    /// Test constructor: injects a static watch channel so no tokio thread-pool
    /// task is spawned — keeps GPUI's test scheduler happy (single-thread only).
    /// `has_repo` controls `_poller`: `true` = `Some(Arc<noop>)`, `false` = `None`.
    /// This drives `visible_tabs` and `select_tab` validation without a real poller.
    #[doc(hidden)]
    pub fn new_for_test(
        repo: Repository,
        state_rx: tokio::sync::watch::Receiver<PollState>,
        has_repo: bool,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        // Dead bar_rx: sender dropped immediately so the observer exits on first Err.
        let (_bar_tx, bar_rx) = tokio::sync::watch::channel(PollState::Loading);
        let diff_view =
            cx.new(|cx| DiffView::new(repo.clone(), theme, density, typography.clone(), cx));
        let diff_view_for_panel = diff_view.clone();
        let git_panel = cx.new(|cx| {
            GitPanel::new(
                repo.clone(),
                state_rx,
                Some(diff_view_for_panel),
                theme,
                density,
                typography.clone(),
                cx,
            )
        });
        let poll_observer = Self::start_poll_observer(bar_rx, cx);

        // Simulate repo presence via a live poller when has_repo=true, None otherwise.
        let poller = if has_repo {
            // Spawn against the repo so the type is satisfied; the actual poll loop
            // never fires in tests because the current_thread runtime blocks it.
            Some(Arc::new(StatusPoller::spawn(repo)))
        } else {
            None
        };

        let initial_tab = if has_repo {
            RightTab::SourceControl
        } else {
            RightTab::Explorer
        };

        Self {
            open: true,
            active_tab: initial_tab,
            git_panel,
            diff_view,
            latest_poll_state: PollState::Loading,
            _poller: poller,
            _poll_observer: poll_observer,
            theme,
            density,
            typography,
        }
    }

    /// Expose the latest poll state so `WorkspaceRoot` can pass it to the status bar.
    pub fn latest_poll_state(&self) -> &PollState {
        &self.latest_poll_state
    }

    /// Switch the active tab and notify GPUI to re-render.
    ///
    /// Falls back to `Explorer` if `tab` is not in the current `visible_tabs` set
    /// (e.g. SourceControl when no repo), preventing inconsistent render state.
    pub fn select_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        let tabs = visible_tabs(TabVisibility {
            has_repo: self._poller.is_some(),
        });
        self.active_tab = if tabs.contains(&tab) {
            tab
        } else {
            RightTab::Explorer
        };
        cx.notify();
    }

    /// Toggle the sidebar open/closed state.
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.open = !self.open;
        cx.notify();
    }

    fn start_poll_observer(
        mut rx: tokio::sync::watch::Receiver<PollState>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let state = rx.borrow_and_update().clone();
                if this
                    .update(cx, |sidebar, cx| {
                        sidebar.latest_poll_state = state;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    }
}

impl Render for RightSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Derive has_repo from poller presence — None means no live repo.
        let tabs = visible_tabs(TabVisibility {
            has_repo: self._poller.is_some(),
        });
        let active = self.active_tab;
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        // NOTE: on_action handlers for sidebar keybindings are registered on
        // WorkspaceRoot's outer div (workspace_root.rs::render), not here.
        // RightSidebar is a sibling of MainPane in the row layout — not an ancestor
        // of TerminalView — so on_action here would never fire when the terminal is focused.

        // Pass the entity handle so click handlers mutate the sidebar directly.
        let entity = cx.entity().clone();

        // Closed: render a thin rail with just the expand toggle so users can
        // re-open without remembering Cmd+L (the reference UX pattern).
        if !self.open {
            return render_collapsed_rail(&entity, theme).into_any_element();
        }

        let top_bar = render_top_tab_bar(active, &tabs, &entity, theme, density, &typography);

        // Inline each tab body — avoids Box<dyn IntoElement> (trait not dyn-compatible).
        let body = match self.active_tab {
            RightTab::Explorer => div()
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.fg_muted)
                .text_size(px(12.))
                .child("File Explorer — Phase 02")
                .into_any_element(),
            RightTab::Search => div()
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.fg_muted)
                .text_size(px(12.))
                .child("Search — Phase 03")
                .into_any_element(),
            RightTab::SourceControl => div()
                // Stack vertically: file list above, diff view below. Phase 04 will
                // restructure with inline commit area between them.
                .flex_1()
                .w_full()
                .flex()
                .flex_col()
                .child(div().flex_1().w_full().child(self.git_panel.clone()))
                .child(div().flex_1().w_full().child(self.diff_view.clone()))
                .into_any_element(),
        };

        // Vertical stack: top tab bar above the panel body. Total column width fixed
        // so it doesn't compete with MainPane's flex_1 in the parent row.
        div()
            .id("right-sidebar")
            .flex()
            .flex_col()
            .h_full()
            .w(DEFAULT_PANEL_WIDTH)
            .child(top_bar)
            .child(body)
            .into_any_element()
    }
}
