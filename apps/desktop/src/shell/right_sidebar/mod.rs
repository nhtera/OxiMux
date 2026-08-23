//! RightSidebar — tab-switchable activity-bar panel replacing the fixed git column.
//!
//! Owns the StatusPoller lifetime, mirrors poll state for the status bar,
//! and dispatches to Explorer / Search / Source Control tab bodies.

pub mod activity_bar;
pub mod layout;
pub mod resize;
pub mod session_history_panel;
pub mod tab;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    Styled, Task, Window, div, px,
};
use oximux_git::{PollState, Repository, StatusPoller};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::{SettingsRepo, WorktreeSettingsRepo};

use crate::git_state_cache::GitStateCache;
use crate::scm_layout_settings;
use crate::shell::diff_view::DiffView;
use crate::shell::file_explorer::FileExplorer;
use crate::shell::file_tree_view::{FileTreeView, OnOpenDiff, OnOpenFile, OnQueryActivePath};
use crate::shell::git_panel::GitPanel;
use crate::shell::right_sidebar::layout::DEFAULT_PANEL_WIDTH;
use crate::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};
use crate::shell::search_panel::SearchPanel;
use crate::shell::source_control::{PanelConfig, SourceControlPanel};
use oximux_editor::FileTree;

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

/// Search for the SettingsRepo + Pixels initial width pair so callers
/// can pass them as a single arg through `RightSidebar::new` and the
/// constructor stays comfortably under the 7-arg clippy limit.
#[derive(Clone)]
pub struct SidebarLayoutBoot {
    /// Initial sidebar width (already loaded + clamped against current
    /// window width by the caller). `None` falls back to the
    /// `DEFAULT_PANEL_WIDTH` constant — used in tests and any code
    /// path that doesn't have a SettingsRepo handy.
    pub initial_width: Option<Pixels>,
    /// Global key/value settings store. When present the sidebar
    /// persists width changes on every drag tick; when absent
    /// (test wiring) resize still works but state evaporates on
    /// teardown.
    pub settings_repo: Option<SettingsRepo>,
}

impl SidebarLayoutBoot {
    /// Test wiring: no persisted width, no settings repo. The sidebar
    /// falls back to `DEFAULT_PANEL_WIDTH` and resize ticks update
    /// state in-memory only.
    pub fn for_test() -> Self {
        Self {
            initial_width: None,
            settings_repo: None,
        }
    }
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

    // Session History panel — past Claude sessions, reopen as chat. Always
    // present (repo-independent); scoped to this workspace's root by default.
    pub(crate) session_history: Entity<session_history_panel::SessionHistoryPanel>,

    // Files tab — workspace file tree. `FileTreeView` holds an
    // `Entity<FileTree>` internally, which keeps the model + watcher alive
    // via reference counting, so storing a separate handle here would be
    // dead weight. `None` when the host hasn't supplied an `on_open`
    // callback yet (tests).
    pub(crate) file_tree_view: Option<Entity<FileTreeView>>,

    // Ports panel. **Not owned here**, unlike every other panel above: the
    // sidebar is cached per project, but a port is a fact about the whole
    // window, and one panel per project would mean N copies of one list
    // disagreeing about which of them the scan last updated. `WorkspaceRoot`
    // owns the single panel and hands the same entity to every sidebar it
    // builds. `None` only before that handoff (and in tests).
    pub(crate) ports_panel: Option<Entity<crate::shell::ports_panel::PortsPanel>>,

    // Poll state mirrored for the status bar (avoids borrowing through entity tree).
    pub latest_poll_state: PollState,

    // Held to keep the poller alive; drop aborts the background task.
    // `None` only in tests injecting a watch channel directly (no live repo).
    _poller: Option<Arc<StatusPoller>>,
    _poll_observer: Task<()>,

    // ----- Phase 13: panel-width state -----
    /// Live sidebar width in pixels. Read by `panel_width()` from
    /// `WorkspaceRoot` for the chrome-width forwarding into
    /// ProjectPanes, and by `render()` for the column's `w(...)` style.
    panel_width: Pixels,
    /// True while a resize drag is in flight. Set on every drag tick
    /// (`resize::apply_drag_move`), cleared on the first render after
    /// the drag ends. Drives the handle's highlight bar — hover styles
    /// are suppressed during drags, so the lit state must come from
    /// sidebar state instead.
    resizing: bool,
    /// Global settings store for persistence on resize. `None` =
    /// test/non-persistent mount; setter just updates state.
    settings_repo: Option<SettingsRepo>,

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
        on_open_file: Option<OnOpenFile>,
        on_open_diff: Option<OnOpenDiff>,
        on_query_active_path: Option<OnQueryActivePath>,
        worktree_settings_repo: Option<WorktreeSettingsRepo>,
        layout_boot: SidebarLayoutBoot,
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
                // Stale-while-revalidate: seed the poller from the last-known
                // `GitState` (in-session cache, persisted across launches) so
                // the status bar + SCM panel paint the prior snapshot instantly
                // instead of "loading git…". A cache miss seeds `Loading` and
                // behaves exactly as before. The first poll still fires right
                // away and overwrites the seed ~one `status()` later.
                let seed = cx
                    .try_global::<GitStateCache>()
                    .and_then(|c| c.get(repo.workdir()))
                    .map(PollState::Ready)
                    .unwrap_or(PollState::Loading);
                let p = Arc::new(StatusPoller::spawn_seeded(repo.clone(), seed));
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
            let diff_view =
                cx.new(|cx| DiffView::new(repo.clone(), theme, density, typography.clone(), cx));
            // SCM row clicks open a diff tab in the main pane via the
            // host's `OnOpenDiff` callback (the inline sidebar mini-diff
            // remains as a glanceable preview). `OnOpenFile` is no longer
            // passed to GitPanel — the diff path covers the use case
            // and avoids spawning both a diff tab and a plain editor
            // tab on the same click.
            let on_open_for_scm = on_open_diff.clone();
            let git_panel = cx.new(|cx| {
                GitPanel::new(
                    repo.clone(),
                    panel_rx,
                    Some(diff_view.clone()),
                    theme,
                    density,
                    typography.clone(),
                    on_open_for_scm,
                    worktree_settings_repo.clone(),
                    cx,
                )
            });
            let sc_settings_repo = layout_boot.settings_repo.clone();
            cx.new(|cx| {
                SourceControlPanel::new(
                    PanelConfig {
                        repo: repo.clone(),
                        theme,
                        density,
                        typography: typography.clone(),
                        worktree_settings_repo: worktree_settings_repo.clone(),
                        // Phase 13: hand the SCM panel a clone of the
                        // global settings repo so CommitGraph can
                        // persist `scm_graph_height` on resize.
                        settings_repo: sc_settings_repo.clone(),
                        // SCM panel's ConflictSummaryCard "Open all
                        // in editor" needs the host file-open
                        // callback. `OnOpenFile` is `Arc<dyn Fn ...>`
                        // — clone is O(1).
                        on_open_file: on_open_file.clone(),
                    },
                    sc_rx,
                    diff_view.clone(),
                    git_panel.clone(),
                    window,
                    cx,
                )
            })
        });

        // Clone the on_open callback so both Explorer and FileTreeView can
        // own it. `OnOpenFile` is `Arc<dyn Fn ...>` — clone is O(1).
        let on_open_for_explorer = on_open_file.clone();
        let file_explorer = cx.new(|cx| {
            FileExplorer::new(
                root_path.clone(),
                explorer_rx,
                theme,
                density,
                typography.clone(),
                on_open_for_explorer,
                window,
                cx,
            )
        });
        let search_panel = cx.new(|cx| {
            SearchPanel::new(
                root_path.clone(),
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });
        let session_history = cx.new(|cx| {
            session_history_panel::SessionHistoryPanel::new(
                root_path.clone(),
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });

        // Files tab: construct the FileTree + FileTreeView only when the host
        // supplied an `on_open` callback. Tests skip the callback (no live
        // host to wire) and the tab body falls back to an empty placeholder.
        // The `FileTree` entity is moved into `FileTreeView::new`, which
        // owns it for the rest of its lifetime — no separate handle is
        // retained here.
        let file_tree_view = on_open_file.map(|on_open| {
            let tree = cx.new(|cx| FileTree::new(root_path, cx));
            cx.new(|cx| FileTreeView::new(tree, theme, on_open, on_query_active_path, window, cx))
        });

        let poll_observer = Self::start_poll_observer(bar_rx, cx);

        // Default to SourceControl when a repo is present; otherwise Explorer.
        // Files is no longer in the visible-tab list (see tab.rs::visible_tabs)
        // so we mustn't default to it — `select_tab` would clamp it to Explorer
        // anyway, but starting on Explorer skips the silent clamp.
        let active_tab = if source_control.is_some() {
            RightTab::SourceControl
        } else {
            RightTab::Explorer
        };

        let SidebarLayoutBoot {
            initial_width,
            settings_repo,
        } = layout_boot;
        let panel_width = initial_width.unwrap_or(DEFAULT_PANEL_WIDTH);

        Self {
            open: initial_open,
            active_tab,
            source_control,
            file_explorer,
            search_panel,
            session_history,
            file_tree_view,
            // Handed over by `WorkspaceRoot` after construction — see the
            // field's own note on why it is not built here.
            ports_panel: None,
            latest_poll_state: initial,
            _poller: poller,
            _poll_observer: poll_observer,
            panel_width,
            resizing: false,
            settings_repo,
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
                None, // test wiring: no host on_open
                None, // test wiring: no per-worktree settings persistence
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
                    // Test wiring: no persistence layer — the panel still
                    // works for in-memory base-ref picks.
                    worktree_settings_repo: None,
                    // Test wiring: no global settings repo — graph
                    // height + sidebar width default constants apply.
                    settings_repo: None,
                    // Test wiring: no host file-open callback — the
                    // ConflictSummaryCard's "Open all in editor"
                    // button stays disabled.
                    on_open_file: None,
                },
                sc_rx,
                diff_view.clone(),
                git_panel.clone(),
                window,
                cx,
            )
        }));
        let repo_root = repo.workdir().to_path_buf();
        // Test constructor: no host on_open callback — pass `None` so
        // file clicks during tests silently no-op (rather than triggering
        // `std::process::Command::new("open")` like before).
        let file_explorer = cx.new(|cx| {
            FileExplorer::new(
                repo_root.clone(),
                explorer_rx,
                theme,
                density,
                typography.clone(),
                None,
                window,
                cx,
            )
        });
        let search_panel = cx.new(|cx| {
            SearchPanel::new(repo_root.clone(), theme, density, typography.clone(), window, cx)
        });
        let session_history = cx.new(|cx| {
            session_history_panel::SessionHistoryPanel::new(
                repo_root,
                theme,
                density,
                typography.clone(),
                window,
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
            source_control,
            file_explorer,
            search_panel,
            session_history,
            file_tree_view: None,
            ports_panel: None,
            latest_poll_state: PollState::Loading,
            _poller: poller,
            _poll_observer: poll_observer,
            panel_width: DEFAULT_PANEL_WIDTH,
            resizing: false,
            settings_repo: None,
            theme,
        }
    }

    /// Live width of the panel column in pixels. `WorkspaceRoot`
    /// reads this to forward the correct chrome width into
    /// `ProjectPanes::set_chrome_width` after a resize, and the
    /// adapter-picker / pane-actions anchors use it to compute the
    /// right-edge offset.
    pub fn panel_width(&self) -> Pixels {
        self.panel_width
    }

    /// Apply a new panel width — clamps against the current window's
    /// expected ceiling (caller already did this), updates state,
    /// notifies the view tree to re-flow, and persists to
    /// `SettingsRepo` when one is wired. Called from the
    /// drag-move handler in `resize::apply_drag_move`.
    pub fn set_panel_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        if self.panel_width == width {
            return;
        }
        self.panel_width = width;
        if let Some(repo) = &self.settings_repo {
            scm_layout_settings::save_panel_width(repo, f32::from(width));
        }
        cx.notify();
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

    /// Adopt the window's single ports panel.
    ///
    /// Called by `WorkspaceRoot` for every sidebar it builds or restores from
    /// its per-project cache, so all of them render the same entity — see the
    /// field's note for why the panel is not built per sidebar.
    pub fn set_ports_panel(
        &mut self,
        panel: Entity<crate::shell::ports_panel::PortsPanel>,
        cx: &mut Context<Self>,
    ) {
        self.ports_panel = Some(panel);
        cx.notify();
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

    /// Focus the Session History panel's search field (so keyboard nav/filter
    /// work the instant the tab is opened).
    pub fn focus_history(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_history
            .update(cx, |p, cx| p.focus(window, cx));
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A resize drag is over once no drag is active — releasing the
        // button produces no further drag-move ticks, so the flag is
        // cleared here on the next render instead.
        if self.resizing && !cx.has_active_drag() {
            self.resizing = false;
        }
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
            RightTab::Files => {
                let body_div = div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden();
                match self.file_tree_view.clone() {
                    Some(view) => body_div
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(0.0))
                                .w_full()
                                .overflow_hidden()
                                .child(view),
                        )
                        .into_any_element(),
                    // No callback wired — `new_for_test` or pre-host mount;
                    // render an empty body rather than panic.
                    None => body_div.into_any_element(),
                }
            }
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
            RightTab::History => div()
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
                        .child(self.session_history.clone()),
                )
                .into_any_element(),
            RightTab::Ports => {
                let body_div = div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden();
                // Rendered empty before `WorkspaceRoot` hands the panel over,
                // and in tests that mount a sidebar with no host — same shape
                // the Files tab uses for the same reason.
                match self.ports_panel.clone() {
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

        // Width is now state, not const — see set_panel_width / the
        // Phase 13 drag handle below. bg_panel + border_l isolate the
        // column visually so the terminal pane to the left can't bleed
        // into the body area.
        //
        // Phase 13: the drag handle sits at the column's left edge as
        // a sibling of the body in a horizontal flex row. Cursor pickup
        // is forgiving (7px hitbox around a 1px visible stripe) and the
        // matching on_drag_move listener lives on WorkspaceRoot's outer
        // row so the cursor stays inside the listener's bounds across
        // the full drag.
        let window_width = f32::from(window.bounds().size.width);
        div()
            .id("right-sidebar")
            .flex()
            .flex_row()
            .h_full()
            .w(self.panel_width)
            .bg(theme.bg_panel)
            .child(resize::build_handle(window_width, self.resizing, theme))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .child(body),
            )
            .into_any_element()
    }
}
