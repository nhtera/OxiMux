//! Breadcrumb header for `EditorView`: a clickable copy-path label and a single
//! "⋯" overflow menu holding every file action (copy contents, reveal in
//! Finder, open in an external editor).
//!
//! Collapsing the actions into one overflow menu — rather than a row of inline
//! icons — keeps the header quiet and matches how a modern editor surfaces
//! file-level actions. The heavier rendering lives here (not `editor_view.rs`)
//! to keep that file under the size cap.

use std::path::Path;
use std::process::Command;

use gpui::{
    AnyElement, App, ClipboardItem, Context, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::{
    ActiveTheme, IconName, Selectable, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    notification::Notification,
    v_flex,
};

use crate::editor_view::EditorView;

/// An external editor offered in the actions menu. `app` is the macOS
/// application name handed to `open -a`.
struct ExternalEditor {
    label: &'static str,
    app: &'static str,
}

/// Editors we offer to hand a file to, in menu order. Only those actually
/// installed (an `<app>.app` bundle present) are listed, so the menu never
/// shows a dead option.
const EXTERNAL_EDITORS: &[ExternalEditor] = &[
    ExternalEditor { label: "Cursor", app: "Cursor" },
    ExternalEditor { label: "VS Code", app: "Visual Studio Code" },
    ExternalEditor { label: "Windsurf", app: "Windsurf" },
    ExternalEditor { label: "Zed", app: "Zed" },
];

fn is_installed(app: &str) -> bool {
    Path::new("/Applications").join(format!("{app}.app")).exists()
}

fn installed_editors() -> Vec<&'static ExternalEditor> {
    EXTERNAL_EDITORS
        .iter()
        .filter(|e| is_installed(e.app))
        .collect()
}

fn open_in_external(app: &str, path: &Path) {
    if let Err(err) = Command::new("open").arg("-a").arg(app).arg(path).spawn() {
        tracing::warn!(?err, app, "editor: open-in-external failed");
    }
}

fn reveal_in_finder(path: &Path) {
    if let Err(err) = Command::new("open").arg("-R").arg(path).spawn() {
        tracing::warn!(?err, "editor: reveal-in-finder failed");
    }
}

/// Write `value` to the clipboard and confirm with a toast.
fn copy_with_toast(value: String, toast: &'static str, window: &mut Window, cx: &mut App) {
    tracing::info!(toast, "editor-header: copy action fired; pushing notification");
    cx.write_to_clipboard(ClipboardItem::new_string(value));
    window.push_notification(Notification::success(toast), cx);
}

/// The single "⋯" overflow button for the breadcrumb. Toggles the actions menu.
pub fn actions_button(menu_open: bool, cx: &Context<EditorView>) -> AnyElement {
    Button::new(("ed-actions", cx.entity_id()))
        .ghost()
        .xsmall()
        .icon(IconName::Ellipsis)
        .selected(menu_open)
        .tooltip("Actions")
        .on_click(cx.listener(|view, _, _window, cx| {
            view.toggle_actions_menu();
            cx.notify();
        }))
        .into_any_element()
}

/// Make the breadcrumb path text a one-click "copy path" affordance.
pub fn clickable_path(label: String, path: &Path, cx: &Context<EditorView>) -> AnyElement {
    let fg = cx.theme().foreground;
    let path = path.to_path_buf();
    div()
        .id(("ed-breadcrumb-path", cx.entity_id()))
        .cursor_pointer()
        .hover(|s| s.text_color(fg))
        .on_click(cx.listener(move |_view, _, window, cx| {
            copy_with_toast(
                path.to_string_lossy().into_owned(),
                "Path copied to clipboard",
                window,
                cx,
            );
        }))
        .child(label)
        .into_any_element()
}

/// The actions dropdown: a full-bleed backdrop that dismisses on click plus a
/// card anchored under the "⋯" button. Holds copy-contents (text files),
/// reveal in Finder, and an "Open in <editor>" row per installed editor.
/// Rendered at the editor root so it paints above the body.
pub fn actions_overlay(path: &Path, has_text: bool, cx: &Context<EditorView>) -> AnyElement {
    let view_id = cx.entity_id();
    let theme = cx.theme();
    let popover = theme.popover;
    let border = theme.border;
    let radius = theme.radius;
    let accent = theme.accent;
    let fg = theme.foreground;

    let mut card = v_flex()
        .min_w(px(210.0))
        .p(px(4.0))
        .gap(px(1.0))
        .bg(popover)
        .border_1()
        .border_color(border)
        .rounded(radius)
        .shadow_md();

    if has_text {
        card = card.child(menu_row(
            ("ed-m-copy", view_id),
            "Copy file contents",
            accent,
            fg,
            cx.listener(|view, _, window, cx| {
                if let Some(text) = view.current_text(cx) {
                    copy_with_toast(text, "File contents copied", window, cx);
                }
                view.close_actions_menu();
                cx.notify();
            }),
        ));
    }

    let reveal_target = path.to_path_buf();
    card = card.child(menu_row(
        ("ed-m-reveal", view_id),
        "Reveal in Finder",
        accent,
        fg,
        cx.listener(move |view, _, _w, cx| {
            reveal_in_finder(&reveal_target);
            view.close_actions_menu();
            cx.notify();
        }),
    ));

    // Reveal in OxiMux's own file-tree sidebar (expand ancestors + scroll to
    // the row). The editor crate doesn't own the tree, so dispatch an action
    // the host shell handles.
    let explorer_target = path.to_path_buf();
    card = card.child(menu_row(
        ("ed-m-reveal-tree", view_id),
        "Reveal in Explorer View",
        accent,
        fg,
        cx.listener(move |view, _, window, cx| {
            window.dispatch_action(
                Box::new(crate::editor_view::RevealInExplorer {
                    path: explorer_target.to_string_lossy().into_owned(),
                }),
                cx,
            );
            view.close_actions_menu();
            cx.notify();
        }),
    ));

    let editors = installed_editors();
    if !editors.is_empty() {
        card = card.child(separator(border));
        for ed in editors {
            let app = ed.app;
            let target = path.to_path_buf();
            card = card.child(menu_row(
                (ed.app, view_id),
                format!("Open in {}", ed.label),
                accent,
                fg,
                cx.listener(move |view, _, _w, cx| {
                    open_in_external(app, &target);
                    view.close_actions_menu();
                    cx.notify();
                }),
            ));
        }
    }

    div()
        .absolute()
        .inset_0()
        .size_full()
        .occlude()
        // Backdrop: any click outside the card dismisses the menu.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _, _w, cx| {
                view.close_actions_menu();
                cx.notify();
            }),
        )
        .child(
            div()
                .absolute()
                .top(px(30.0))
                .right(px(10.0))
                // Clicks inside the card must not reach the backdrop, or the
                // backdrop's mouse-down would close the menu before a row's
                // click can complete (the row element would be gone on mouse-up).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(card),
        )
        .into_any_element()
}

/// A 1px divider between menu groups.
fn separator(border: Hsla) -> impl IntoElement {
    div().h(px(1.0)).my(px(3.0)).mx(px(4.0)).bg(border)
}

/// One row in the actions card — a left-aligned, hover-highlighted button.
fn menu_row(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    accent: Hsla,
    fg: Hsla,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .text_color(fg)
        .cursor_pointer()
        .hover(|s| s.bg(accent))
        .on_click(on_click)
        .child(label.into())
}
