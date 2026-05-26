//! Header strip for the file explorer panel.
//!
//! Renders the workspace name (folder leaf) on the left and four
//! actions on the right: show/hide git-ignored entries (eye), collapse
//! all expanded directories, manually refresh from disk, and a `⋯`
//! overflow opening additional items (Open in VS Code / Open in Finder).
//! The eye toggle only appears when the current poll has at least one
//! ignored entry, so a clean repo doesn't show a no-op control.
//!
//! The action buttons use the upstream `Button` widget so their tooltips go
//! through `managed_tooltip` — that gives proper cursor-anchored placement
//! instead of the plain `.tooltip()` which centers on element bounds and
//! gets clamped far to the side near a panel edge.

use crate::shell::file_explorer::FileExplorer;
use gpui::{
    Anchor, AnyElement, ClickEvent, Context, IntoElement, ParentElement, Styled, Window, div, px,
};
// `gpui::Context<PopupMenu>` is required by `dropdown_menu_with_anchor`'s
// closure signature even though the menu builder doesn't use it.
type MenuCtx<'a> = gpui::Context<'a, PopupMenu>;
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
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
        ))
        .child(overflow_menu(explorer.repo_root().clone(), show_ignored));

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

/// Build the `⋯` overflow dropdown. The trigger is a ghost icon button
/// that opens the menu on its own click — the `DropdownMenu` trait
/// renders the popup anchored to the button's top-right, matching the
/// "click anywhere to open" affordance the user expects from a `⋯` icon.
fn overflow_menu(root: std::path::PathBuf, show_ignored: bool) -> impl IntoElement {
    let root_vs = root.clone();
    let root_finder = root;
    Button::new("fe-toolbar-overflow")
        .ghost()
        .xsmall()
        .icon(Icon::default().path("icons/ellipsis.svg"))
        .tooltip("More actions")
        .dropdown_menu_with_anchor(
            Anchor::TopRight,
            move |menu: PopupMenu, _window: &mut Window, _cx: &mut MenuCtx<'_>| {
                build_overflow_menu(menu, show_ignored, root_vs.clone(), root_finder.clone())
            },
        )
}

fn build_overflow_menu(
    menu: PopupMenu,
    _show_ignored: bool,
    root_vs_code: std::path::PathBuf,
    root_finder: std::path::PathBuf,
) -> PopupMenu {
    // Show Git Ignored Files lives next to the inline eye button rather
    // than in this menu — the eye is more discoverable when it matters
    // (i.e. when there are ignored entries). The overflow stays focused
    // on workspace-level external opens.
    menu.min_w(px(200.0))
        .item(PopupMenuItem::new("Open in VS Code").on_click(move |_, _window, cx| {
            cx.dispatch_action(&crate::actions::OpenInVSCode {
                path: root_vs_code.to_string_lossy().into_owned(),
            });
        }))
        .item(PopupMenuItem::new("Open in Finder").on_click(move |_, _window, cx| {
            cx.dispatch_action(&crate::actions::OpenInFinder {
                path: root_finder.to_string_lossy().into_owned(),
            });
        }))
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
