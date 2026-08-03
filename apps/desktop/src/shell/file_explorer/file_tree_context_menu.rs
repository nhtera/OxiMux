//! File-tree right-click context menu.
//!
//! Three open modes, one shared entity owned by `WorkspaceRoot`:
//! - **Row (file)** — full menu: Open / Open to the Side / New File /
//!   New Folder / Copy Path / Copy Relative Path / Reveal in Finder /
//!   Rename / Delete.
//! - **Row (directory)** — same as file minus Open / Open to the Side
//!   plus Find in Folder.
//! - **Background** (empty area below the tree) — only New File /
//!   New Folder, rooted at the project root.
//!
//! Opened via three payload actions: `OpenFileTreeContextMenuAt` (rows)
//! and `OpenFileTreeBackgroundMenuAt` (empty area). The menu hides
//! itself on any outside click.

use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;

/// Width of the dropdown card. Matches `TabContextMenu::MENU_WIDTH`.
pub const MENU_WIDTH: f32 = 200.0;
const ROW_PADDING_X: f32 = 10.0;

/// What the menu is targeting at open time. `Row` covers right-clicks
/// on a file or directory row; `Background` covers right-clicks in the
/// empty area below the tree (offers "New File" / "New Folder" at the
/// workspace root).
#[derive(Clone, Debug)]
enum FileTreeContextTarget {
    Row {
        path: PathBuf,
        /// `None` if no project root is known — the relative-path row is
        /// hidden in that case.
        project_root: Option<PathBuf>,
        is_dir: bool,
    },
    Background {
        root: PathBuf,
    },
}

pub struct FileTreeContextMenu {
    open: bool,
    x_px: f32,
    y_px: f32,
    target: Option<FileTreeContextTarget>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl FileTreeContextMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            x_px: 0.0,
            y_px: 0.0,
            target: None,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(
        &mut self,
        x_px: f32,
        y_px: f32,
        path: PathBuf,
        project_root: Option<PathBuf>,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(FileTreeContextTarget::Row {
            path,
            project_root,
            is_dir,
        });
        self.open = true;
        cx.notify();
    }

    /// Open the smaller "empty area" menu rooted at the workspace
    /// root — only offers New File / New Folder.
    pub fn open_background(&mut self, x_px: f32, y_px: f32, root: PathBuf, cx: &mut Context<Self>) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(FileTreeContextTarget::Background { root });
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }
}

impl Render for FileTreeContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let Some(target) = self.target.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let x_px = self.x_px;
        let y_px = self.y_px;

        let card_base = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg();

        let card = match target {
            FileTreeContextTarget::Row {
                path,
                project_root,
                is_dir,
            } => build_row_card(
                card_base,
                path,
                project_root,
                is_dir,
                theme,
                density,
                &typography,
                cx,
            ),
            FileTreeContextTarget::Background { root } => {
                build_background_card(card_base, root, theme, density, &typography, cx)
            }
        };

        let left_px = (x_px - MENU_WIDTH).max(0.0);
        let card_container = div()
            .absolute()
            .top(px(y_px))
            .left(px(left_px))
            .w(px(MENU_WIDTH))
            .child(card);

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    this.close(cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    this.close(cx);
                }),
            )
            .child(card_container)
            .into_any_element()
    }
}

#[allow(clippy::too_many_arguments)]
fn build_row_card(
    mut card: gpui::Div,
    path: PathBuf,
    project_root: Option<PathBuf>,
    is_dir: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<FileTreeContextMenu>,
) -> gpui::Div {
    // ── 1. Editor opens (file rows only). Directories skip these
    //   entirely — the parent's editor view doesn't make sense.
    if !is_dir {
        let open_path = path.clone();
        card = card.child(menu_row(
            "file-ctx-open",
            "Open",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                let p = open_path.clone();
                this.close(cx);
                window.dispatch_action(
                    Box::new(crate::actions::OpenFileFromContextMenu {
                        path: p.to_string_lossy().into_owned(),
                        split_right: false,
                    }),
                    cx,
                );
            }),
        ));
        let side_path = path.clone();
        card = card.child(menu_row(
            "file-ctx-open-side",
            "Open to the Side",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                let p = side_path.clone();
                this.close(cx);
                window.dispatch_action(
                    Box::new(crate::actions::OpenFileFromContextMenu {
                        path: p.to_string_lossy().into_owned(),
                        split_right: true,
                    }),
                    cx,
                );
            }),
        ));
        card = card.child(separator(theme));
    }

    // ── 2. Create-new ops. New File/Folder are scoped to the
    //   target's parent directory for files, or the directory itself
    //   for directories — that's the conventional behavior of right-
    //   clicking and choosing "New …".
    let new_parent = if is_dir {
        path.clone()
    } else {
        path.parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| path.clone())
    };
    let new_file_parent = new_parent.clone();
    card = card.child(menu_row(
        "file-ctx-new-file",
        "New File",
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let parent = new_file_parent.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeNewFile {
                    parent: parent.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ));
    let new_folder_parent = new_parent;
    card = card.child(menu_row(
        "file-ctx-new-folder",
        "New Folder",
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let parent = new_folder_parent.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeNewFolder {
                    parent: parent.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ));
    card = card.child(separator(theme));

    // ── 3. Clipboard ops. Copy Path is always available; Copy
    //   Relative Path needs a project root and is hidden when the
    //   target sits outside the workspace.
    let copy_path = path.clone();
    card = card.child(menu_row_with_shortcut(
        "file-ctx-copy-path",
        "Copy Path",
        Some("⌘⌥C"),
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(
                copy_path.to_string_lossy().into_owned(),
            ));
            this.close(cx);
        }),
    ));
    if let Some(rel_string) = relative_path_string(&path, project_root.as_ref()) {
        card = card.child(menu_row_with_shortcut(
            "file-ctx-copy-relative",
            "Copy Relative Path",
            Some("⌘⌥⇧C"),
            !rel_string.is_empty(),
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(rel_string.clone()));
                this.close(cx);
            }),
        ));
    }

    // ── Duplicate — copy the file/folder next to itself with a collision-free
    //   " copy" name. Available for both files and directories.
    let duplicate_path_target = path.clone();
    card = card.child(menu_row(
        "file-ctx-duplicate",
        "Duplicate",
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let p = duplicate_path_target.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeDuplicate {
                    path: p.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ));

    // ── 4. Find in Folder (directory-only). Seeds the Search panel's
    //   include glob with `<rel>/**` and switches the right sidebar
    //   to the Search tab. Files don't get this — there's no folder
    //   to scope into.
    if is_dir {
        let find_path = path.clone();
        card = card.child(menu_row(
            "file-ctx-find-in-folder",
            "Find in Folder",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                let p = find_path.clone();
                this.close(cx);
                window.dispatch_action(
                    Box::new(crate::actions::FindInFolder {
                        path: p.to_string_lossy().into_owned(),
                    }),
                    cx,
                );
            }),
        ));
    }

    // ── 5. Reveal in Finder — always available.
    let reveal_path = path.clone();
    card = card.child(menu_row(
        "file-ctx-reveal-in-finder",
        "Reveal in Finder",
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            cx.reveal_path(&reveal_path);
            this.close(cx);
        }),
    ));
    card = card.child(separator(theme));

    // ── 6. Mutating ops. Phase 02 ships the menu items; the inline
    //   input + confirm-dialog wiring lands in Phase 03 behind the
    //   same action payload.
    let rename_path = path.clone();
    card = card.child(menu_row_with_shortcut(
        "file-ctx-rename",
        "Rename…",
        Some("↵"),
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let p = rename_path.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeRename {
                    path: p.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ));
    let delete_path = path.clone();
    card = card.child(menu_row_destructive(
        "file-ctx-delete",
        "Delete",
        Some("⌘⌫"),
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let p = delete_path.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeDelete {
                    path: p.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ));
    card
}

fn build_background_card(
    mut card: gpui::Div,
    root: PathBuf,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<FileTreeContextMenu>,
) -> gpui::Div {
    let new_file_root = root.clone();
    card = card.child(menu_row(
        "file-ctx-bg-new-file",
        "New File",
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let r = new_file_root.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeNewFile {
                    parent: r.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ));
    let new_folder_root = root;
    card.child(menu_row(
        "file-ctx-bg-new-folder",
        "New Folder",
        true,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            let r = new_folder_root.clone();
            this.close(cx);
            window.dispatch_action(
                Box::new(crate::actions::FileTreeNewFolder {
                    parent: r.to_string_lossy().into_owned(),
                }),
                cx,
            );
        }),
    ))
}

/// Compute the path relative to the project root for the Copy Relative
/// Path item. Falls back to the file name when the path sits outside the
/// project root (e.g. a symlinked file).
fn relative_path_string(path: &std::path::Path, project_root: Option<&PathBuf>) -> Option<String> {
    let rel = project_root
        .as_ref()
        .and_then(|root| path.strip_prefix(root.as_path()).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
    rel.filter(|s| !s.is_empty())
}


fn separator(theme: Theme) -> impl IntoElement {
    div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive)
}

fn menu_row<H>(
    row_id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
{
    menu_row_with_shortcut(row_id, label, None, enabled, theme, density, typography, on_click)
}

/// `menu_row` plus an optional right-aligned shortcut hint (e.g. `⌘⌥C`). The
/// hint is display-only — it documents the keyboard binding handled by the
/// file explorer's key handler; it does not itself wire a binding.
#[allow(clippy::too_many_arguments)]
fn menu_row_with_shortcut<H>(
    row_id: &'static str,
    label: &'static str,
    shortcut: Option<&'static str>,
    enabled: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
{
    let fg = if enabled {
        theme.fg_base
    } else {
        theme.fg_subtle
    };
    let mut row = div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .h(px(density.h_overlay_item))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(fg)
        .child(label);
    if let Some(shortcut) = shortcut {
        row = row.child(div().flex_1()).child(
            div()
                .text_color(theme.fg_subtle)
                .text_size(px(typography.t_body_sm))
                .child(shortcut),
        );
    }
    if enabled {
        row = row
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .on_mouse_down(MouseButton::Left, on_click);
    }
    row
}

/// Same shape as `menu_row` but paints the label in `status_error` so
/// the user has a visual brake before clicking. Used by the Delete row.
/// Carries an optional right-aligned shortcut hint like the regular row.
fn menu_row_destructive<H>(
    row_id: &'static str,
    label: &'static str,
    shortcut: Option<&'static str>,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
{
    let mut row = div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .h(px(density.h_overlay_item))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(theme.status_error)
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label);
    if let Some(shortcut) = shortcut {
        row = row.child(div().flex_1()).child(
            div()
                .text_color(theme.fg_subtle)
                .text_size(px(typography.t_body_sm))
                .child(shortcut),
        );
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> FileTreeContextMenu {
        FileTreeContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target.is_none());
    }
}
