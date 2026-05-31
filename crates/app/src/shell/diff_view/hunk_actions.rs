//! Per-hunk action chips rendered into the diff hunk header.
//!
//! Three text chips — Stage / Unstage / Discard — that dispatch back to
//! `DiffView::stage_hunk` / `unstage_hunk` / `request_discard_hunk`
//! against the (file_idx, hunk_idx) pair the chip captured when it was
//! built. Indices are positional inside the current `Ready` snapshot;
//! `DiffView::load` re-runs after each op and the renderer rebuilds
//! the chips against the fresh hunk list, so indices stay valid.
//!
//! Visibility gate (decided here, not in the renderer):
//! - Untracked file → no chips. Untracked content has no index hunk
//!   path; the user stages the whole file from the SCM panel.
//! - Staged side on screen → Unstage only. Stage / Discard are
//!   worktree-side ops; the unstaged diff is where they belong.
//! - Unstaged side on screen → Stage + Discard.
//!
//! Returns `None` when no chip would render — caller skips the slot
//! entirely so the hunk-header row stays narrow on read-only views.

use crate::shell::diff_view::{DiffView, HunkActionSide};
use gpui::{
    ClickEvent, Context, Hsla, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, div, px,
};
use oximux_settings::{Density, Theme, Typography};

/// Render the action-chip cluster for one hunk. `None` when the
/// current side carries no actions (untracked file, or no visible chip
/// applies to the current snapshot).
pub fn render_hunk_actions(
    side: HunkActionSide,
    file_idx: usize,
    hunk_idx: usize,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<DiffView>,
) -> Option<gpui::Div> {
    if side.untracked {
        return None;
    }
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .pr(px(density.pad_panel));
    if side.staged {
        row = row.child(action_chip(
            "unstage",
            "Unstage",
            file_idx,
            hunk_idx,
            HunkAction::Unstage,
            theme,
            density,
            typography,
            cx,
        ));
    } else {
        row = row
            .child(action_chip(
                "stage",
                "Stage",
                file_idx,
                hunk_idx,
                HunkAction::Stage,
                theme,
            density,
            typography,
            cx,
            ))
            .child(action_chip(
                "discard",
                "Discard",
                file_idx,
                hunk_idx,
                HunkAction::Discard,
                theme,
            density,
            typography,
            cx,
            ));
    }
    Some(row)
}

/// Which dispatch a chip targets. Discard takes a distinct color tint
/// — it's destructive and the user should read it at a glance against
/// the neutral Stage/Unstage chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkAction {
    Stage,
    Unstage,
    Discard,
}

#[allow(clippy::too_many_arguments)]
fn action_chip(
    id_seed: &'static str,
    label: &'static str,
    file_idx: usize,
    hunk_idx: usize,
    action: HunkAction,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<DiffView>,
) -> gpui::Stateful<gpui::Div> {
    // Per-hunk unique id keeps GPUI's interactive-element bookkeeping
    // from collapsing chips that share a label across hunks.
    let id =
        gpui::ElementId::Name(format!("hunk-action-{id_seed}-{file_idx}-{hunk_idx}").into());
    let (fg, hover_bg) = chip_palette(action, &theme);
    let label: SharedString = label.into();
    div()
        .id(id)
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_chip))
        .text_size(px(typography.t_body_sm))
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .child(label)
        .on_click(cx.listener(
            move |view, _: &ClickEvent, window, cx| match action {
                HunkAction::Stage => {
                    view.stage_hunk(file_idx, hunk_idx, cx);
                    cx.notify();
                }
                HunkAction::Unstage => {
                    view.unstage_hunk(file_idx, hunk_idx, cx);
                    cx.notify();
                }
                HunkAction::Discard => {
                    view.request_discard_hunk(file_idx, hunk_idx, window, cx);
                    cx.notify();
                }
            },
        ))
}

/// Map action → (foreground color, hover background). Stage/Unstage
/// read as neutral metadata (info blue); Discard pops red so the
/// destructive affordance never blends in with the constructive ones.
fn chip_palette(action: HunkAction, theme: &Theme) -> (Hsla, Hsla) {
    let neutral_fg = theme.status_info;
    let danger_fg = theme.status_error;
    let hover_bg = Hsla {
        a: 0.18,
        ..theme.bg_panel_alt
    };
    let danger_hover = Hsla {
        a: 0.25,
        ..theme.status_error
    };
    match action {
        HunkAction::Stage | HunkAction::Unstage => (neutral_fg, hover_bg),
        HunkAction::Discard => (danger_fg, danger_hover),
    }
}
