//! WorkspaceRoot — the top-level view mounted into the GPUI window.
//!
//! Composition (vertical):
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐  ← TopBar (36px)
//! ├──────────┬──────────────────────────┬─────────┬─────────┤
//! │ Sidebar  │        MainPane          │ GitPnl  │ DiffVw  │  ← HSplit
//! │ (240px)  │     (Phase 1 grid)       │ (260px) │ (260px) │
//! ├──────────┴──────────────────────────┴─────────┴─────────┤  ← StatusBar (22px)
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Git column (`GitPnl` + `DiffVw`) renders only when a repository was
//! opened at startup. Without one, the layout collapses to the Phase 1
//! `Sidebar | MainPane | StatusBar` form.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Task, Window, div, px,
};
use oximux_git::{PollState, Repository, StatusPoller};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::{
    diff_view::DiffView,
    git_panel::GitPanel,
    main_area,
    main_pane::MainPane,
    sidebar, status_bar,
    tabbed_pane::TabbedPane,
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView},
    top_bar,
};

/// Git surface mounted into the workspace. Owns the StatusPoller so it
/// outlives the GitPanel and DiffView, and mirrors the latest poll state
/// into `latest_poll_state` so the status bar can read it without
/// borrowing the poller through the entity tree.
pub struct GitMount {
    /// Held to keep the poller alive (drop aborts the background task).
    _poller: StatusPoller,
    git_panel: Entity<GitPanel>,
    diff_view: Entity<DiffView>,
    latest_poll_state: PollState,
    _poll_observer: Task<()>,
}

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
    main_pane: Option<Entity<MainPane>>,
    git: Option<GitMount>,
}

impl WorkspaceRoot {
    pub fn new(repo: Option<Repository>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::charcoal();
        let density = Density::cockpit();
        let typography = Typography::cockpit();

        let main_pane = spawn_initial_pane(theme, density, typography.clone(), window, cx);
        let git = repo.map(|r| build_git_mount(r, theme, density, typography.clone(), cx));

        Self {
            theme,
            density,
            typography,
            main_pane,
            git,
        }
    }
}

fn build_git_mount(
    repo: Repository,
    theme: Theme,
    density: Density,
    typography: Typography,
    cx: &mut Context<WorkspaceRoot>,
) -> GitMount {
    let poller = StatusPoller::spawn(repo.clone());
    let panel_rx = poller.subscribe();
    let bar_rx = poller.subscribe();
    let initial = poller.current();
    let diff_view = cx.new(|cx| {
        DiffView::new(repo.clone(), theme, density, typography.clone(), cx)
    });
    let diff_view_for_panel = diff_view.clone();
    let git_panel = cx.new(|cx| {
        GitPanel::new(
            repo,
            panel_rx,
            Some(diff_view_for_panel),
            theme,
            density,
            typography,
            cx,
        )
    });
    let poll_observer = start_poll_observer(bar_rx, cx);
    GitMount {
        _poller: poller,
        git_panel,
        diff_view,
        latest_poll_state: initial,
        _poll_observer: poll_observer,
    }
}

fn start_poll_observer(
    mut rx: tokio::sync::watch::Receiver<PollState>,
    cx: &mut Context<WorkspaceRoot>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            if rx.changed().await.is_err() {
                return;
            }
            let state = rx.borrow_and_update().clone();
            if this
                .update(cx, |root, cx| {
                    if let Some(g) = root.git.as_mut() {
                        g.latest_poll_state = state;
                        cx.notify();
                    }
                })
                .is_err()
            {
                return;
            }
        }
    })
}

fn spawn_initial_pane(
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<MainPane>> {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        ..SpawnConfig::default()
    };
    let session_id = match backend.spawn(cfg) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "pty spawn failed; rendering placeholder");
            return None;
        }
    };
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
    let initial_pane = cx.new(|cx| TabbedPane::new(initial_view, cx));
    Some(cx.new(|cx| MainPane::new(initial_pane, theme, density, typography, cx)))
}

impl Render for WorkspaceRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        let main = div().flex().flex_1().h_full().min_w(px(0.));
        let main = match self.main_pane.clone() {
            Some(view) => main.child(view),
            None => main.child(main_area::view(theme, density, typography)),
        };

        let pane_count = self
            .main_pane
            .as_ref()
            .map(|mp| mp.read(cx).leaf_count())
            .unwrap_or(0);

        let git_column = self.git.as_ref().map(|g| {
            div()
                .flex()
                .flex_row()
                .h_full()
                .child(
                    div()
                        .w(px(260.0))
                        .h_full()
                        .child(g.git_panel.clone()),
                )
                .child(div().w(px(260.0)).h_full().child(g.diff_view.clone()))
        });

        let row = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            .child(sidebar::view(theme, density, typography))
            .child(main);
        let row = match git_column {
            Some(col) => row.child(col),
            None => row,
        };

        let git_state = self.git.as_ref().map(|g| &g.latest_poll_state);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            .child(top_bar::view(theme, density, typography))
            .child(row)
            .child(status_bar::view(
                theme, density, typography, pane_count, git_state,
            ))
    }
}
