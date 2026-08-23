//! The worktree pill's popover body, lifted out of `composer`.
//!
//! A pure assembler: every input arrives as a parameter, so it needs nothing
//! from the view beyond the entity it emits events back into. It lives here
//! rather than in `composer.rs` because that file is well over the file-size
//! hard cap, and this is the largest piece of it that carries no state.

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use gpui_component::popover::PopoverState;
use oximux_settings::{Density, Theme, Typography};

use super::composer::{ComposerEvent, ComposerView, WorktreeDraft};

/// The worktree pill's popover body: two isolation rows, then the slug field
/// while a worktree is armed. A plain panel rather than a `PopupMenu` because
/// menu rows can't host a text input — see [`ComposerView::render_worktree_picker`].
pub(super) fn worktree_popover_panel(
    draft: WorktreeDraft,
    view: Entity<ComposerView>,
    theme: Theme,
    typo: &Typography,
    density: Density,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .w(px(260.0))
        .p(px(4.0))
        .gap(px(2.0));

    for (enabled, label, detail) in [
        (false, "This project", "Run in the project directory"),
        (true, "New worktree", "Run in an isolated branch"),
    ] {
        let selected = draft.enabled == enabled;
        let view = view.clone();
        panel = panel.child(
            div()
                .id(SharedString::from(if enabled { "wt-row-new" } else { "wt-row-local" }))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(density.r_xs))
                .hover(|s| s.bg(theme.hover_overlay))
                .cursor_pointer()
                .on_click(move |_ev, _window, cx| {
                    view.update(cx, |_v, cx| {
                        // Emit the DESIRED state; the parent no-ops on a re-pick.
                        cx.emit(ComposerEvent::WorktreeIsolationPicked(enabled));
                    });
                })
                .child(
                    div()
                        .w(px(12.0))
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_base)
                        .child(if selected { "\u{2713}" } else { " " }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .text_size(px(typo.t_body_sm))
                                .text_color(theme.fg_base)
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(typo.t_body_sm))
                                .text_color(theme.fg_subtle)
                                .child(detail),
                        ),
                ),
        );
    }

    if draft.enabled && let Some(input) = draft.slug_input.clone() {
        panel = panel
            .child(
                div()
                    .my(px(4.0))
                    .h(px(1.0))
                    .w_full()
                    .bg(theme.border_inactive),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .px(px(8.0))
                    .pb(px(4.0))
                    .child(
                        div()
                            .text_size(px(typo.t_body_sm))
                            .text_color(theme.fg_subtle)
                            .child("Branch"),
                    )
                    .child(Input::new(&input).small())
                    .child(
                        div()
                            .text_size(px(typo.t_body_sm))
                            .text_color(if draft.hint_is_error {
                                theme.status_error
                            } else {
                                theme.fg_subtle
                            })
                            .child(draft.hint.clone()),
                    ),
            );
    }

    let _ = cx;
    panel.into_any_element()
}
