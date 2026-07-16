//! The turn-end "N files changed" card.
//!
//! An editing turn otherwise ends with only prose: the model says it changed
//! something and the user has to take its word, or expand the tool cards one by
//! one. This closes the turn with what actually landed on disk — file count and
//! per-file ±.
//!
//! The stats are computed at fold time ([`oximux_agents::thread::turn_diff`]);
//! this module only renders them. The Review affordance is wired but no caller
//! passes a handler yet: opening the turn's diff needs a viewer entry point that
//! takes an in-memory diff, and `DiffView` currently loads only from a git repo.

use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use oximux_agents::thread::TurnFileChange;
use oximux_settings::{Density, Theme, Typography};

/// Opens the turn's diff. Boxed so the caller can decide at runtime whether a
/// turn is reviewable at all.
type ReviewHandler = Box<dyn Fn(&ClickEvent, &mut gpui::Window, &mut App) + 'static>;

/// The card for one turn's changes.
///
/// `on_review` is `None` when the turn has no diff to open — either the backend
/// reported none (Claude/ACP/Pi, whose edit cards carry fragments rather than
/// line-numbered hunks), or the host has no viewer to open it in. The card still
/// reports what changed; it just doesn't offer a button that would open an empty
/// or invented diff.
pub(super) fn render(
    files: &[TurnFileChange],
    theme: Theme,
    density: Density,
    typo: &Typography,
    on_review: Option<ReviewHandler>,
) -> AnyElement {
    let (added, removed) = totals(files);
    let head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_muted)
                .child(SharedString::from(headline(files))),
        )
        .child(stat(format!("+{added}"), theme.status_ok, typo))
        .child(stat(format!("−{removed}"), theme.status_error, typo));

    let mut card = div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .w_full()
        .p(px(8.0))
        .rounded(px(density.r_chip))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel)
        .child(head);

    for f in files {
        card = card.child(file_row(f, theme, typo));
    }
    if let Some(review) = on_review {
        card = card.child(
            div()
                .id("turn-diff-review")
                .flex_none()
                .mt(px(3.0))
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(density.r_chip))
                .border_1()
                .border_color(theme.border_inactive)
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_base)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_base))
                .child(SharedString::from("Review"))
                .on_click(review),
        );
    }
    card.into_any_element()
}

/// `3 files changed` / `1 file changed`.
fn headline(files: &[TurnFileChange]) -> String {
    let n = files.len();
    format!("{n} file{} changed", if n == 1 { "" } else { "s" })
}

/// Turn totals across every file.
fn totals(files: &[TurnFileChange]) -> (usize, usize) {
    files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
}

/// One file's row: its path, then its own ±. The path column is `min_w_0` so a
/// deep path elides inside the card rather than widening it.
fn file_row(f: &TurnFileChange, theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .w_full()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(super::bubble::elide(&f.path, 90))),
        )
        .child(stat(format!("+{}", f.added), theme.status_ok, typo))
        .child(stat(format!("−{}", f.removed), theme.status_error, typo))
        .into_any_element()
}

fn stat(text: String, color: gpui::Hsla, typo: &Typography) -> AnyElement {
    div()
        .flex_none()
        .text_size(px(typo.t_label_xs))
        .text_color(color)
        .child(SharedString::from(text))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, added: usize, removed: usize) -> TurnFileChange {
        TurnFileChange { path: path.into(), added, removed }
    }

    #[test]
    fn headline_counts_files_and_reads_singular() {
        assert_eq!(headline(&[f("a", 1, 0)]), "1 file changed");
        assert_eq!(headline(&[f("a", 1, 0), f("b", 0, 1)]), "2 files changed");
    }

    #[test]
    fn totals_sum_every_files_own_stats() {
        assert_eq!(totals(&[f("a", 3, 1), f("b", 4, 2)]), (7, 3));
        assert_eq!(totals(&[]), (0, 0));
    }
}
