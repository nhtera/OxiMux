//! StashPanel — list git stash entries with per-row Apply / Pop / Drop.
//!
//! Drop is destructive and irreversible in the UI, so the panel never fires
//! it: the row emits [`DropStashRequested`], the shell host mounts a
//! `ConfirmDialog`, and only its confirm callback reaches
//! [`StashPanel::drop_confirmed`]. An event, not a flag — a flag has no
//! subscriber, which is precisely why Drop shipped doing nothing.
//!
//! Apply and Pop fire directly. **Neither is undone by the reflog**: after a
//! pop the stash's reflog entry is gone and the only way back is the commit
//! sha, which is why every destructive path here logs one before it fires.
//!
//! Layout:
//!   - Always-rendered header: chevron + "STASHES (N)" + refresh + "+" push.
//!   - Body: list (or "No stashes" placeholder). Hidden when collapsed
//!     (default). Power-user surface; eats no visual real estate when
//!     unused.
//!
//! Runtime: refresh + ops use `tokio::runtime::Handle::try_current` + the
//! same log+no-op fallback as DiffView / CommitDialog. Refresh is
//! single-flight via `_refresh_task: Option<Task<()>>` — dropping cancels.
//! Ops are detached instead (see `ops.rs`): cancelling a destructive op
//! mid-subprocess loses its result.

pub mod list_render;
pub mod ops;
pub mod push_dialog;

use crate::shell::stash_panel::list_render::row_label;
use crate::ui::danger_ghost;
use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Task,
    Window, div, px,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants},
    scroll::ScrollableElement as _,
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

/// How many stash rows the expanded body shows before it starts scrolling.
///
/// The section is `flex_shrink_0`, so without a cap every stash row it
/// renders comes straight out of the changed-files block above — five rows
/// (~170px) is what tips a 13" display into clipping the CHANGES header.
/// The cap makes the section's appetite bounded and hands the overflow to
/// its own scroll region instead of to its neighbour.
///
/// Deliberately a private const with no test: eight is an admitted guess,
/// and Phase 5 deletes it when the section becomes drag-resizable. Promoting
/// a soon-to-be-deleted guess into public, unit-tested settings API would be
/// churn. If it chafes before Phase 5 lands, this is one line to raise.
const STASH_BODY_MAX_ROWS: f32 = 8.0;

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

/// Emitted when the user clicks a row's `Drop`. The host mounts a
/// `ConfirmDialog` and, on confirm, calls [`StashPanel::drop_confirmed`].
///
/// Carries everything the dialog copy needs, because an index tells the user
/// nothing about which stash is about to disappear.
#[derive(Debug, Clone)]
pub struct DropStashRequested {
    /// The `stash@{N}` address the row was PAINTED with — for display and for
    /// the context menu, never for the git call. The op re-resolves `sha` to a
    /// live address at fire time; see `ops.rs`.
    pub stash_ref: StashRef,
    /// The stash's immutable identity, and what the op actually acts on.
    pub sha: String,
    pub message: String,
    pub relative: String,
    pub branch: String,
}

pub struct StashPanel {
    repo: Repository,
    state: StashListState,
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
    /// Scroll position for the capped stash body. Wired through
    /// `track_scroll` on the overflow region and consumed by
    /// `vertical_scrollbar` so the thumb tracks the user's wheel/drag.
    /// Mirrors `GitPanel::scroll_handle` (`git_panel/mod.rs:113`).
    scroll_handle: ScrollHandle,
    /// Serialises the resolve→fire window of every stash op.
    ///
    /// Ops are detached so they cannot cancel each other, which leaves them
    /// free to overlap: two confirms landing within one subprocess (~25 ms)
    /// both resolve against the pre-mutation stack, so the second fires on an
    /// index the first has already invalidated. `stash_drop`'s sha assertion
    /// catches that and `rollback` undoes it, but a rollback is a loud error
    /// and a reordered stack — the wrong outcome for a legitimate pair of
    /// clicks. Holding this across resolve-and-fire makes the race
    /// unreachable, and leaves the assertion as the last resort it was meant
    /// to be rather than the expected path.
    op_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    _refresh_task: Option<Task<()>>,
}

impl EventEmitter<PushStashRequested> for StashPanel {}
impl EventEmitter<DropStashRequested> for StashPanel {}

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
            collapsed: true,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            scroll_handle: ScrollHandle::new(),
            op_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            _refresh_task: None,
        };
        panel.refresh(cx);
        panel
    }

    pub fn state(&self) -> &StashListState {
        &self.state
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

    /// Re-read the stash list, honouring `stash_list`'s 15 s read-TTL.
    ///
    /// `false` is right for the render path: the TTL only ever leaves us
    /// stale against an *external* writer, and self-heals within 15 s of the
    /// user actually looking at the section.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_inner(false, cx);
    }

    /// Re-read ignoring the TTL. For the header button (the user is asking
    /// precisely because they suspect the list is stale) and for the tail of
    /// our own ops, which must never sit on a change they just made.
    pub fn force_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_inner(true, cx);
    }

    fn refresh_inner(&mut self, force: bool, cx: &mut Context<Self>) {
        // Keep showing the list we have while the new one loads. Every op now
        // ends in a refresh, so blanking to "Loading stashes…" each time both
        // flashes the section and — load-bearing — empties the sha→message
        // snapshot `drop_confirmed` takes for its rollback, which would leave
        // a wrongly-dropped stash restored under a synthesised label with its
        // real message gone for good.
        if !matches!(self.state, StashListState::Ready(_)) {
            self.state = StashListState::Loading;
        }
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<StashEntry>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.stash_list(force).await.map_err(|e| e.to_string());
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
}

impl Focusable for StashPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StashPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
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
            // Cap the body and give the overflow its own scroll region.
            //
            // This section is `flex_shrink_0` and so is the graph below it,
            // which leaves the changed-files block as the SCM column's only
            // flexible child — it absorbs 100% of any height deficit. An
            // uncapped stash list therefore does not push itself off-screen,
            // it squeezes CHANGES until the header is guillotined. Bounding
            // the section's appetite is half the fix; the floor on the file
            // block (`files_floor`) is the other half.
            //
            // `.id()` is load-bearing: `overflow_y_scroll` without a stateful
            // id silently does nothing (the GPUI trap documented at
            // `git_panel/mod.rs:527`). `relative()` anchors the scrollbar
            // overlay, and `track_scroll` + `vertical_scrollbar` share
            // `scroll_handle` so the thumb mirrors the scroll position.
            let max_h = STASH_BODY_MAX_ROWS * self.density.h_action_row;
            container = container.child(
                div()
                    .id("stash-panel-scroll")
                    .relative()
                    .w_full()
                    .max_h(px(max_h))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(body)
                    .vertical_scrollbar(&self.scroll_handle),
            );
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
        use crate::shell::source_control::style::ScmStyle;
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let style = ScmStyle::new(density, typography);
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
                            .size(px(style.icon))
                            .text_color(theme.fg_muted),
                    )
                    .child(format!("STASHES ({count})")),
            )
            .child(
                Button::new("stash-refresh")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/refresh-cw.svg"))
                    .tooltip("Re-read the stash list")
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        // Forced: the user is clicking this precisely because
                        // they think the cached list is behind git.
                        panel.force_refresh(cx);
                    })),
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
        // Ops are keyed by sha, not by the index this row is painted with:
        // the stack is shared with every worktree and with the user's
        // terminal, so `index` can address a different stash by the time a
        // click lands. See `ops.rs`.
        let apply_sha = entry.sha.clone();
        let pop_sha = entry.sha.clone();
        let drop_entry = entry.clone();
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
                        panel.apply(apply_sha.clone(), cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("stash-pop", index))
                    .ghost()
                    .xsmall()
                    .label("Pop")
                    // NOT "reversible via reflog" — a pop deletes the stash's
                    // reflog entry, leaving the commit sha as the only way back.
                    .tooltip("Apply stash and remove it from the list")
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                        panel.pop(pop_sha.clone(), cx);
                        cx.notify();
                    })),
            )
            .child(danger_ghost(
                ("stash-drop", index),
                "Drop",
                &theme,
                &density,
                typography,
                cx.listener(move |_panel, _: &ClickEvent, _window, cx| {
                    // The panel never drops on its own — the host owns the
                    // confirm step and calls back into `drop_confirmed`.
                    cx.emit(DropStashRequested {
                        stash_ref: drop_entry.stash_ref.clone(),
                        sha: drop_entry.sha.clone(),
                        message: drop_entry.message.clone(),
                        relative: drop_entry.relative.clone(),
                        branch: drop_entry.branch.clone(),
                    });
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
