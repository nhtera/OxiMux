//! Body layout primitives for the settings panes: a bordered "section card"
//! that groups rows with hairline dividers, and a described setting row
//! (label + helper text on the left, the control pinned right). These give
//! the panes the grouped, descriptive look of a native preferences window
//! without touching control behavior.

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder, px,
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
    /// Render the control full-width *beneath* the label rather than pinned
    /// right. See [`entry_stacked`] for when a row needs it.
    pub stacked: bool,
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
        stacked: false,
    }
}

/// [`entry`] for a control with no intrinsic width — a text field, an editor,
/// a growing cluster — rendered full-width under the label instead of pinned
/// right.
///
/// The pinned column is `flex_none`, so it is measured on the control's own
/// terms and never stretched. A widget that sizes itself `w_full` (every
/// `Input` does) has no definite parent width there and collapses to its
/// padding: the Voice pane's "Custom words" field shipped as a ~24px box you
/// could not read or aim at. Anything that wants the row must say so here.
pub(super) fn entry_stacked(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
) -> SettingEntry {
    SettingEntry { stacked: true, ..entry(label, description, control) }
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
        .map(|e| {
            if e.stacked {
                setting_row_stack(e.label, e.description, e.control, theme, typography)
            } else {
                setting_row_desc(e.label, e.description, e.control, theme, typography)
            }
        })
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
/// label + description, with the live control pinned right — or, for an
/// [`entry_stacked`] entry, full-width beneath the text, for the same reason
/// its card row stacks.
fn result_row(
    pane: &'static str,
    e: SettingEntry,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let description = e.description;
    let text = div()
        .flex()
        .flex_col()
        .gap(px(3.0))
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
        });

    if e.stacked {
        // No `flex_1` on the text here: this parent is a column, so it would
        // stretch the label block vertically rather than share a row.
        return div()
            .flex()
            .flex_col()
            .w_full()
            .py(px(12.0))
            .gap(px(8.0))
            .child(text.w_full().min_w_0())
            .child(div().w_full().min_w_0().child(e.control))
            .into_any_element();
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .py(px(12.0))
        .gap(px(16.0))
        // See `setting_row_desc` — same wrap floor, same rows.
        .child(text.flex_1().min_w_0())
        .child(div().flex_none().child(e.control))
        .into_any_element()
}

/// The stacked `label` + muted `description` column shared by every setting
/// row shape. Kept in one place so the wrap floor (`min_w_0`, applied by the
/// caller that needs it) and the type scale cannot drift between shapes.
fn label_column(
    label: SharedString,
    description: SharedString,
    theme: Theme,
    typography: &Typography,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(label),
        )
        .when(!description.is_empty(), |c| {
            c.child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .child(description),
            )
        })
}

/// The shared body of [`setting_row_desc`] and [`setting_row_desc_hint`]:
/// label + description left, `control` pinned right, and an optional full-width
/// `hint` line beneath both.
///
/// The hint lives inside the row rather than beside it because [`section_card`]
/// hairlines every child it is handed — a free-standing hint would be divided
/// off from the control it explains.
fn desc_row(
    label: SharedString,
    description: SharedString,
    control: AnyElement,
    hint: Option<AnyElement>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .py(px(12.0))
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full()
                .gap(px(16.0))
                .child(
                    label_column(label, description, theme, typography)
                        .flex_1()
                        // Without `min_w_0` a flex item's floor is its *content*
                        // width, so a long description refuses to wrap and
                        // instead pushes the control off the card's right edge.
                        // The two longest strings in the modal live in the
                        // launch-environment card, where this hid the profile
                        // picker and the env editor entirely.
                        .min_w_0(),
                )
                // The control is measured on its own terms and never grows: a
                // field that styles itself `w_full` would otherwise claim the
                // whole row as its flex basis and leave the description a
                // single character wide.
                .child(div().flex_none().child(control)),
        )
        // The hint spans the row, so it can wrap freely without competing with
        // the control for width the way the description column does.
        .children(hint.map(|h| div().w_full().min_w_0().child(h)))
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
    desc_row(
        label.into(),
        description.into(),
        control.into_any_element(),
        None,
        theme,
        typography,
    )
}

/// [`setting_row_desc`] with a full-width `hint` line beneath the control —
/// the format rule, the state of the thing being picked, or a transient commit
/// message. Build the line with [`hint_text`] or [`notice_text`].
pub(super) fn setting_row_desc_hint(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    hint: impl IntoElement,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    desc_row(
        label.into(),
        description.into(),
        control.into_any_element(),
        Some(hint.into_any_element()),
        theme,
        typography,
    )
}

/// A setting whose `body` is the full width of the card — label and
/// description stacked above it rather than beside it — with an `action`
/// control on the label line and a `hint` line beneath.
///
/// [`setting_row_desc`] pins its control to the right at the control's own
/// width, which is correct for a switch, a chip, or a picker. A list or a text
/// editor wants the whole row, and squeezing one into the right-hand column
/// leaves both halves unusable — the field too narrow to read and the
/// description wrapped to a sliver.
///
/// The hint is where a field's *format rule* belongs: the reference
/// preferences panes all put "one `KEY=value` per line" under the editor rather
/// than inside the description, which lets the description stay one sentence
/// about what the field is for.
#[allow(clippy::too_many_arguments)]
pub(super) fn setting_row_action_hint(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    action: impl IntoElement,
    body: impl IntoElement,
    hint: impl IntoElement,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    stacked_row(
        label.into(),
        description.into(),
        Some(action.into_any_element()),
        body.into_any_element(),
        Some(hint.into_any_element()),
        theme,
        typography,
    )
}

/// A setting whose control is a full-width block under its label, with no
/// trailing action and no hint.
///
/// Use this the moment a control's *intrinsic* width can exceed the row.
/// [`setting_row_desc`] pins its control right at that intrinsic width and
/// never shrinks it, which is correct for a switch or a chip and catastrophic
/// for a growing cluster: the label column is the only thing in the row
/// allowed to shrink, so it absorbs the whole overflow and collapses to one
/// character per line. Not hypothetical — it is what the default-agent picker
/// did the moment its list stopped being four items.
pub(super) fn setting_row_stack(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    body: impl IntoElement,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    stacked_row(
        label.into(),
        description.into(),
        None,
        body.into_any_element(),
        None,
        theme,
        typography,
    )
}

/// The shared body of [`setting_row_action_hint`] and [`setting_row_stack`]:
/// a label line carrying an optional trailing `action`, a full-width `body`
/// beneath it, and an optional `hint` under that.
fn stacked_row(
    label: SharedString,
    description: SharedString,
    action: Option<AnyElement>,
    body: AnyElement,
    hint: Option<AnyElement>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .py(px(12.0))
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full()
                .gap(px(16.0))
                // Same pairing as `desc_row`: the text column may shrink and
                // wrap, the action never does.
                .child(label_column(label, description, theme, typography).flex_1().min_w_0())
                .children(action.map(|a| div().flex_none().child(a))),
        )
        // `min_w_0` so a body that cannot wrap (a long chip cluster) is clipped
        // by the row rather than widening it, which would push the whole card
        // past the pane and take its scroll with it.
        .child(div().w_full().min_w_0().child(body))
        .children(hint.map(|h| div().w_full().min_w_0().child(h)))
        .into_any_element()
}

/// One row of an inset list inside a card row — a name and its résumé on the
/// left, a cluster of actions pinned right, and an accent fill when
/// `selected`. Returned unfinished (`Stateful<Div>`) so the caller attaches
/// its own click handler; every other property is fixed here so the rows of a
/// list cannot drift apart.
///
/// The three width rules are the whole point of this primitive, and this card
/// has already shipped the failure each one prevents: the text column is
/// allowed to shrink below its content and wrap, the action cluster is
/// measured on its own terms and never grows, and the row claims its parent's
/// full width rather than being sized by its content. See
/// `a_list_rows_actions_stay_inside_it_and_its_resume_wraps` for which of them
/// a probe can actually catch.
pub(super) fn list_row(
    id: impl Into<gpui::ElementId>,
    selected: bool,
    text: impl IntoElement,
    controls: impl IntoElement,
    theme: Theme,
    density: Density,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(12.0))
        .px(px(8.0))
        .py(px(7.0))
        .rounded(px(density.r_xs))
        .border_1()
        .cursor_pointer()
        .when(selected, |s| {
            s.bg(gpui::Hsla { a: 0.12, ..theme.status_info }).border_color(theme.status_info)
        })
        .when(!selected, |s| {
            s.border_color(gpui::Hsla { a: 0.0, ..theme.border_inactive })
                .hover(|h| h.bg(theme.bg_panel_alt))
        })
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(text.into_any_element()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .flex_none()
                // A click on an action is not also a click on the row. Stopping
                // it here rather than inside each button keeps the rule in one
                // place and leaves the shared chip widgets untouched — they are
                // used outside any list, where swallowing the click is wrong.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(controls.into_any_element()),
        )
}

/// A muted hint line for beneath a control: a format rule, or a statement of
/// what the current selection means. Always-present, unlike [`notice_text`].
pub(super) fn hint_text(
    text: impl Into<SharedString>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    div()
        .w_full()
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(text.into())
        .into_any_element()
}

/// A transient message acknowledging a commit (`ok`) or explaining why one was
/// refused. Coloured rather than muted, because the whole point is that it is
/// noticed the moment it appears — these replace the silent no-ops that made
/// the environment card read as a broken button.
pub(super) fn notice_text(
    ok: bool,
    text: impl Into<SharedString>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    div()
        .w_full()
        .text_size(px(typography.t_sub_label))
        .text_color(if ok { theme.status_ok } else { theme.status_error })
        .child(text.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{
        entries_card, entry, entry_stacked, hint_text, list_row, query_matches, setting_row_desc,
        setting_row_desc_hint, setting_row_stack,
    };
    use gpui::{
        Bounds, Context, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
        TestAppContext, Window, canvas, div, prelude::FluentBuilder as _, px, size,
    };
    use oximux_settings::{Density, Theme, Typography};
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
    /// A hint long enough to reproduce the wrap fault on its own if the hint
    /// line were ever allowed to set the row's intrinsic width.
    const LONG_HINT: &str = "One KEY=value per line; blank lines and lines starting with # are \
         ignored, and the first = splits each line so a value may contain more of them.";

    /// Renders one described row at a fixed width, standing a bounds-recording
    /// canvas in for the control so the test can read where the control
    /// actually landed. Layout faults of this kind are invisible to `render`
    /// unit tests — the element tree is identical either way, only the measured
    /// geometry differs.
    struct RowProbe {
        control: Rc<Cell<Option<Bounds<Pixels>>>>,
        greedy: bool,
        /// Render the row through `setting_row_desc_hint` with a long hint line
        /// below the control instead of the plain row.
        hinted: bool,
    }

    impl RowProbe {
        fn new(control: Rc<Cell<Option<Bounds<Pixels>>>>) -> Self {
            Self { control, greedy: false, hinted: false }
        }
    }

    impl Render for RowProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let sink = self.control.clone();
            let control = div().when(self.greedy, |d| d.w_full()).h(px(24.0)).child(
                canvas(
                    |_, _, _| (),
                    move |bounds: Bounds<Pixels>, _: (), _window, _cx| sink.set(Some(bounds)),
                )
                .w(px(CONTROL_W))
                .h(px(24.0)),
            );
            let theme = Theme::default();
            let typography = Typography::default();
            let row = if self.hinted {
                setting_row_desc_hint(
                    "Environment",
                    LONG_DESC,
                    control,
                    hint_text(LONG_HINT, theme, &typography),
                    theme,
                    &typography,
                )
            } else {
                setting_row_desc("Environment", LONG_DESC, control, theme, &typography)
            };
            div().w(px(ROW_W)).child(row)
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
        let w = cx.add_window(move |_window, _cx| RowProbe::new(sink));
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
        let w = cx.add_window(move |_window, _cx| RowProbe { greedy: true, ..RowProbe::new(sink) });
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

    /// The hint line added in phase 1 is a THIRD long string in the row that
    /// broke twice already. It spans the row rather than sharing the
    /// description's column, so it must wrap on its own and leave the control
    /// exactly where the un-hinted row put it — neither pushed out (the first
    /// fault) nor squeezed (the second).
    #[gpui::test]
    fn a_long_hint_does_not_disturb_the_control(cx: &mut TestAppContext) {
        let control = Rc::new(Cell::new(None));
        let sink = control.clone();
        let w = cx.add_window(move |_window, _cx| RowProbe { hinted: true, ..RowProbe::new(sink) });
        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(900.0), px(600.0)));
        vcx.run_until_parked();

        let bounds = control.get().expect("the control painted");
        assert!(
            f32::from(bounds.right()) <= ROW_W + 0.5,
            "hinted row's control right edge {} escaped the {ROW_W}px row",
            f32::from(bounds.right()),
        );
        assert!(
            (f32::from(bounds.size.width) - CONTROL_W).abs() < 0.5,
            "hinted row squeezed the control to {} instead of {CONTROL_W}px",
            f32::from(bounds.size.width),
        );
    }

    /// A profile's résumé — the widest text a list row carries, and the only
    /// part of it that grows without bound (flags are free text).
    ///
    /// The long unbroken `--settings=<path>` token is the point: a text
    /// column's automatic minimum size is its longest unbreakable run, so a
    /// résumé of ordinary words cannot reproduce the fault however long it is.
    /// Real flags carry paths, and a path is one word.
    const LONG_RESUME: &str = "flags --dangerously-skip-permissions \
         --settings=/Users/me/.config/oximux/agents/claude-code/proxy-settings.json \
         · model opus · 4 variables";

    /// Stands a bounds-recording canvas in for a list row's action cluster, so
    /// the test can read where the actions actually landed. Same technique as
    /// [`RowProbe`], against the other shape this card renders.
    struct ListRowProbe {
        control: Rc<Cell<Option<Bounds<Pixels>>>>,
    }

    impl Render for ListRowProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let sink = self.control.clone();
            let theme = Theme::default();
            let typography = Typography::default();
            let controls = div().h(px(24.0)).child(
                canvas(
                    |_, _, _| (),
                    move |bounds: Bounds<Pixels>, _: (), _window, _cx| sink.set(Some(bounds)),
                )
                .w(px(CONTROL_W))
                .h(px(24.0)),
            );
            let text = div()
                .flex()
                .flex_col()
                .child(div().text_size(px(typography.t_body_sm)).child("proxy"))
                .child(div().text_size(px(typography.t_sub_label)).child(LONG_RESUME));
            div().w(px(ROW_W)).child(list_row(
                "probe",
                false,
                text,
                controls,
                theme,
                Density::default(),
            ))
        }
    }

    /// The list row is the shape the phase-2 risk names: a growing subtitle
    /// beside a trailing control cluster, which is what erased this card's
    /// controls twice. Measured rather than reasoned about — the element tree
    /// is identical whether the flex rules are right or wrong.
    ///
    /// Measured coverage, established by reverting each rule in turn:
    ///
    /// - Let the text column keep its automatic minimum size (drop BOTH
    ///   `min_w_0` and `overflow_hidden`) and the actions land at x=808 in a
    ///   420px row — the original fault, reproduced in the new shape. The two
    ///   are interchangeable here: `overflow` other than `visible` zeroes the
    ///   automatic minimum size on its own, so either alone holds the row.
    /// - `flex_none` on the cluster cannot be caught by a probe of this shape.
    ///   A `flex_1` text column has a zero flex basis, so the row is never
    ///   under the overflow pressure that would shrink an auto-basis sibling.
    ///   It is kept because it states the intent and holds if the text column
    ///   ever stops being `flex_1`.
    #[gpui::test]
    fn a_list_rows_actions_stay_inside_it_and_its_resume_wraps(cx: &mut TestAppContext) {
        let control = Rc::new(Cell::new(None));
        let sink = control.clone();
        let w = cx.add_window(move |_window, _cx| ListRowProbe { control: sink });
        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(900.0), px(600.0)));
        vcx.run_until_parked();

        let bounds = control.get().expect("the action cluster painted");
        assert!(
            f32::from(bounds.right()) <= ROW_W + 0.5,
            "actions right edge {} escaped the {ROW_W}px row",
            f32::from(bounds.right()),
        );
        assert!(
            (f32::from(bounds.size.width) - CONTROL_W).abs() < 0.5,
            "actions were resized to {} instead of {CONTROL_W}px",
            f32::from(bounds.size.width),
        );
        assert!(
            f32::from(bounds.origin.x) >= ROW_W / 2.0,
            "the name + résumé column got only {}px of the {ROW_W}px row",
            f32::from(bounds.origin.x),
        );
    }

    /// Wider than `ROW_W` — eight chips' worth. The shape a growing cluster
    /// becomes; every other probe here uses a control that FITS, which is
    /// exactly the case `setting_row_desc` handles correctly.
    const WIDE_W: f32 = 640.0;

    /// Renders one row with an over-wide control and measures the row's
    /// HEIGHT, by reading where a canvas placed directly beneath it lands.
    ///
    /// Height is the signal, not width: pinned right, the over-wide control
    /// keeps its intrinsic size and the label column — the only shrinkable
    /// thing in the row — is squeezed to nothing, so "Default agent" wraps to
    /// one character per line and the row grows several times taller. Reading
    /// the control's own bounds cannot tell the two shapes apart; the label's
    /// collapse is what is visible.
    struct WideProbe {
        below: Rc<Cell<Option<Bounds<Pixels>>>>,
        pinned: bool,
    }

    impl Render for WideProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let sink = self.below.clone();
            let theme = Theme::default();
            let typography = Typography::default();
            let body = div().h(px(24.0)).child(div().w(px(WIDE_W)).h(px(24.0)));
            let row = if self.pinned {
                setting_row_desc("Default agent", "Surfaced first in the launcher.", body, theme, &typography)
            } else {
                setting_row_stack("Default agent", "Surfaced first in the launcher.", body, theme, &typography)
            };
            div().w(px(ROW_W)).flex().flex_col().child(row).child(
                canvas(
                    |_, _, _| (),
                    move |bounds: Bounds<Pixels>, _: (), _window, _cx| sink.set(Some(bounds)),
                )
                .w_full()
                .h(px(1.0)),
            )
        }
    }

    fn wide_row_height(cx: &mut TestAppContext, pinned: bool) -> f32 {
        let below = Rc::new(Cell::new(None));
        let sink = below.clone();
        let w = cx.add_window(move |_window, _cx| WideProbe { below: sink, pinned });
        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(900.0), px(600.0)));
        vcx.run_until_parked();
        f32::from(below.get().expect("the marker painted").origin.y)
    }

    /// A control cluster wider than its row must be stacked, not pinned right.
    ///
    /// This is the fault phase 5 shipped and no existing probe caught. The two
    /// shapes are compared against each other rather than against a fixed
    /// pixel budget, so the guard survives a font-metric change and still
    /// fails the moment a growing cluster is routed back through
    /// [`setting_row_desc`].
    #[gpui::test]
    fn a_cluster_wider_than_its_row_must_be_stacked_not_pinned(cx: &mut TestAppContext) {
        let stacked = wide_row_height(cx, false);
        let pinned = wide_row_height(cx, true);
        assert!(
            pinned > stacked * 1.5,
            "the probe no longer tells the two shapes apart — pinned {pinned}px vs \
             stacked {stacked}px. Either the pinned shape stopped starving its label \
             (good: rewrite this test) or the probe stopped reproducing the fault.",
        );
        // A stacked row is 12+12 padding, two 8px gaps, a label line, a
        // description line and a 24px body — a shade over 90. Well clear of a
        // label that has wrapped even twice.
        assert!(
            stacked < 110.0,
            "a stacked row is a label line, a description line and a 24px body; \
             {stacked}px means the label wrapped when it should not have",
        );
    }

    /// A text field carries no intrinsic width of its own — `Input` sizes
    /// itself `size_full` — so it is the mirror image of the wide cluster
    /// above: pinned right it does not overflow, it VANISHES, collapsing to
    /// its padding while the row looks otherwise correct.
    ///
    /// Rendered through [`entries_card`] rather than a row helper directly, so
    /// the probe covers the dispatch as well as the shape: an entry built with
    /// [`entry`] must collapse and one built with [`entry_stacked`] must fill.
    struct FieldProbe {
        field: Rc<Cell<Option<Bounds<Pixels>>>>,
        stacked: bool,
    }

    impl Render for FieldProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let sink = self.field.clone();
            let theme = Theme::default();
            let typography = Typography::default();
            // Stands in for `Input`: no intrinsic size, fills whatever
            // definite width its parent gives it.
            let field = div().w_full().h(px(24.0)).child(
                canvas(
                    |_, _, _| (),
                    move |bounds: Bounds<Pixels>, _: (), _window, _cx| sink.set(Some(bounds)),
                )
                .w_full()
                .h(px(24.0)),
            );
            let e = if self.stacked {
                entry_stacked("Custom words", "Names to correct toward.", field)
            } else {
                entry("Custom words", "Names to correct toward.", field)
            };
            div()
                .w(px(ROW_W))
                .child(entries_card(theme, Density::default(), &typography, vec![e]))
        }
    }

    fn field_width(cx: &mut TestAppContext, stacked: bool) -> f32 {
        let field = Rc::new(Cell::new(None));
        let sink = field.clone();
        let w = cx.add_window(move |_window, _cx| FieldProbe { field: sink, stacked });
        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(900.0), px(600.0)));
        vcx.run_until_parked();
        f32::from(field.get().expect("the field painted").size.width)
    }

    /// The Voice pane's "Custom words" field shipped as an unusable ~24px box.
    /// Both halves are asserted: that the stacked row actually gives the field
    /// the row, and that the pinned row is still the shape that starves it —
    /// the second half is what keeps the first from passing vacuously.
    #[gpui::test]
    fn a_text_field_pinned_right_collapses_and_stacked_fills_the_row(cx: &mut TestAppContext) {
        let stacked = field_width(cx, true);
        let pinned = field_width(cx, false);
        assert!(
            stacked >= ROW_W - 0.5,
            "a stacked field got {stacked}px of the {ROW_W}px row",
        );
        assert!(
            pinned < ROW_W / 4.0,
            "the probe no longer reproduces the fault: a pinned field took {pinned}px of \
             the {ROW_W}px row. Either `desc_row` stopped starving width-less controls \
             (good: rewrite this test) or the stand-in stopped behaving like `Input`.",
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
