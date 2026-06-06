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
        .child(control)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::query_matches;

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
