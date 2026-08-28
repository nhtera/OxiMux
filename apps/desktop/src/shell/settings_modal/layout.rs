//! Body layout primitives for the settings panes: a bordered "section card"
//! that groups rows with hairline dividers, and a described setting row
//! (label + helper text on the left, the control pinned right). These give
//! the panes the grouped, descriptive look of a native preferences window
//! without touching control behavior.

use gpui::{
    AnyElement, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder, px,
};
use oximux_settings::{Density, Theme, Typography};

/// Stack `rows` full-width, separating each from the next with a hairline
/// divider (the last row gets none). No surrounding border or fill — the airy,
/// borderless grouped-list look of a native preferences pane.
pub(super) fn section_card(
    theme: Theme,
    _density: Density,
    rows: Vec<AnyElement>,
) -> AnyElement {
    let last = rows.len().saturating_sub(1);
    let mut group = div().flex().flex_col().w_full();

    for (idx, row) in rows.into_iter().enumerate() {
        group = group.child(row);
        if idx != last {
            group = group.child(div().w_full().h(px(1.0)).bg(theme.border_inactive));
        }
    }
    group.into_any_element()
}

/// Wrap a section's content in a raised "card": a slightly recessed fill, a
/// hairline border, rounded corners, and a 1px top edge-highlight that catches
/// light to suggest elevation. This is the grouped-card look of a polished
/// preferences window — each cluster of rows reads as its own panel instead of
/// floating on the flat body. The horizontal padding insets the rows (and their
/// dividers) off the rounded corners.
pub(super) fn card_surface(theme: Theme, density: Density, content: AnyElement) -> AnyElement {
    div()
        .relative()
        .w_full()
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(density.r_card))
        .px(px(14.0))
        .py(px(2.0))
        .child(
            div()
                .absolute()
                .top_0()
                .left(px(density.r_card))
                .right(px(density.r_card))
                .h(px(1.0))
                .bg(theme.edge_highlight),
        )
        .child(content)
        .into_any_element()
}

/// A section heading above a [`card_surface`]: a bold title with a muted
/// one-line description beneath. Gives each card a labelled, scannable header
/// the way a native preferences pane groups its settings.
pub(super) fn section_title(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let subtitle = subtitle.into();
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(typography.t_body_md))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child(title.into()),
        )
        .when(!subtitle.is_empty(), |c| {
            c.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(subtitle),
            )
        })
        .into_any_element()
}

/// A searchable setting: its label + description (matched against the filter
/// query) and the already-built control element.
pub(super) struct SettingEntry {
    pub label: SharedString,
    pub description: SharedString,
    pub control: AnyElement,
}

/// Build a [`SettingEntry`] from a label, description, and control element.
pub(super) fn entry(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
) -> SettingEntry {
    SettingEntry {
        label: label.into(),
        description: description.into(),
        control: control.into_any_element(),
    }
}

/// Case-insensitive substring match of `query` against a row's label or
/// description. An empty query matches everything.
fn query_matches(query: &str, label: &str, description: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    label.to_lowercase().contains(&q) || description.to_lowercase().contains(&q)
}

/// Case-insensitive match of `query` against a setting entry (label or
/// description). An empty query matches everything.
pub(super) fn entry_matches(query: &str, e: &SettingEntry) -> bool {
    query_matches(query, &e.label, &e.description)
}

/// Render `entries` as a borderless, hairline-divided group of described rows.
pub(super) fn entries_card(
    theme: Theme,
    density: Density,
    typography: &Typography,
    entries: Vec<SettingEntry>,
) -> AnyElement {
    let rows: Vec<AnyElement> = entries
        .into_iter()
        .map(|e| setting_row_desc(e.label, e.description, e.control, theme, typography))
        .collect();
    section_card(theme, density, rows)
}

/// Global search results: every entry across all panes that matches `query`,
/// each tagged with its source `pane`, hairline-divided. Empty → a muted
/// "no matches" line so the body never looks blank.
pub(super) fn search_results(
    query: &str,
    groups: Vec<(&'static str, Vec<SettingEntry>)>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();
    for (pane, entries) in groups {
        for e in entries {
            if entry_matches(query, &e) {
                rows.push(result_row(pane, e, theme, typography));
            }
        }
    }

    if rows.is_empty() {
        return div()
            .py(px(12.0))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .child(SharedString::from(format!("No settings match “{query}”.")))
            .into_any_element();
    }

    let last = rows.len().saturating_sub(1);
    let mut col = div().flex().flex_col().w_full();
    for (idx, row) in rows.into_iter().enumerate() {
        col = col.child(row);
        if idx != last {
            col = col.child(div().w_full().h(px(1.0)).bg(theme.border_inactive));
        }
    }
    col.into_any_element()
}

/// One global-search result row: a small source-pane tag above the stacked
/// label + description, with the live control pinned right.
fn result_row(
    pane: &'static str,
    e: SettingEntry,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let description = e.description;
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .py(px(12.0))
        .gap(px(16.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .flex_1()
                // See `setting_row_desc` — same wrap floor, same rows.
                .min_w_0()
                .child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(pane)),
                )
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child(e.label),
                )
                .when(!description.is_empty(), |c| {
                    c.child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            .child(description),
                    )
                }),
        )
        .child(e.control)
        .into_any_element()
}

/// One setting row: a stacked `label` + muted `description` on the left, a
/// flexible gap, then `control` pinned to the right edge.
pub(super) fn setting_row_desc(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let description = description.into();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .py(px(12.0))
        .gap(px(16.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .flex_1()
                // Without `min_w_0` a flex item's floor is its *content* width,
                // so a long description refuses to wrap and instead pushes the
                // control off the card's right edge. The two longest strings in
                // the modal live in the launch-environment card, where this hid
                // the profile picker and the env editor entirely.
                .min_w_0()
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child(label.into()),
                )
                .when(!description.is_empty(), |c| {
                    c.child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            .child(description),
                    )
                }),
        )
        // The control is measured on its own terms and never grows: a field
        // that styles itself `w_full` would otherwise claim the whole row as
        // its flex basis and leave the description a single character wide.
        .child(div().flex_none().child(control))
        .into_any_element()
}

/// A setting whose control is the full width of the card: label and
/// description stacked above it rather than beside it.
///
/// [`setting_row_desc`] pins its control to the right at the control's own
/// width, which is correct for a switch, a chip, or a picker. A text editor
/// wants the whole row, and squeezing one into the right-hand column leaves
/// both halves unusable — the field too narrow to read and the description
/// wrapped to a sliver.
pub(super) fn setting_row_stacked(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let description = description.into();
    div()
        .flex()
        .flex_col()
        .w_full()
        .py(px(12.0))
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .w_full()
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child(label.into()),
                )
                .when(!description.is_empty(), |c| {
                    c.child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            .child(description),
                    )
                }),
        )
        .child(div().w_full().child(control))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{query_matches, setting_row_desc};
    use gpui::{
        Bounds, Context, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
        TestAppContext, Window, canvas, div, prelude::FluentBuilder as _, px, size,
    };
    use oximux_settings::{Theme, Typography};
    use std::cell::Cell;
    use std::rc::Rc;

    /// The row's own width in the probe below — narrower than the real settings
    /// body, so the failure it pins reproduces without a 1000px window.
    const ROW_W: f32 = 420.0;
    /// The control's intrinsic width. A control that reaches layout with room
    /// to spare keeps exactly this.
    const CONTROL_W: f32 = 120.0;
    /// The launch-environment card's real description — the longest string in
    /// the modal, and the one that pushed its editor off the card.
    const LONG_DESC: &str = "One KEY=value per line, applied on top of the inherited environment \
         at launch — for both terminal and chat. Stored in plain text in \
         agent_launch.toml; this is not encrypted storage.";

    /// Renders one described row at a fixed width, standing a bounds-recording
    /// canvas in for the control so the test can read where the control
    /// actually landed. Layout faults of this kind are invisible to `render`
    /// unit tests — the element tree is identical either way, only the measured
    /// geometry differs.
    struct RowProbe {
        control: Rc<Cell<Option<Bounds<Pixels>>>>,
        greedy: bool,
    }

    impl Render for RowProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let sink = self.control.clone();
            div().w(px(ROW_W)).child(setting_row_desc(
                "Environment",
                LONG_DESC,
                div().when(self.greedy, |d| d.w_full()).h(px(24.0)).child(
                    canvas(
                        |_, _, _| (),
                        move |bounds: Bounds<Pixels>, _: (), _window, _cx| sink.set(Some(bounds)),
                    )
                    .w(px(CONTROL_W))
                    .h(px(24.0)),
                ),
                Theme::default(),
                &Typography::default(),
            ))
        }
    }

    /// A long description must WRAP inside its column, not shove the control
    /// past the row's right edge. Without `min_w_0` on the text column a flex
    /// item's minimum size is its content width, so the description claimed
    /// ~3x the row and the control — a segmented picker, or the environment
    /// editor itself — was laid out entirely outside the card.
    #[gpui::test]
    fn a_long_description_does_not_push_the_control_out_of_the_row(cx: &mut TestAppContext) {
        let control = Rc::new(Cell::new(None));
        let sink = control.clone();
        let w = cx.add_window(move |_window, _cx| RowProbe { control: sink, greedy: false });
        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(900.0), px(600.0)));
        vcx.run_until_parked();

        let bounds = control.get().expect("the control painted");
        assert!(
            f32::from(bounds.right()) <= ROW_W + 0.5,
            "control right edge {} escaped the {ROW_W}px row",
            f32::from(bounds.right()),
        );
        assert!(
            (f32::from(bounds.size.width) - CONTROL_W).abs() < 0.5,
            "control was squeezed to {} instead of {CONTROL_W}px",
            f32::from(bounds.size.width),
        );
    }

    /// The other half of the same squeeze. A control that styles itself
    /// `w_full` — which a text field does — claims the whole row as its flex
    /// basis, and once `min_w_0` lets the description shrink there is nothing
    /// left to stop it: the text collapsed to one character per line. The
    /// control is therefore measured on its own terms and never grows.
    #[gpui::test]
    fn a_full_width_control_does_not_starve_the_description(cx: &mut TestAppContext) {
        let control = Rc::new(Cell::new(None));
        let sink = control.clone();
        let w = cx.add_window(move |_window, _cx| RowProbe { control: sink, greedy: true });
        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(900.0), px(600.0)));
        vcx.run_until_parked();

        let bounds = control.get().expect("the control painted");
        // Half the row is the floor a two-column setting row has to clear;
        // the real failure left the description a single character.
        assert!(
            f32::from(bounds.origin.x) >= ROW_W / 2.0,
            "description column got only {}px of the {ROW_W}px row",
            f32::from(bounds.origin.x),
        );
        assert!(
            (f32::from(bounds.size.width) - CONTROL_W).abs() < 0.5,
            "control grew to {} instead of its own {CONTROL_W}px",
            f32::from(bounds.size.width),
        );
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(query_matches("", "Scrollback", "lines kept"));
    }

    #[test]
    fn matches_label_case_insensitively() {
        assert!(query_matches("SCROLL", "Scrollback", "lines kept"));
        assert!(query_matches("back", "Scrollback", "lines kept"));
    }

    #[test]
    fn matches_description_when_label_misses() {
        assert!(query_matches("clipboard", "OSC 52", "Write the clipboard"));
    }

    #[test]
    fn non_matching_query_is_rejected() {
        assert!(!query_matches("zzz", "Scrollback", "lines kept"));
    }
}
