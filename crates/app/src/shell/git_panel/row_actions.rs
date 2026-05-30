//! Per-row hover actions for the changed-files list.
//!
//! Each row exposes a small cluster of icon buttons (stage / unstage /
//! discard) on the right that fade in when the row is hovered. The cluster
//! occupies the same vertical slot as the status badge — on hover the
//! badge fades out and the actions fade in via `group_hover`, so the row
//! layout never shifts.
//!
//! Buttons are split out here (rather than inlined in `changed_files::row`)
//! so `changed_files.rs` stays under the 500-LOC warn cap and so the
//! click-handler shapes are independently testable.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::changed_files::RowKindForActions;
use gpui::{AnyElement, ClickEvent, Context, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{
    Disableable, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use std::path::PathBuf;

/// Width of one action button. Two buttons → 36px slot.
pub const ACTION_BTN_W: f32 = 16.0;

/// Build the action cluster that replaces the status badge on hover.
/// Returns `AnyElement` (not `impl IntoElement`) so the returned value
/// doesn't carry `cx`'s lifetime under Rust-2024 impl-trait capture rules —
/// the caller can hold the cluster across other `cx`-borrowing calls.
///
/// `row_id` must match the parent row's `.group()` id — that's the hover
/// scope `group_hover` reads. `is_discarding` is `true` when the row's
/// path is sitting in `GitPanel::in_flight_discards`; the discard
/// button renders a spinner and ignores clicks until the op completes.
pub fn action_cluster(
    path: PathBuf,
    kind: RowKindForActions,
    row_id: gpui::SharedString,
    is_discarding: bool,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    let stage_id = format!("git-row-stage-{row_id}");
    let unstage_id = format!("git-row-unstage-{row_id}");
    let discard_id = format!("git-row-discard-{row_id}");

    let mut cluster = div().flex().flex_row().items_center().gap(px(2.0));

    match kind {
        RowKindForActions::Staged => {
            cluster = cluster.child(unstage_button(unstage_id, path.clone(), cx));
        }
        RowKindForActions::Unstaged => {
            // Destructive (discard) on the left, additive (stage) on the
            // right. Reading order pushes the safer action toward the
            // pointer's resting position so the eye lands there last.
            cluster = cluster
                .child(discard_button(discard_id, path.clone(), is_discarding, cx))
                .child(stage_button(stage_id, path.clone(), cx));
        }
        RowKindForActions::Untracked => {
            cluster = cluster.child(stage_button(stage_id, path.clone(), cx));
        }
    }
    cluster.into_any_element()
}

fn stage_button(id: String, path: PathBuf, cx: &mut Context<GitPanel>) -> AnyElement {
    Button::new(gpui::SharedString::from(id))
        .ghost()
        .xsmall()
        .icon(
            Icon::default()
                .path("icons/plus.svg")
                .size(px(ACTION_BTN_W)),
        )
        .tooltip("Stage file")
        .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
            panel.stage_path(path.clone(), cx);
        }))
        .into_any_element()
}

fn unstage_button(id: String, path: PathBuf, cx: &mut Context<GitPanel>) -> AnyElement {
    Button::new(gpui::SharedString::from(id))
        .ghost()
        .xsmall()
        .icon(
            Icon::default()
                .path("icons/minus.svg")
                .size(px(ACTION_BTN_W)),
        )
        .tooltip("Unstage file")
        .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
            panel.unstage_path(path.clone(), cx);
        }))
        .into_any_element()
}

fn discard_button(
    id: String,
    path: PathBuf,
    is_discarding: bool,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    // Destructive action — clicking emits a `DiscardRequested` event;
    // the shell mounts a type-to-confirm modal before the worktree
    // mutation actually runs (see `WorkspaceRoot::mount_discard_dialog`).
    //
    // `undo.svg` ships in the gpui-component asset bundle (Lucide
    // `rotate-ccw` is not bundled). Both glyphs read as "revert /
    // restore". `rotate.svg` plays the spinner role while the op is in
    // flight — the icon swap is enough visual feedback; the row layout
    // never shifts.
    let icon_path = if is_discarding {
        "icons/loader-circle.svg"
    } else {
        "icons/undo.svg"
    };
    let tooltip = if is_discarding {
        "Discarding…"
    } else {
        "Discard changes"
    };
    let mut btn = Button::new(gpui::SharedString::from(id))
        .ghost()
        .xsmall()
        .icon(Icon::default().path(icon_path).size(px(ACTION_BTN_W)))
        .tooltip(tooltip);
    if is_discarding {
        // Swallow clicks while the op is in flight — re-clicking would
        // queue a second confirm modal for the same path.
        btn = btn.disabled(true);
    } else {
        btn = btn.on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
            panel.discard_path(path.clone(), cx);
        }));
    }
    btn.into_any_element()
}
