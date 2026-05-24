//! File-tree right-click context menu — Open / Open to the Side /
//! Copy Path / Copy Relative Path / Reveal in Finder.
//!
//! Mirrors `TabContextMenu`'s shape: one shared entity owned by
//! `WorkspaceRoot`, opened via a payload action carrying the click
//! coords + the right-clicked filesystem path + an `is_dir` flag.
//! Directory rows get a reduced menu (Reveal + Copy Path only) —
//! Open / Open to the Side / Copy Relative Path don't apply.

use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

/// Width of the dropdown card. Matches `TabContextMenu::MENU_WIDTH`.
pub const MENU_WIDTH: f32 = 188.0;
const CARD_PADDING: f32 = 6.0;
const ITEM_HEIGHT: f32 = 30.0;
const ROW_PADDING_X: f32 = 10.0;

/// Right-click context target: the filesystem path + whether it's a
/// directory. Set at open() time so render doesn't need to walk back
/// into the file tree for the payload.
#[derive(Clone, Debug)]
struct FileTreeContextTarget {
    path: PathBuf,
    /// `None` if no project root is known — the relative-path row is
    /// hidden in that case.
    project_root: Option<PathBuf>,
    is_dir: bool,
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
        self.target = Some(FileTreeContextTarget {
            path,
            project_root,
            is_dir,
        });
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

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(CARD_PADDING))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        // Open + Open to the Side: file-only. Directories get just
        // Reveal + Copy Path (no editor view for a directory).
        if !target.is_dir {
            let open_path = target.path.clone();
            card = card.child(menu_row(
                "file-ctx-open",
                "Open",
                true,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    let path = open_path.clone();
                    this.close(cx);
                    window.dispatch_action(
                        Box::new(crate::actions::OpenFileFromContextMenu {
                            path: path.to_string_lossy().into_owned(),
                            split_right: false,
                        }),
                        cx,
                    );
                }),
            ));
            let side_path = target.path.clone();
            card = card.child(menu_row(
                "file-ctx-open-side",
                "Open to the Side",
                true,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    let path = side_path.clone();
                    this.close(cx);
                    window.dispatch_action(
                        Box::new(crate::actions::OpenFileFromContextMenu {
                            path: path.to_string_lossy().into_owned(),
                            split_right: true,
                        }),
                        cx,
                    );
                }),
            ));
            card = card.child(separator(theme));
        }

        // Copy Path — always available (files + dirs).
        let copy_path = target.path.clone();
        card = card.child(menu_row(
            "file-ctx-copy-path",
            "Copy Path",
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

        // Copy Relative Path — file-only (dirs already root-relative is
        // unusual; matches reference editor scope).
        if !target.is_dir {
            let rel_string = target
                .project_root
                .as_ref()
                .and_then(|root| target.path.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    target
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            card = card.child(menu_row(
                "file-ctx-copy-relative",
                "Copy Relative Path",
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

        // Reveal in Finder — always available.
        let reveal_path = target.path.clone();
        card = card.child(menu_row(
            "file-ctx-reveal-in-finder",
            "Reveal in Finder",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Err(err) = std::process::Command::new("open")
                    .arg("-R")
                    .arg(&reveal_path)
                    .spawn()
                {
                    tracing::warn!(
                        ?err,
                        path = %reveal_path.display(),
                        "reveal in finder failed"
                    );
                }
                this.close(cx);
            }),
        ));

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
        .h(px(ITEM_HEIGHT))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(fg)
        .child(label);
    if enabled {
        row = row
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_panel_alt))
            .on_mouse_down(MouseButton::Left, on_click);
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
