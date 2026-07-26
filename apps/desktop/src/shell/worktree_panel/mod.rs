//! WorktreePanel — list worktrees, create new (slug-based branch
//! `oximux/<slug>`), remove with confirm.
//!
//! Create form: slug InputState + suggested path InputState + Create button.
//! Slug pre-validation is server-side (Repository::add_worktree calls
//! `validate_slug` internally and returns a user-friendly error); the panel
//! surfaces that error in the create-form status row rather than reinventing
//! the rule list here. KISS — the source of truth stays in `oximux-git`.
//!
//! Remove: per-row button sets `pending_remove`; the shell host (step 14)
//! observes and mounts a ConfirmDialog naming the worktree before calling
//! `remove_pending()`.

pub mod list_render;

use crate::shell::worktree_panel::list_render::{row_label, suggest_worktree_path};
use crate::ui::danger_ghost;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Task, Window, div, px,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_core::WorktreeInfo;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum WorktreeListState {
    Idle,
    Loading,
    Ready(Vec<WorktreeInfo>),
    Failed(String),
}

#[derive(Debug, Default)]
pub enum CreateState {
    #[default]
    Editing,
    Creating,
    Failed(String),
    Done(PathBuf),
}

pub struct WorktreePanel {
    repo: Repository,
    state: WorktreeListState,
    create_state: CreateState,
    slug_state: Entity<InputState>,
    path_state: Entity<InputState>,
    pending_remove: Option<PathBuf>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    _refresh_task: Option<Task<()>>,
    _create_task: Option<Task<()>>,
    _remove_task: Option<Task<()>>,
}

impl WorktreePanel {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let slug_state = cx.new(|cx| InputState::new(window, cx).placeholder("slug (no spaces)"));
        let path_state = cx.new(|cx| InputState::new(window, cx).placeholder("path (auto)"));
        let mut panel = Self {
            repo,
            state: WorktreeListState::Idle,
            create_state: CreateState::Editing,
            slug_state,
            path_state,
            pending_remove: None,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _refresh_task: None,
            _create_task: None,
            _remove_task: None,
        };
        panel.refresh(cx);
        panel
    }

    pub fn state(&self) -> &WorktreeListState {
        &self.state
    }
    pub fn create_state(&self) -> &CreateState {
        &self.create_state
    }
    pub fn pending_remove(&self) -> Option<&PathBuf> {
        self.pending_remove.as_ref()
    }
    pub fn clear_pending_remove(&mut self) {
        self.pending_remove = None;
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.state = WorktreeListState::Loading;
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<WorktreeInfo>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.list_worktrees().await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(target: "oximux_app::worktree_panel", "no tokio runtime");
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |panel, cx| {
                panel.state = match result {
                    Ok(list) => WorktreeListState::Ready(list),
                    Err(e) => WorktreeListState::Failed(e),
                };
                cx.notify();
            });
        });
        self._refresh_task = Some(task);
    }

    pub fn submit_create(&mut self, cx: &mut Context<Self>) {
        if matches!(self.create_state, CreateState::Creating) {
            return;
        }
        let slug = self.slug_state.read(cx).value().to_string();
        let path_text = self.path_state.read(cx).value().to_string();
        let slug = slug.trim().to_string();
        if slug.is_empty() {
            self.create_state = CreateState::Failed("slug is empty".to_string());
            return;
        }
        let path = if path_text.trim().is_empty() {
            PathBuf::from(suggest_worktree_path(self.repo.workdir(), &slug))
        } else {
            PathBuf::from(path_text.trim())
        };
        self.create_state = CreateState::Creating;
        let repo = self.repo.clone();
        let path_for_op = path.clone();
        let (tx, rx) = oneshot::channel::<Result<PathBuf, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo
                        .add_worktree(&path_for_op, &slug)
                        .await
                        .map(|info| info.path)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                self.create_state = CreateState::Failed("no tokio runtime (step 14)".to_string());
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |panel, cx| {
                match result {
                    Ok(p) => panel.create_state = CreateState::Done(p),
                    Err(e) => panel.create_state = CreateState::Failed(e),
                }
                panel.refresh(cx);
            });
        });
        self._create_task = Some(task);
    }

    pub fn request_remove(&mut self, path: PathBuf) {
        self.pending_remove = Some(path);
    }

    pub fn remove_pending(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.pending_remove.take() else {
            return;
        };
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo
                        .remove_worktree(&path, false)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => return,
        }
        let task = cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                if let Ok(Err(err)) = &result {
                    crate::shell::toast::toast_op_error(cx, "Remove worktree", err);
                }
                panel.refresh(cx);
            });
        });
        self._remove_task = Some(task);
    }
}

impl Focusable for WorktreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorktreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let create_status_msg = match &self.create_state {
            CreateState::Editing => String::new(),
            CreateState::Creating => "Creating…".to_string(),
            CreateState::Failed(e) => format!("Failed: {e}"),
            CreateState::Done(p) => format!("Created at {}", p.display()),
        };
        let create_status_color = match &self.create_state {
            CreateState::Editing | CreateState::Creating => theme.fg_subtle,
            CreateState::Failed(_) => theme.status_error,
            CreateState::Done(_) => theme.status_ok,
        };
        let list = self.render_list(cx);
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_panel)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .p(px(density.pad_panel))
                    .gap(px(density.gap_inline))
                    .border_b_1()
                    .border_color(theme.border_inactive)
                    .child(
                        div()
                            .text_size(px(typography.t_label_caps))
                            .font_weight(typography.w_semibold)
                            .text_color(theme.fg_muted)
                            .child("NEW WORKTREE"),
                    )
                    .child(Input::new(&self.slug_state))
                    .child(Input::new(&self.path_state))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(density.gap_inline))
                            .child(
                                Button::new("worktree-create")
                                    .primary()
                                    .label("Create")
                                    .on_click(cx.listener(|p, _: &ClickEvent, _window, cx| {
                                        p.submit_create(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(typography.t_body_sm))
                                    .text_color(create_status_color)
                                    .child(create_status_msg),
                            ),
                    ),
            )
            .child(list)
    }
}

impl WorktreePanel {
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        match &self.state {
            WorktreeListState::Idle | WorktreeListState::Loading => placeholder(
                "Loading worktrees…",
                self.theme,
                self.density,
                &self.typography,
            )
            .into_any_element(),
            WorktreeListState::Failed(e) => placeholder(
                &format!("worktree list failed: {e}"),
                self.theme,
                self.density,
                &self.typography,
            )
            .into_any_element(),
            WorktreeListState::Ready(list) => {
                let mut col = div().flex().flex_col().h_full().w_full();
                for (i, w) in list.iter().cloned().enumerate() {
                    col = col.child(self.render_row(i, w, cx));
                }
                col.into_any_element()
            }
        }
    }

    fn render_row(&self, idx: usize, w: WorktreeInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let label = row_label(&w);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let row_path = w.path.clone();
        let is_main = w.is_main;
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(density.h_action_row))
            .px(px(density.pad_panel))
            .gap(px(density.gap_inline))
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                div()
                    .flex_1()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_base)
                    .child(label),
            );
        if !is_main {
            row = row.child(danger_ghost(
                ("worktree-remove", idx),
                "Remove",
                &theme,
                &density,
                typography,
                cx.listener(move |p, _: &ClickEvent, _window, cx| {
                    p.request_remove(row_path.clone());
                    cx.notify();
                }),
            ));
        }
        row
    }
}

fn placeholder(
    msg: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(80.0))
        .p(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
