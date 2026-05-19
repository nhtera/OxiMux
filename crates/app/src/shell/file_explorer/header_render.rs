//! Header strip for the file explorer panel.
//!
//! Renders the workspace name (folder leaf) on the left and a small action
//! row on the right with three icon buttons: show/hide git-ignored entries,
//! collapse all expanded directories, and manually refresh from disk. The
//! eye toggle only appears when the current poll has at least one ignored
//! entry, so a clean repo doesn't show a no-op control.
//!
//! The action buttons use the upstream `Button` widget so their tooltips go
//! through `managed_tooltip` — that gives proper cursor-anchored placement
//! instead of the plain `.tooltip()` which centers on element bounds and
//! gets clamped far to the side near a panel edge.

use crate::shell::file_explorer::FileExplorer;
use gpui::{AnyElement, ClickEvent, Context, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};

/// Identifies which toolbar action a header button maps to.
#[derive(Clone, Copy)]
enum HeaderAction {
    ToggleIgnored,
    CollapseAll,
    Refresh,
}

/// Build the header strip. Returns `AnyElement` rather than `impl IntoElement`
/// because the listener closures borrow `cx` mutably during construction;
/// under the Rust 2024 capture rules `impl IntoElement` would propagate that
/// borrow to the caller and prevent later use of `cx` in the same `render`.
pub fn render_header(explorer: &FileExplorer, cx: &mut Context<FileExplorer>) -> AnyElement {
    let theme = explorer.theme();
    let typography = explorer.typography_ref();
    let title = explorer
        .repo_root()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Files".to_string());

    let show_ignored = explorer.show_ignored();
    let has_ignored = explorer.has_ignored_entries();
    let can_collapse = explorer.can_collapse_all();

    let title_el = div()
        .flex_1()
        .overflow_hidden()
        .text_size(px(typography.t_label_caps))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_muted)
        .child(title);

    let mut actions = div().flex().flex_row().items_center().gap(px(2.0));
    if has_ignored {
        let (icon, tip) = if show_ignored {
            ("icons/eye-off.svg", "Hide Git Ignored Files")
        } else {
            ("icons/eye.svg", "Show Git Ignored Files")
        };
        actions = actions.child(action_button(
            "fe-toolbar-eye",
            icon,
            tip,
            HeaderAction::ToggleIgnored,
            true,
            cx,
        ));
    }
    actions = actions
        .child(action_button(
            "fe-toolbar-collapse",
            "icons/list-collapse.svg",
            "Collapse All",
            HeaderAction::CollapseAll,
            can_collapse,
            cx,
        ))
        .child(action_button(
            "fe-toolbar-refresh",
            "icons/refresh-cw.svg",
            "Refresh Explorer",
            HeaderAction::Refresh,
            true,
            cx,
        ));

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .h(px(28.0))
        .pl(px(10.0))
        .pr(px(4.0))
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(title_el)
        .child(actions)
        .into_any_element()
}

fn action_button(
    id: &'static str,
    icon_path: &'static str,
    tooltip_text: &'static str,
    action: HeaderAction,
    enabled: bool,
    cx: &mut Context<FileExplorer>,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::default().path(icon_path))
        .tooltip(tooltip_text)
        .disabled(!enabled)
        .on_click(
            cx.listener(move |me, _: &ClickEvent, _window, cx| match action {
                HeaderAction::ToggleIgnored => me.toggle_show_ignored(cx),
                HeaderAction::CollapseAll => me.collapse_all(cx),
                HeaderAction::Refresh => me.manual_refresh(cx),
            }),
        )
}
