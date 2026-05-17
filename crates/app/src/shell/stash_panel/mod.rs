//! StashPanel — list git stash entries with per-row Apply / Pop / Drop.
//!
//! Drop is destructive, so the panel only sets a `pending_drop` flag; the
//! shell host (step 14) observes the entity, opens a `ConfirmDialog` with
//! expected = "drop", and calls `drop_pending()` on confirm. Apply and Pop
//! fire directly (Pop is reversible via reflog).
//!
//! Runtime: refresh + ops use `tokio::runtime::Handle::try_current` + the
//! same log+no-op fallback as DiffView / CommitDialog. Refresh is
//! single-flight via `_refresh_task: Option<Task<()>>` — dropping cancels.

pub mod list_render;

use crate::shell::stash_panel::list_render::row_label;
use gpui::{
    App, ClickEvent, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Task, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use oximux_core::{StashEntry, StashRef};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum StashListState {
    Idle,
    Loading,
    Ready(Vec<StashEntry>),
    Failed(String),
}

pub struct StashPanel {
    repo: Repository,
    state: StashListState,
    /// Stash entry the user just clicked "Drop" on. Host watches the
    /// entity and mounts a ConfirmDialog when this becomes Some. Cleared
    /// on `clear_pending_drop()` or `drop_pending()`.
    pending_drop: Option<StashRef>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    _refresh_task: Option<Task<()>>,
    _op_task: Option<Task<()>>,
}

impl StashPanel {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            repo,
            state: StashListState::Idle,
            pending_drop: None,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _refresh_task: None,
            _op_task: None,
        };
        panel.refresh(cx);
        panel
    }

    pub fn state(&self) -> &StashListState {
        &self.state
    }

    pub fn pending_drop(&self) -> Option<&StashRef> {
        self.pending_drop.as_ref()
    }

    pub fn clear_pending_drop(&mut self) {
        self.pending_drop = None;
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.state = StashListState::Loading;
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<StashEntry>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.stash_list().await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::stash_panel",
                    "no tokio runtime; stash_list skipped (step 14 wires runtime)"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |panel, cx| {
                panel.state = match result {
                    Ok(entries) => StashListState::Ready(entries),
                    Err(e) => StashListState::Failed(e),
                };
                cx.notify();
            });
        });
        self._refresh_task = Some(task);
    }

    pub fn apply(&mut self, stash_ref: StashRef, cx: &mut Context<Self>) {
        self.spawn_op(
            move |repo| async move { repo.stash_apply(&stash_ref).await },
            "stash_apply",
            cx,
        );
    }

    pub fn pop(&mut self, stash_ref: StashRef, cx: &mut Context<Self>) {
        self.spawn_op(
            move |repo| async move { repo.stash_pop(&stash_ref).await },
            "stash_pop",
            cx,
        );
    }

    pub fn request_drop(&mut self, stash_ref: StashRef) {
        self.pending_drop = Some(stash_ref);
    }

    pub fn drop_pending(&mut self, cx: &mut Context<Self>) {
        let Some(stash_ref) = self.pending_drop.take() else {
            return;
        };
        self.spawn_op(
            move |repo| async move { repo.stash_drop(&stash_ref).await },
            "stash_drop",
            cx,
        );
    }

    fn spawn_op<F, Fut>(&mut self, op: F, label: &'static str, cx: &mut Context<Self>)
    where
        F: FnOnce(Repository) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = oximux_git::Result<()>> + Send + 'static,
    {
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = op(repo).await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(target: "oximux_app::stash_panel", op = label, "no tokio runtime; op skipped");
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let _result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                // Always refresh after an op — even on failure the user
                // wants to see the current state.
                panel.refresh(cx);
            });
        });
        self._op_task = Some(task);
    }
}

impl Focusable for StashPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StashPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            StashListState::Idle | StashListState::Loading => placeholder(
                "Loading stashes…",
                self.theme,
                self.density,
                &self.typography,
            )
            .into_any_element(),
            StashListState::Failed(err) => placeholder(
                &format!("stash list failed: {err}"),
                self.theme,
                self.density,
                &self.typography,
            )
            .into_any_element(),
            StashListState::Ready(entries) if entries.is_empty() => {
                placeholder("No stashes", self.theme, self.density, &self.typography)
                    .into_any_element()
            }
            StashListState::Ready(entries) => {
                let mut col = div().flex().flex_col().h_full().w_full();
                for entry in entries.iter().cloned() {
                    col = col.child(self.render_row(entry, cx));
                }
                col.into_any_element()
            }
        };
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_panel)
            .border_l_1()
            .border_color(self.theme.border_inactive)
            .child(body)
    }
}

impl StashPanel {
    fn render_row(&self, entry: StashEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let label = row_label(&entry);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let index = entry.stash_ref.index;
        let apply_ref = entry.stash_ref.clone();
        let pop_ref = entry.stash_ref.clone();
        let drop_ref = entry.stash_ref.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(density.h_row * 1.4))
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
            )
            .child(
                Button::new(("stash-apply", index))
                    .ghost()
                    .label("Apply")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.apply(apply_ref.clone(), cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("stash-pop", index))
                    .ghost()
                    .label("Pop")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.pop(pop_ref.clone(), cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("stash-drop", index))
                    .danger()
                    .label("Drop")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.request_drop(drop_ref.clone());
                        cx.notify();
                    })),
            )
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
        .h_full()
        .p(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
