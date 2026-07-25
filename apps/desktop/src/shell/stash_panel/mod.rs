//! StashPanel — list git stash entries with per-row Apply / Pop / Drop.
//!
//! Drop is destructive, so the panel only sets a `pending_drop` flag; the
//! shell host (step 14) observes the entity, opens a `ConfirmDialog`, and
//! calls `drop_pending()` on confirm. Apply and Pop fire directly (Pop is
//! reversible via reflog).
//!
//! Layout:
//!   - Always-rendered header: chevron + "STASHES (N)" + "+" push button.
//!   - Body: list (or "No stashes" placeholder). Hidden when collapsed
//!     (default). Power-user surface; eats no visual real estate when
//!     unused.
//!
//! Runtime: refresh + ops use `tokio::runtime::Handle::try_current` + the
//! same log+no-op fallback as DiffView / CommitDialog. Refresh is
//! single-flight via `_refresh_task: Option<Task<()>>` — dropping cancels.

pub mod list_render;
pub mod push_dialog;

use crate::shell::stash_panel::list_render::row_label;
use crate::ui::danger_ghost;
use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Task, Window, div, px,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants},
};
use oximux_core::{StashEntry, StashRef};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use tokio::sync::oneshot;

/// Resting opacity of a stash row's Apply/Pop/Drop cluster — ghosted enough to
/// calm the row, present enough that the actions are always discoverable and
/// clickable (the panel has no context-menu fallback). Lifts to full on
/// row-hover.
const STASH_ACTION_REST_OPACITY: f32 = 0.45;

#[derive(Debug)]
pub enum StashListState {
    Idle,
    Loading,
    Ready(Vec<StashEntry>),
    Failed(String),
}

/// Emitted when the user clicks the header `+` button. The host
/// (`SourceControlPanel` via `WorkspaceRoot`) subscribes and mounts a
/// `PushStashDialog`. Routed through an event rather than a direct
/// callback so the panel stays free of host-modal coupling.
#[derive(Debug, Clone, Copy)]
pub struct PushStashRequested;

pub struct StashPanel {
    repo: Repository,
    state: StashListState,
    /// Stash entry the user just clicked "Drop" on. Host watches the
    /// entity and mounts a ConfirmDialog when this becomes Some. Cleared
    /// on `clear_pending_drop()` or `drop_pending()`.
    pending_drop: Option<StashRef>,
    /// Body visibility flag. Default `true` — the stash list is a
    /// power-user surface; keeping it collapsed by default avoids
    /// burning vertical real estate in the SCM tab for users who don't
    /// rely on git stash. The header (with `STASHES (N)`) is always
    /// rendered so the count is glanceable even when collapsed.
    collapsed: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    _refresh_task: Option<Task<()>>,
    _op_task: Option<Task<()>>,
}

impl EventEmitter<PushStashRequested> for StashPanel {}

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
            collapsed: true,
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

    /// Whether the body is currently hidden. Header stays rendered
    /// regardless so the count and `+` push affordance are always
    /// reachable.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// Flip the body visibility. Wired to the header's chevron click.
    pub fn toggle_collapsed(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        cx.notify();
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
            "Stash apply",
            cx,
        );
    }

    pub fn pop(&mut self, stash_ref: StashRef, cx: &mut Context<Self>) {
        self.spawn_op(
            move |repo| async move { repo.stash_pop(&stash_ref).await },
            "Stash pop",
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
            "Stash drop",
            cx,
        );
    }

    /// Fire-and-forget `git stash push` from outside the panel
    /// (the host's `PushStashDialog` confirm callback). Mirrors the
    /// existing `apply` / `pop` / `drop_pending` plumbing: shells out
    /// on tokio, refreshes on completion regardless of success so the
    /// user sees the current state. The push-result `StashRef` is
    /// dropped — the new entry will land at `stash@{0}` and surface
    /// via the refreshed list rendering.
    pub fn push(&mut self, msg: Option<String>, include_untracked: bool, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo
                        .stash_push(msg.as_deref(), include_untracked)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::stash_panel",
                    "no tokio runtime; stash_push skipped"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                if let Ok(Err(err)) = &result {
                    crate::shell::toast::toast_op_error(cx, "Stash push", err);
                }
                panel.refresh(cx);
            });
        });
        self._op_task = Some(task);
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
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                if let Ok(Err(err)) = &result {
                    crate::shell::toast::toast_op_error(cx, label, err);
                }
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
        let count = match &self.state {
            StashListState::Ready(entries) => entries.len(),
            _ => 0,
        };
        let header = self.render_header(count, cx);

        let mut container = div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .bg(self.theme.bg_panel)
            .child(header);

        if !self.collapsed {
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
                    placeholder("No stashes yet", self.theme, self.density, &self.typography)
                        .into_any_element()
                }
                StashListState::Ready(entries) => {
                    let mut col = div().flex().flex_col().w_full();
                    for entry in entries.iter().cloned() {
                        col = col.child(self.render_row(entry, cx));
                    }
                    col.into_any_element()
                }
            };
            container = container.child(body);
        }

        container
    }
}

impl StashPanel {
    /// Header: chevron toggle + "STASHES (N)" label + push button.
    /// Always rendered, even when the body is collapsed, so the count
    /// stays visible at a glance and the `+` action is always
    /// reachable. Chevron points down when open, right when collapsed
    /// (matches the SCM-section convention).
    fn render_header(&self, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::shell::source_control::style as sc_style;
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let collapsed = self.collapsed;
        let chevron = if collapsed {
            "icons/chevron-right.svg"
        } else {
            "icons/chevron-down.svg"
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(density.h_row))
            .px(px(density.pad_panel))
            .gap(px(density.gap_inline))
            .border_b_1()
            .border_color(theme.border_inactive)
            .text_size(px(typography.t_label_caps))
            .text_color(theme.fg_muted)
            .child(
                // Whole chevron+label area is clickable, mirroring
                // collapsible-section UX elsewhere in the panel.
                div()
                    .id("stash-header-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .flex_1()
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        panel.toggle_collapsed(cx);
                    }))
                    .child(
                        Icon::default()
                            .path(chevron)
                            .size(px(sc_style::ICON))
                            .text_color(theme.fg_muted),
                    )
                    .child(format!("STASHES ({count})")),
            )
            .child(
                Button::new("stash-push-new")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/plus.svg"))
                    .tooltip("Push new stash")
                    .on_click(cx.listener(|_panel, _: &ClickEvent, _window, cx| {
                        cx.emit(PushStashRequested);
                    })),
            )
    }

    fn render_row(&self, entry: StashEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let label = row_label(&entry);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let index = entry.stash_ref.index;
        let apply_ref = entry.stash_ref.clone();
        let pop_ref = entry.stash_ref.clone();
        let drop_ref = entry.stash_ref.clone();
        // Hover scope for the progressive-disclosure cluster below.
        let group_name = format!("stash-row-{index}");
        // Apply / Pop / Drop all sit at the same xsmall (22px) height so the
        // row reads as one action cluster — the destructive verb doesn't
        // dominate by being larger than its siblings.
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            // Pin the action cluster: it must never shrink or clip — a narrow
            // panel truncates the label instead (the canonical SCM-row collapse
            // priority). Without this, a long stash message pushed the cluster
            // off the right edge and clipped "Drop".
            .flex_shrink_0()
            .gap(px(density.gap_inline))
            // Progressive disclosure: the cluster rests ghosted and lifts to
            // full on row-hover, so a calm row at rest but every verb is one
            // hover away. NOT fully hidden on purpose — the stash panel has no
            // context menu, so a hidden cluster would leave Drop (destructive)
            // with no alternative invocation path. Ghost-at-rest keeps every
            // action reachable at all times (documented exception to the
            // fully-hidden row-action convention used where a context-menu
            // backup exists).
            .opacity(STASH_ACTION_REST_OPACITY)
            .group_hover(group_name.clone(), |s| s.opacity(1.0))
            .child(
                Button::new(("stash-apply", index))
                    .ghost()
                    .xsmall()
                    .label("Apply")
                    .tooltip("Apply stash (keep it in the list)")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.apply(apply_ref.clone(), cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("stash-pop", index))
                    .ghost()
                    .xsmall()
                    .label("Pop")
                    .tooltip("Apply stash and remove it (reversible via reflog)")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.pop(pop_ref.clone(), cx);
                        cx.notify();
                    })),
            )
            .child(danger_ghost(
                ("stash-drop", index),
                "Drop",
                &theme,
                &density,
                typography,
                cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                    panel.request_drop(drop_ref.clone());
                    cx.notify();
                }),
            ));
        div()
            .group(group_name)
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
                    // Shrink-to-fit + ellipsis so a long stash subject collapses
                    // gracefully instead of shoving the action cluster off-panel.
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_base)
                    .child(label),
            )
            .child(actions)
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
        .h(px(density.h_action_row))
        .p(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
