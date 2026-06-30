//! Tasks page — rendered when the Tasks pane tab is active.
//!
//! A GitHub issue/PR browser for the active project's repo. Owns its own async
//! fetch + filter state (kind, state, assigned-to-me) so the list survives
//! re-renders; data is fetched through the [`ForgeProvider`] seam (never `gh`
//! directly). Mounted by the pane system as an `Entity<TasksView>`; row actions
//! (create workspace, open in browser) dispatch up via `weak_root`.
//!
//! Layout (top → bottom):
//!   1. Toolbar    — `table::render_toolbar`
//!   2. Col-header — `table::render_col_header`
//!   3. Body       — virtualized `uniform_list` of `row::render_task_row`

pub mod row;
mod table;

use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, UniformListScrollHandle, WeakEntity, Window,
    div, px, uniform_list,
};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::{Forge, ForgeItem, ForgeListFilter, ForgeProvider, ForgeState};
use crate::shell::tasks_view::row::render_task_row;
use crate::shell::tasks_view::table::{render_col_header, render_toolbar};
use crate::workspace_root::WorkspaceRoot;

/// Whether the page is listing issues or pull requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Issues,
    Prs,
}

/// Identifies what a given `items` snapshot was fetched for, so re-activating
/// the page with the same project + filter doesn't re-hit the network.
type FetchKey = (String, TaskKind, ForgeState, bool);

pub struct TasksView {
    weak_root: WeakEntity<WorkspaceRoot>,
    /// Focus handle so `PaneContent::Tasks` can satisfy `Focusable` and
    /// forward focus correctly. The Tasks view is read-only (no text input),
    /// so this handle is held but focus is not actively routed to it.
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Active project the list is scoped to. `None` until a project is opened.
    project: Option<Project>,
    kind: TaskKind,
    filter: ForgeListFilter,
    items: Vec<ForgeItem>,
    loading: bool,
    loaded_key: Option<FetchKey>,
    scroll: UniformListScrollHandle,
    _fetch_task: Option<gpui::Task<()>>,
}

impl TasksView {
    pub fn new(
        weak_root: WeakEntity<WorkspaceRoot>,
        theme: Theme,
        density: Density,
        typography: Typography,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            weak_root,
            focus_handle: _cx.focus_handle(),
            theme,
            density,
            typography,
            project: None,
            kind: TaskKind::Issues,
            filter: ForgeListFilter::default(),
            items: Vec::new(),
            loading: false,
            loaded_key: None,
            scroll: UniformListScrollHandle::new(),
            _fetch_task: None,
        }
    }

    /// Push the active project. Stores it without fetching — the pane system
    /// calls [`TasksView::activate`]/[`TasksView::refresh`] to drive the network.
    pub fn set_project(&mut self, project: Option<Project>, cx: &mut Context<Self>) {
        let changed = self.project.as_ref().map(|p| &p.id) != project.as_ref().map(|p| &p.id);
        self.project = project;
        if changed {
            cx.notify();
        }
    }

    /// Called when the page becomes visible. Fetches only when the current
    /// project + filter hasn't already been loaded (cheap nav toggling).
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        if self.loaded_key.is_some() && self.loaded_key == self.current_key() {
            return;
        }
        self.fetch(cx);
    }

    /// Force a re-fetch (Refresh button / live project switch).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loaded_key = None;
        self.fetch(cx);
    }

    fn current_key(&self) -> Option<FetchKey> {
        self.project
            .as_ref()
            .map(|p| (p.id.clone(), self.kind, self.filter.state, self.filter.mine))
    }

    pub(super) fn set_kind(&mut self, kind: TaskKind, cx: &mut Context<Self>) {
        if self.kind != kind {
            self.kind = kind;
            self.fetch(cx);
        }
    }

    pub(super) fn set_state(&mut self, state: ForgeState, cx: &mut Context<Self>) {
        if self.filter.state != state {
            self.filter.state = state;
            self.fetch(cx);
        }
    }

    pub(super) fn toggle_mine(&mut self, cx: &mut Context<Self>) {
        self.filter.mine = !self.filter.mine;
        self.fetch(cx);
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            self.items.clear();
            self.loading = false;
            self.loaded_key = None;
            cx.notify();
            return;
        };
        self.loaded_key = self.current_key();
        self.loading = true;
        // Drop the previous result so a kind/state/Mine switch shows the
        // loading state instead of stale rows from the old query.
        self.items.clear();
        cx.notify();

        let cwd = PathBuf::from(&project.root_path);
        let kind = self.kind;
        let filter = self.filter;
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<ForgeItem>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    // Unsupported remote (neither GitHub nor GitLab) → empty
                    // list without firing a forge CLI against a foreign host.
                    let items = match Forge::detect(&cwd).await {
                        Some(forge) => match kind {
                            TaskKind::Issues => forge.list_issues(&cwd, filter).await,
                            TaskKind::Prs => forge.list_prs(&cwd, filter).await,
                        },
                        None => Vec::new(),
                    };
                    let _ = tx.send(items);
                });
            }
            Err(_) => {
                // No tokio runtime (headless test) — leave items as-is, clear
                // the spinner so the page doesn't hang.
                self.loading = false;
                cx.notify();
                return;
            }
        }
        self._fetch_task = Some(cx.spawn(async move |this, cx| {
            let items = rx.await.unwrap_or_default();
            let _ = this.update(cx, |tv, cx| {
                tv.items = items;
                tv.loading = false;
                cx.notify();
            });
        }));
    }

    fn render_body(&self) -> AnyElement {
        let theme = self.theme;
        let Some(project) = self.project.clone() else {
            return self.hint("Open a project to browse its issues.");
        };
        if self.loading && self.items.is_empty() {
            return self.hint("Loading\u{2026}");
        }
        if self.items.is_empty() {
            let what = match self.kind {
                TaskKind::Issues => "issues",
                TaskKind::Prs => "pull requests",
            };
            return self.hint(&format!(
                "No {what}. Requires the gh CLI authenticated for a GitHub repo."
            ));
        }

        let items = self.items.clone();
        let density = self.density;
        let typography = self.typography.clone();
        let kind = self.kind;
        let weak_root = self.weak_root.clone();
        let row_count = items.len();
        let list = uniform_list(
            "tasks-rows",
            row_count,
            move |range: std::ops::Range<usize>, _window, _cx| {
                range
                    .filter_map(|i| items.get(i).cloned())
                    .map(|item| {
                        render_task_row(
                            &item,
                            kind,
                            weak_root.clone(),
                            project.clone(),
                            theme,
                            density,
                            &typography,
                        )
                    })
                    .collect::<Vec<AnyElement>>()
            },
        )
        .track_scroll(&self.scroll)
        .h_full();
        div().flex_1().w_full().child(list).into_any_element()
    }

    fn hint(&self, text: &str) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .w_full()
            .p(px(self.density.pad_panel))
            .text_size(px(self.typography.t_body_sm))
            .text_color(self.theme.fg_subtle)
            .child(text.to_string())
            .into_any_element()
    }
}

impl Focusable for TasksView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TasksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = render_toolbar(self.kind, &self.filter, self.theme, self.density, &self.typography, cx);
        let col_header = render_col_header(self.theme, self.density, &self.typography);
        let body = self.render_body();
        // Tasks sits on the content canvas (`bg_panel`), not the rail surface.
        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_panel)
            .child(toolbar)
            .child(col_header)
            .child(body)
    }
}
