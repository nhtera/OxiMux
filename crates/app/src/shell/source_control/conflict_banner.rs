//! Amber-bordered status cards rendered above the file list when the
//! worktree is in an unhappy state:
//!
//! - [`render_conflict_summary_card`] — "N unresolved conflicts" with
//!   an "Open all in editor" button. Shown when
//!   `unresolved_conflict_count > 0`.
//! - [`render_operation_banner`] — "Merge in progress" /
//!   "Rebase in progress" / etc. Shown when
//!   `Repository::current_operation()` is `Some(...)`.
//!
//! Both stack inside the panel render between the filter row and the
//! file list. The card always sits above the banner when both apply
//! — the more granular "files in conflict" surface dominates the
//! broader "operation pending" surface so the user's eye lands on
//! what they can act on first.
//!
//! Pure render functions (no entity, no state). The panel reads the
//! conflict count + current operation from its poll snapshot, decides
//! whether to mount each card per render, and hands the values in.
//! Click wiring for "Open all in editor" is plumbed by the caller —
//! the button takes an `on_open_all` closure rather than reaching for
//! a panel-side method, keeping the banner module free of
//! GPUI-entity coupling.

use gpui::{ClickEvent, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::{
    Disableable, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_core::GitOperation;
use oximux_settings::Theme;

use crate::shell::source_control::style as sc_style;

/// Plural-aware copy for the conflict-summary card. Pure for
/// unit-test coverage.
pub fn conflict_summary_text(count: usize) -> String {
    if count == 1 {
        "1 unresolved conflict".to_string()
    } else {
        format!("{count} unresolved conflicts")
    }
}

/// Render the amber "{N} unresolved conflicts" card with a button
/// that dispatches `on_open_all` when `enabled` is true. The closure
/// takes `(&mut Window, &mut App)` — same shape as `OnOpenFile` —
/// so the caller can iterate `list_conflicting_paths()` and fire the
/// host's open-file callback for each.
///
/// `enabled = false` renders the button in its disabled state with
/// an "unavailable in this context" tooltip; callers without the
/// host file-open callback wired (e.g. integration tests) pass
/// `false` so the button reflects the actual capability rather
/// than silently no-op'ing.
///
/// Returns `None` when `count == 0` so the caller can
/// `.children(iter)` the result without an extra outer `if`.
pub fn render_conflict_summary_card<F>(
    count: usize,
    theme: Theme,
    enabled: bool,
    on_open_all: F,
) -> Option<impl IntoElement>
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    if count == 0 {
        return None;
    }
    let label = conflict_summary_text(count);
    let button_tooltip = if enabled {
        "Open every conflicting file in the editor"
    } else {
        "Open-file action unavailable in this context"
    };
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .w_full()
            .px(px(sc_style::PAD_H))
            .py(px(sc_style::PAD_V))
            .border_b_1()
            .border_color(theme.status_warning)
            .text_size(px(sc_style::BODY_TEXT))
            .text_color(theme.fg_base)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(sc_style::PAD_V))
                    .child(
                        Icon::default()
                            .path("icons/alert-triangle.svg")
                            .size(px(sc_style::ICON))
                            .text_color(theme.status_warning),
                    )
                    .child(label),
            )
            .child(
                Button::new("sc-conflict-open-all")
                    .ghost()
                    .xsmall()
                    .label("Open all in editor")
                    .tooltip(button_tooltip)
                    .disabled(!enabled)
                    .on_click(move |_: &ClickEvent, window, cx| {
                        on_open_all(window, cx);
                    }),
            ),
    )
}

/// Render the amber "X in progress" banner for an in-flight git
/// operation. No buttons — Phase 08 only surfaces the state; resolve
/// / abort affordances land later. Returns `None` when `op` is
/// `None` so the caller can `.children(iter)` the result.
pub fn render_operation_banner(
    op: Option<GitOperation>,
    theme: Theme,
) -> Option<impl IntoElement> {
    let op = op?;
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .w_full()
            .px(px(sc_style::PAD_H))
            .py(px(sc_style::PAD_V))
            .border_b_1()
            .border_color(theme.status_warning)
            .text_size(px(sc_style::BODY_TEXT))
            .text_color(theme.fg_base)
            .gap(px(sc_style::PAD_V))
            .child(
                Icon::default()
                    .path("icons/git-merge.svg")
                    .size(px(sc_style::ICON))
                    .text_color(theme.status_warning),
            )
            .child(op.banner_label()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_summary_singular_zero() {
        // count == 0 is the "no banner" case at the render layer, but
        // the text helper should still return a stable string if
        // anyone calls it defensively (e.g. for an accessibility
        // label that's computed eagerly).
        assert_eq!(conflict_summary_text(0), "0 unresolved conflicts");
    }

    #[test]
    fn conflict_summary_singular_one() {
        assert_eq!(conflict_summary_text(1), "1 unresolved conflict");
    }

    #[test]
    fn conflict_summary_plural_two() {
        assert_eq!(conflict_summary_text(2), "2 unresolved conflicts");
    }

    #[test]
    fn conflict_summary_plural_many() {
        assert_eq!(conflict_summary_text(47), "47 unresolved conflicts");
    }

    #[test]
    fn operation_banner_labels_each_op_kind() {
        // Mirrors oximux_core::GitOperation::banner_label — the UI
        // surfacing of those strings is THIS module's responsibility,
        // so locking them here catches drift between the core enum
        // and what users actually see.
        assert_eq!(GitOperation::Merge.banner_label(), "Merge in progress");
        assert_eq!(GitOperation::Rebase.banner_label(), "Rebase in progress");
        assert_eq!(
            GitOperation::CherryPick.banner_label(),
            "Cherry-pick in progress"
        );
        assert_eq!(GitOperation::Revert.banner_label(), "Revert in progress");
        assert_eq!(GitOperation::Bisect.banner_label(), "Bisect in progress");
    }
}
