//! RightSidebar — tab-switchable activity-bar panel replacing the fixed git column.
//!
//! Owns the StatusPoller lifetime, mirrors poll state for the status bar,
//! and dispatches to Explorer / Search / Source Control tab bodies.

pub mod activity_bar;
pub mod layout;
pub mod tab;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Task, Window, div, px,
};
use oximux_git::{PollState, Repository, StatusPoller};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::diff_view::DiffView;
use crate::shell::file_explorer::FileExplorer;
use crate::shell::git_panel::GitPanel;
use crate::shell::right_sidebar::layout::DEFAULT_PANEL_WIDTH;
use crate::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};
use crate::shell::search_panel::SearchPanel;
use crate::shell::source_control::{PanelConfig, SourceControlPanel};

/// Configuration bundle for `RightSidebar::new_for_test`. Keeps the test
/// constructor under the 7-argument clippy limit.
#[doc(hidden)]
pub struct SidebarTestConfig {
    pub state_rx: tokio::sync::watch::Receiver<PollState>,
    pub has_repo: bool,
    pub theme: Theme,
    pub density: Density,
    pub typography: Typography,
}

/// Tab-switchable right panel that replaces the old fixed `GitMount` column.
pub struct RightSidebar {
    pub open: bool,
    pub active_tab: RightTab,

    // Source Control panel; `None` when the active project isn't a git repo.
    // Composes the file list, diff view, inline commit area, and commit graph.
    pub(crate) source_control: Option<Entity<SourceControlPanel>>,

    // Explorer panel.
    pub(crate) file_explorer: Entity<FileExplorer>,

    // Search panel (ripgrep-backed).
    pub(crate) search_panel: Entity<SearchPanel>,

    // Poll state mirrored for the status bar (avoids borrowing through entity tree).
    pub latest_poll_state: PollState,

    // Held to keep the poller alive; drop aborts the background task.
    // `None` only in tests injecting a watch channel directly (no live repo).
    _poller: Option<Arc<StatusPoller>>,
    _poll_observer: Task<()>,

    theme: Theme,
}

impl RightSidebar {
    /// Build the sidebar for an active project. When `repo` is `Some` the
    /// full git-aware UI is wired (Source Control tab + status poller).
    /// When `None`, only Explorer + Search tabs exist — they read directly
    /// from `root_path`. Source Control tab is hidden via `visible_tabs`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Option<Repository>,
        root_path: PathBuf,
        initial_open: bool,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Channel wiring varies by repo presence. Dead channels in non-git
        // mode mean the explorer / source-control receivers wait forever on
        // `rx.changed()`, which is fine for an idle file explorer.
        let (poller, bar_rx, explorer_rx, sc_rx, panel_rx, initial) = match &repo {
            Some(repo) => {
                let p = Arc::new(StatusPoller::spawn(repo.clone()));
                let bar = p.subscribe();
                let ex = p.subscribe();
                let sc = p.subscribe();
                let panel = p.subscribe();
                let init = p.current();
                (Some(p), bar, ex, sc, panel, init)
            }
            None => {
                let (_bar_tx, bar) = tokio::sync::watch::channel(PollState::Loading);
                let (_ex_tx, ex) = tokio::sync::watch::channel(PollState::Loading);
                let (_sc_tx, sc) = tokio::sync::watch::channel(PollState::Loading);
                let (_panel_tx, panel) = tokio::sync::watch::channel(PollState::Loading);
                (None, bar, ex, sc, panel, PollState::Loading)
            }
        };

        let source_control = repo.as_ref().map(|repo| {
            let diff_view = cx.new(|cx| {
                DiffView::new(repo.clone(), theme, density, typography.clone(), cx)
            });
            let git_panel = cx.new(|cx| {
                GitPanel::new(
                    repo.clone(),
                    panel_rx,
                    Some(diff_view.clone()),
                    theme,
                    density,
                    typography.clone(),
                    cx,
                )
            });
            cx.new(|cx| {
                SourceControlPanel::new(
                    PanelConfig {
                        repo: repo.clone(),
                        theme,
                        density,
                        typography: typography.clone(),
                    },
                    sc_rx,
                    diff_view.clone(),
                    git_panel.clone(),
                    window,
                    cx,
                )
            })
        });

        let file_explorer = cx.new(|cx| {
            FileExplorer::new(
                root_path.clone(),
                explorer_rx,
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });
        let search_panel = cx
            .new(|cx| SearchPanel::new(root_path, theme, density, typography.clone(), window, cx));

        let poll_observer = Self::start_poll_observer(bar_rx, cx);

        let active_tab = if source_control.is_some() {
            RightTab::SourceControl
        } else {
            RightTab::Explorer
        };

        Self {
            open: initial_open,
            active_tab,
            source_control,
            file_explorer,
            search_panel,
            latest_poll_state: initial,
            _poller: poller,
            _poll_observer: poll_observer,
            theme,
        }
    }

    /// Reach into the source-control panel to focus the commit subject input.
    /// Called from `WorkspaceRoot` when Cmd+K fires. No-op for non-git projects.
    pub fn focus_commit_subject(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.source_control.clone() {
            panel.update(cx, |p, cx| p.focus_commit_subject(window, cx));
        }
    }

    /// Update the status poller's focus gate. `WorkspaceRoot` calls this on
    /// window activation changes so polling pauses when the user is
    /// elsewhere and resumes on focus regain.
    pub fn set_polling_focused(&self, focused: bool) {
        if let Some(poller) = &self._poller {
            poller.set_focused(focused);
            if focused {
                // Force an immediate poll so the user doesn't stare at stale
                // status while the 500 ms tick winds down.
                poller.kick();
            }
        }
    }

    /// Test constructor: injects a static watch channel so no tokio thread-pool
    /// task is spawned — keeps GPUI's test scheduler happy (single-thread only).
    /// `has_repo` controls `_poller`: `true` = `Some(Arc<noop>)`, `false` = `None`.
    /// This drives `visible_tabs` and `select_tab` validation without a real poller.
    #[doc(hidden)]
    pub fn new_for_test(
        repo: Repository,
        cfg: SidebarTestConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let SidebarTestConfig {
            state_rx,
            has_repo,
            theme,
            density,
            typography,
        } = cfg;
        // Dead bar_rx: sender dropped immediately so the observer exits on first Err.
        let (_bar_tx, bar_rx) = tokio::sync::watch::channel(PollState::Loading);
        // Dead explorer_rx for tests — never ticks.
        let (_explorer_tx, explorer_rx) = tokio::sync::watch::channel(PollState::Loading);
        // Dead source-control channel — same lifetime as bar.
        let (_sc_tx, sc_rx) = tokio::sync::watch::channel(PollState::Loading);
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
        let source_control = Some(cx.new(|cx| {
            SourceControlPanel::new(
                PanelConfig {
                    repo: repo.clone(),
                    theme,
                    density,
                    typography: typography.clone(),
                },
                sc_rx,
                diff_view.clone(),
                git_panel.clone(),
                window,
                cx,
            )
        }));
        let repo_root = repo.workdir().to_path_buf();
        let file_explorer = cx.new(|cx| {
            FileExplorer::new(
                repo_root.clone(),
                explorer_rx,
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });
        let search_panel = cx
            .new(|cx| SearchPanel::new(repo_root, theme, density, typography.clone(), window, cx));
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
            source_control,
            file_explorer,
            search_panel,
            latest_poll_state: PollState::Loading,
            _poller: poller,
            _poll_observer: poll_observer,
            theme,
        }
    }

    /// Expose the latest poll state so `WorkspaceRoot` can pass it to the status bar.
    pub fn latest_poll_state(&self) -> &PollState {
        &self.latest_poll_state
    }

    /// Whether this sidebar is backed by a live git repo. Non-git projects
    /// instantiate the sidebar in explorer-only mode (no poller), so the
    /// `latest_poll_state` stays at `Loading` forever — callers wanting to
    /// render a git status indicator should gate on this before reading
    /// the poll state.
    pub fn has_repo(&self) -> bool {
        self._poller.is_some()
    }

    /// Tabs the activity bar should expose given current repo presence. Used by
    /// `WorkspaceRoot` to render the tab strip inside the global top bar.
    pub fn visible_tabs(&self) -> Vec<RightTab> {
        visible_tabs(TabVisibility {
            has_repo: self._poller.is_some(),
        })
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        // NOTE: on_action handlers for sidebar keybindings are registered on
        // WorkspaceRoot's outer div (workspace_root.rs::render), not here.
        // RightSidebar is a sibling of MainPane in the row layout — not an ancestor
        // of TerminalView — so on_action here would never fire when the terminal is focused.
        //
        // Activity-bar tabs are rendered inside the global top_bar (see
        // workspace_root.rs) so the chrome reads as one continuous row.
        // This view renders ONLY the panel body now.

        // Closed: render nothing — the right-sidebar toggle lives in the
        // global top_bar (workspace_root.rs), which stays visible and can
        // re-open the panel. Closed state = fully gone, no mini-rail.
        if !self.open {
            return div().into_any_element();
        }

        // Inline each tab body — avoids Box<dyn IntoElement> (trait not dyn-compatible).
        //
        // `min_h(0) + overflow_hidden` on the flex_1 wrappers is load-bearing:
        // a tab body whose inner content has a large intrinsic height (e.g.
        // Source Control with an expanded STAGED CHANGES section listing
        // dozens of rows) would otherwise push past its flex share and bulge
        // the entire sidebar, hiding the chrome above. Clip here so each
        // panel's own scroll region (uniform_list, overflow_y_scroll) owns
        // overflow handling.
        let body = match self.active_tab {
            RightTab::Explorer => div()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .w_full()
                        .overflow_hidden()
                        .child(self.file_explorer.clone()),
                )
                .into_any_element(),
            RightTab::Search => div()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .w_full()
                        .overflow_hidden()
                        .child(self.search_panel.clone()),
                )
                .into_any_element(),
            RightTab::SourceControl => {
                // Source Control tab is filtered out of `visible_tabs` for
                // non-git projects, so `active_tab` should never land here
                // without `source_control`. Guard anyway — render an empty
                // panel rather than panic if a stale tab pointer survives.
                let body_div = div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden();
                match self.source_control.clone() {
                    Some(panel) => body_div
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(0.0))
                                .w_full()
                                .overflow_hidden()
                                .child(panel),
                        )
                        .into_any_element(),
                    None => body_div.into_any_element(),
                }
            }
        };

        // Fixed-width column so it doesn't compete with MainPane's flex_1 in the
        // parent row. bg_panel + border_l isolate the column visually so the
        // terminal pane to the left can't visually bleed into the body area.
        div()
            .id("right-sidebar")
            .flex()
            .flex_col()
            .h_full()
            .w(DEFAULT_PANEL_WIDTH)
            .bg(theme.bg_panel)
            .border_l_1()
            .border_color(theme.border_inactive)
            .child(body)
            .into_any_element()
    }
}
