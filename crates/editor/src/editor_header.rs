//! Breadcrumb header actions for `EditorView`: copy path, copy file contents,
//! reveal in Finder, and an "Open in <external editor>" menu.
//!
//! These mirror the affordances of a modern file viewer's header so an open
//! file isn't a dead-end — the path is one click from the clipboard, contents
//! are one click from a paste, and the file can be handed to Finder or another
//! editor. The heavier rendering lives here (not `editor_view.rs`) to keep that
//! file under the size cap.

use std::path::Path;
use std::process::Command;

use gpui::{
    AnyElement, App, ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::{
    ActiveTheme, IconName, Selectable, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    h_flex,
    notification::Notification,
    v_flex,
};

use crate::editor_view::EditorView;

/// An external editor offered in the "Open in" menu. `app` is the macOS
/// application name handed to `open -a`.
struct ExternalEditor {
    label: &'static str,
    app: &'static str,
}

/// Editors we offer to hand a file to, in menu order. Only those actually
/// installed (an `<app>.app` bundle present) are shown, so the menu never lists
/// a dead option.
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

/// `true` when the "Open in" button should appear — only if at least one
/// external editor is installed (Finder is always reachable via Reveal).
pub fn has_external_editors() -> bool {
    EXTERNAL_EDITORS.iter().any(|e| is_installed(e.app))
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
    cx.write_to_clipboard(ClipboardItem::new_string(value));
    window.push_notification(Notification::success(toast), cx);
}

/// The right-aligned action buttons for the breadcrumb: copy contents (text
/// files only — `has_text`), reveal in Finder, and the "Open in" toggle. The
/// copy reads the live buffer at click time (reflecting unsaved edits) rather
/// than cloning it every frame. The "Open in" button flips the menu state.
pub fn action_buttons(
    path: &Path,
    has_text: bool,
    menu_open: bool,
    cx: &Context<EditorView>,
) -> AnyElement {
    let view_id = cx.entity_id();
    let reveal_path = path.to_path_buf();
    // Order + icons mirror a modern file viewer's header: reveal, open-in,
    // copy. Element ids are entity-scoped so split panes showing two files
    // never collide.
    let mut row = h_flex().gap(px(2.0)).items_center();

    row = row.child(
        Button::new(("ed-reveal", view_id))
            .ghost()
            .xsmall()
            .icon(IconName::Folder)
            .tooltip("Reveal in Finder")
            .on_click(cx.listener(move |_view, _, _window, _cx| {
                reveal_in_finder(&reveal_path);
            })),
    );

    if has_external_editors() {
        row = row.child(
            Button::new(("ed-open-in", view_id))
                .ghost()
                .xsmall()
                .icon(IconName::FolderOpen)
                .selected(menu_open)
                .tooltip("Open in…")
                .on_click(cx.listener(|view, _, _window, cx| {
                    view.toggle_open_in_menu();
                    cx.notify();
                })),
        );
    }

    if has_text {
        row = row.child(
            Button::new(("ed-copy-contents", view_id))
                .ghost()
                .xsmall()
                .icon(IconName::Copy)
                .tooltip("Copy file contents")
                .on_click(cx.listener(|view, _, window, cx| {
                    if let Some(text) = view.current_text(cx) {
                        copy_with_toast(text, "File contents copied", window, cx);
                    }
                })),
        );
    }

    row.into_any_element()
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

/// The "Open in" dropdown: a full-bleed backdrop that dismisses on click plus a
/// card anchored under the breadcrumb listing installed editors and "Show in
/// Finder". Rendered at the editor root so it paints above the body.
pub fn open_in_overlay(path: &Path, cx: &Context<EditorView>) -> AnyElement {
    let view_id = cx.entity_id();
    let theme = cx.theme();
    let popover = theme.popover;
    let border = theme.border;
    let radius = theme.radius;
    let accent = theme.accent;
    let fg = theme.foreground;

    let mut card = v_flex()
        .min_w(px(190.0))
        .p(px(4.0))
        .gap(px(1.0))
        .bg(popover)
        .border_1()
        .border_color(border)
        .rounded(radius)
        .shadow_md();

    for ed in installed_editors() {
        let app = ed.app;
        let target = path.to_path_buf();
        card = card.child(menu_row(
            (ed.label, view_id),
            ed.label,
            accent,
            fg,
            cx.listener(move |view, _, _w, cx| {
                open_in_external(app, &target);
                view.close_open_in_menu();
                cx.notify();
            }),
        ));
    }

    card = card.child(
        div()
            .h(px(1.0))
            .my(px(3.0))
            .mx(px(4.0))
            .bg(border),
    );
    let target = path.to_path_buf();
    card = card.child(menu_row(
        ("show-in-finder", view_id),
        "Show in Finder",
        accent,
        fg,
        cx.listener(move |view, _, _w, cx| {
            reveal_in_finder(&target);
            view.close_open_in_menu();
            cx.notify();
        }),
    ));

    div()
        .absolute()
        .inset_0()
        // Backdrop: any click outside the card dismisses the menu.
        .child(
            div()
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, _w, cx| {
                        view.close_open_in_menu();
                        cx.notify();
                    }),
                ),
        )
        .child(div().absolute().top(px(30.0)).right(px(10.0)).child(card))
        .into_any_element()
}

/// One row in the "Open in" card — a left-aligned, hover-highlighted button.
fn menu_row(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    accent: gpui::Hsla,
    fg: gpui::Hsla,
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
        .child(label)
}
