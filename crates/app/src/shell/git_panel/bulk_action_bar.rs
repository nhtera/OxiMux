//! Floating bulk-action bar for the multi-select SCM panel.
//!
//! Renders below the changed-files scroll body when at least one row
//! is selected. The bar floats over the bottom of the panel (absolute
//! positioning); the scroll body gets a 6px bottom padding so the last
//! row stays visible behind it.
//!
//! Layout: `N selected · + Stage (N_to_stage) · − Unstage (N_to_unstage) · ×`
//!
//! Stage / Unstage buttons hide when their count is 0 (no-op operations
//! shouldn't clutter the bar). While a bulk op is in flight, the count
//! swaps for a spinner icon and the action buttons disable so a
//! re-click can't queue a second op against stale state.

use crate::shell::git_panel::GitPanel;
use crate::shell::source_control::style as sc_style;
use gpui::{
    AnyElement, ClickEvent, Context, Hsla, IntoElement, ParentElement, Styled, div, px,
};
use gpui_component::{
    Disableable, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_settings::{Density, Theme, Typography};

/// Pixel size of the action-button icons. Matches `row_actions::ACTION_BTN_W`
/// so the bar feels consistent with the per-row hover cluster.
const ACTION_ICON_PX: f32 = 14.0;

/// Compute the (`to_stage`, `to_unstage`) counts from the selected
/// path set against the current poll snapshot. A partial-stage path
/// contributes to BOTH counts — `stage_paths` on it stages the rest,
/// `unstage_paths` unstages the staged side, so each op makes
/// independent sense.
///
/// Conflict rows (`IndexStatus::Unmerged` or `WorktreeStatus::Unmerged`)
/// also contribute to BOTH counts because `is_staged()` and
/// `is_unstaged()` both report `true` for them. Clicking Stage on a
/// conflict row tells git to mark the conflict resolved with the
/// worktree version — that may surprise users who expected a manual
/// merge first. Phase 08 (`ConflictSummaryCard`) will gate this; for
/// Slice B the bar reflects what the backend will actually do.
///
/// Paths in `selected` that are absent from `git_state` (e.g. the
/// poller hasn't caught up to an externally-modified worktree) are
/// silently ignored — the bar counts reflect what the UI actually
/// knows about, not stale selection ghosts.
fn classify_selection(panel: &GitPanel) -> (usize, usize) {
    let Some(state) = panel.git_state.as_ref() else {
        return (0, 0);
    };
    let mut to_stage = 0usize;
    let mut to_unstage = 0usize;
    for path in panel.selected.iter() {
        let Some(file) = state.files.iter().find(|f| &f.path == path) else {
            continue;
        };
        if file.is_unstaged() {
            to_stage += 1;
        }
        if file.is_staged() {
            to_unstage += 1;
        }
    }
    (to_stage, to_unstage)
}

/// Build the floating bar. Returns `None` when nothing is selected so
/// the caller can skip the slot entirely.
///
/// `cx` is needed so the buttons' click handlers can call the
/// bulk-op methods on the parent `GitPanel` via `cx.listener`.
pub fn render_bulk_action_bar(
    panel: &GitPanel,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<GitPanel>,
) -> Option<AnyElement> {
    let n_selected = panel.selected.len();
    if n_selected == 0 {
        return None;
    }
    let in_flight = panel.bulk_op_in_flight;
    let (to_stage, to_unstage) = classify_selection(panel);

    // Count slot: spinner when an op is running, otherwise the
    // "N selected" tally.
    let count_slot: AnyElement = if in_flight {
        Icon::default()
            .path("icons/loader-circle.svg")
            .size(px(ACTION_ICON_PX))
            .text_color(theme.fg_muted)
            .into_any_element()
    } else {
        div()
            .text_color(theme.fg_base)
            .font_weight(typography.w_medium)
            .child(format!("{n_selected} selected"))
            .into_any_element()
    };

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .px(px(sc_style::PAD_H))
        .py(px(4.0))
        .text_size(px(sc_style::TEXT))
        .child(count_slot);

    // Stage / Unstage buttons: omit when their count is 0 (the op
    // would be a no-op on an empty subset). On in-flight, pass
    // `None` for the click handler — `action_button` swaps that for
    // `.disabled(true)`. Guarding listener construction here means
    // GPUI doesn't allocate a closure every spinner-state render
    // frame just to drop it inside `action_button`.
    if to_stage > 0 {
        let on_click = (!in_flight).then(|| {
            cx.listener(|panel, _: &ClickEvent, _window, cx| {
                panel.bulk_stage_selected(cx);
            })
        });
        row = row.child(action_button(
            "bulk-stage",
            "icons/plus.svg",
            theme.status_added,
            format!("Stage ({to_stage})"),
            "Stage selected files",
            on_click,
        ));
    }
    if to_unstage > 0 {
        let on_click = (!in_flight).then(|| {
            cx.listener(|panel, _: &ClickEvent, _window, cx| {
                panel.bulk_unstage_selected(cx);
            })
        });
        row = row.child(action_button(
            "bulk-unstage",
            "icons/minus.svg",
            theme.status_removed,
            format!("Unstage ({to_unstage})"),
            "Unstage selected files",
            on_click,
        ));
    }

    // Clear lives at the far right via `ml_auto`. Always enabled —
    // even mid-op, dismissing the bar shouldn't be locked because the
    // selection itself is the artefact the user wants gone.
    row = row.child(
        div().ml_auto().child(
            Button::new("bulk-clear")
                .ghost()
                .xsmall()
                .icon(Icon::default().path("icons/x.svg").size(px(ACTION_ICON_PX)))
                .tooltip("Clear selection")
                .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                    panel.clear_selection(cx);
                })),
        ),
    );

    // Outer bar container: absolute-positioned at the bottom of the
    // panel so the file list keeps its full scroll height. `bg_panel`
    // matches the panel background; a top border separates it from
    // the rows above.
    Some(
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .bg(theme.bg_panel)
            .border_t_1()
            .border_color(theme.border_inactive)
            .child(row)
            .into_any_element(),
    )
}

/// Render one action button. `on_click = None` puts the button in the
/// `.disabled(true)` state — passing `None` instead of a guarded
/// closure keeps GPUI from allocating a listener every render frame
/// while the spinner is up.
fn action_button<F>(
    id: &'static str,
    icon_path: &'static str,
    icon_color: Hsla,
    label: String,
    tooltip: &'static str,
    on_click: Option<F>,
) -> AnyElement
where
    F: Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let mut btn = Button::new(id)
        .ghost()
        .xsmall()
        .icon(
            Icon::default()
                .path(icon_path)
                .size(px(ACTION_ICON_PX))
                .text_color(icon_color),
        )
        .label(label)
        .tooltip(tooltip);
    match on_click {
        Some(handler) => btn = btn.on_click(handler),
        None => btn = btn.disabled(true),
    }
    btn.into_any_element()
}
