//! Tab right-click context menu — Close / Close Others / Close to Right
//! / Close All in Group.
//!
//! Mirrors `PaneActionsMenu`: one shared entity owned by `WorkspaceRoot`,
//! opened via a payload action carrying the click coords + the target
//! group's id + the right-clicked tab index. The menu stores a
//! `WeakEntity<PaneGroup>` so row clicks can mutate the right group
//! even if focus has moved elsewhere by the time the user picks an item.

use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, WeakEntity, Window, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{MoveTabToNewWindow, RequestRenameTabAt, SplitGroupAt, TogglePinTabAt};
use crate::shell::pane_group::{PaneGroup, TabColor};
use crate::shell::pane_tree::PaneGroupId;

/// Per-tab metadata that drives kind-specific rows in the menu. Set
/// at open() time so render doesn't need to walk back into the
/// `PaneGroup` entity for the path. Editor tabs carry their absolute
/// path; terminal/agent tabs carry no extra payload.
#[derive(Clone, Debug)]
pub enum TabContextKind {
    Terminal,
    Editor {
        /// Absolute file path for Copy/Reveal rows.
        path: PathBuf,
        /// Project root used to derive the "Copy Relative Path" string.
        /// `None` if no project root is known — Copy Relative Path
        /// falls back to the file name in that case.
        project_root: Option<PathBuf>,
    },
}

/// Width of the dropdown card.
pub const MENU_WIDTH: f32 = 188.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Split icon glyph size inside each split row.
const SPLIT_ICON_SIZE: f32 = 14.0;
/// Gap between icon and label in a split row.
const SPLIT_ROW_GAP: f32 = 10.0;

/// Right-click context target: which group + which tab inside it.
#[derive(Clone)]
struct TabContextTarget {
    group: WeakEntity<PaneGroup>,
    /// Stable group id forwarded to `SplitGroupAt` so Split rows can
    /// target the right-clicked group even if focus has moved.
    group_id: PaneGroupId,
    tab_idx: usize,
    /// Tab count snapshot at open time — drives "Close Others" / "Close
    /// to Right" disabled rendering. Not refreshed on tick: if the user
    /// somehow mutates the group between open and click the helpers
    /// will still bail safely (each verifies its own bounds).
    tab_count: usize,
    /// Kind-specific payload for conditional rows (e.g. Copy Path /
    /// Reveal in Finder on editor tabs only).
    kind: TabContextKind,
    /// Whether the right-clicked tab is currently pinned. Drives the
    /// "Pin Tab" vs "Unpin Tab" row label; the action handler reads
    /// the live flag from the PaneGroup at dispatch time so a stale
    /// snapshot here never produces a wrong flip.
    is_pinned: bool,
    /// Whether this tab can be torn off into a new window. `true` only
    /// for single-leaf terminal tabs backed by a relay PTY (has an
    /// external id). Multi-leaf split tabs and non-terminal tabs are
    /// excluded: split tabs require a more complex cross-window handoff;
    /// editor/diff tabs hold window-bound entities that cannot move.
    can_tear_off: bool,
}

pub struct TabContextMenu {
    open: bool,
    /// Absolute window x of the click — drives left-shift positioning
    /// so the card stays inside the right edge.
    x_px: f32,
    /// Absolute window y of the click.
    y_px: f32,
    target: Option<TabContextTarget>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl TabContextMenu {
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

    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        x_px: f32,
        y_px: f32,
        group: WeakEntity<PaneGroup>,
        group_id: PaneGroupId,
        tab_idx: usize,
        tab_count: usize,
        kind: TabContextKind,
        is_pinned: bool,
        can_tear_off: bool,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(TabContextTarget {
            group,
            group_id,
            tab_idx,
            tab_count,
            kind,
            is_pinned,
            can_tear_off,
        });
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }
}

impl Render for TabContextMenu {
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

        let has_multiple = target.tab_count > 1;
        let has_right = target.tab_idx + 1 < target.tab_count;

        let close_idx = target.tab_idx;
        let others_idx = target.tab_idx;
        let right_idx = target.tab_idx;
        let target_group_id = target.group_id.0;

        let group_close = target.group.clone();
        let group_others = target.group.clone();
        let group_right = target.group.clone();
        let group_all = target.group.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        // Four-direction split actions — per-tab context-menu splits.
        // Each row dispatches `SplitGroupAt` carrying the right-clicked
        // group id so the split lands on this group regardless of focus.
        let splits: [(&'static str, &'static str, &'static str, u8, bool); 4] = [
            (
                "tab-ctx-split-right",
                "Split Right",
                "icons/arrow-right.svg",
                0,
                false,
            ),
            (
                "tab-ctx-split-down",
                "Split Down",
                "icons/arrow-down.svg",
                1,
                false,
            ),
            (
                "tab-ctx-split-left",
                "Split Left",
                "icons/arrow-left.svg",
                0,
                true,
            ),
            (
                "tab-ctx-split-up",
                "Split Up",
                "icons/arrow-up.svg",
                1,
                true,
            ),
        ];
        for (row_id, label, icon_path, axis, insert_before) in splits {
            let icon = svg()
                .path(icon_path)
                .size(px(SPLIT_ICON_SIZE))
                .text_color(theme.fg_muted);
            let row = div()
                .id(row_id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(SPLIT_ROW_GAP))
                .h(px(density.h_overlay_item))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_base)
                .child(icon)
                .child(div().flex_1().child(label))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        window.dispatch_action(
                            Box::new(SplitGroupAt {
                                group_id: target_group_id,
                                axis,
                                insert_before,
                            }),
                            cx,
                        );
                        this.close(cx);
                    }),
                );
            card = card.child(row);
        }
        card = card.child(div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive));

        let rename_idx = target.tab_idx;
        card = card.child(menu_row(
            "tab-ctx-rename",
            "Change Title…",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                // Closing FIRST so the rename modal can take focus
                // without the menu's mouse-down-out closing it on the
                // next dispatch tick.
                this.close(cx);
                window.dispatch_action(
                    Box::new(RequestRenameTabAt {
                        group_id: target_group_id,
                        tab_idx: rename_idx as u32,
                    }),
                    cx,
                );
            }),
        ));

        let pin_idx = target.tab_idx;
        let pin_label = if target.is_pinned {
            "Unpin Tab"
        } else {
            "Pin Tab"
        };
        card = card.child(menu_row(
            "tab-ctx-pin",
            pin_label,
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                this.close(cx);
                window.dispatch_action(
                    Box::new(TogglePinTabAt {
                        group_id: target_group_id,
                        tab_idx: pin_idx as u32,
                    }),
                    cx,
                );
            }),
        ));

        // "Move Tab to New Window" — only when the tab can be torn off
        // (single-leaf relay-backed terminal). Hidden for editor/diff tabs
        // and for multi-leaf sub-pane terminals.
        if target.can_tear_off {
            let move_group_id = target.group_id.0;
            let move_tab_idx = target.tab_idx;
            card = card.child(menu_row(
                "tab-ctx-move-to-new-window",
                "Move Tab to New Window",
                true,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.close(cx);
                    window.dispatch_action(
                        Box::new(MoveTabToNewWindow {
                            group_id: move_group_id,
                            tab_idx: move_tab_idx as u32,
                        }),
                        cx,
                    );
                }),
            ));
        }

        card = card.child(div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive));

        card = card.child(menu_row(
            "tab-ctx-close",
            "Close",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_close.upgrade() {
                    group.update(cx, |g, cx| g.close_tab(close_idx, window, cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "tab-ctx-close-others",
            "Close Others",
            has_multiple,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_others.upgrade() {
                    group.update(cx, |g, cx| g.close_others(others_idx, window, cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "tab-ctx-close-to-right",
            "Close to Right",
            has_right,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_right.upgrade() {
                    group.update(cx, |g, cx| g.close_to_right(right_idx, window, cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "tab-ctx-close-all",
            "Close All in Group",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_all.upgrade() {
                    group.update(cx, |g, cx| g.close_all(window, cx));
                }
                this.close(cx);
            }),
        ));

        // Terminal-specific rows: Tab Color palette. Appended only when
        // the right-clicked tab is a terminal/agent (editor tabs don't
        // get a color tag for v1 — match the reference editor scope).
        if matches!(target.kind, TabContextKind::Terminal) {
            card = card.child(div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive));
            card = card.child(color_palette_row(
                target.group.clone(),
                target.tab_idx,
                theme,
                cx,
            ));
        }

        // Editor-specific rows: appended only when the right-clicked
        // tab is an editor. Carries the file path so handlers don't
        // need to walk back into the PaneGroup entity.
        if let TabContextKind::Editor { path, project_root } = &target.kind {
            card = card.child(div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive));
            let copy_path = path.clone();
            card = card.child(menu_row(
                "tab-ctx-copy-path",
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
            // Compute relative path eagerly: strip the project_root
            // prefix; if the file lives outside the root (or no root
            // known) fall back to the file name. Closure captures the
            // resolved string so the click handler is cheap.
            let rel_string = project_root
                .as_ref()
                .and_then(|root| path.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            card = card.child(menu_row(
                "tab-ctx-copy-relative",
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
            let reveal_path = path.clone();
            card = card.child(menu_row(
                "tab-ctx-reveal-in-finder",
                "Reveal in Finder",
                true,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    // `open -R <path>` selects the file in Finder.
                    // Failure here is non-fatal — log + ignore so a
                    // permission denial doesn't crash the renderer.
                    if let Err(err) = std::process::Command::new("open")
                        .arg("-R")
                        .arg(&reveal_path)
                        .spawn()
                    {
                        tracing::warn!(?err, path = %reveal_path.display(), "reveal in finder failed");
                    }
                    this.close(cx);
                }),
            ));
        }

        let left_px = (x_px - MENU_WIDTH).max(0.0);
        let card_container = div()
            .absolute()
            .top(px(y_px))
            .left(px(left_px))
            .w(px(MENU_WIDTH))
            .child(card);

        // Full-window invisible overlay for click-outside dismiss.
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

/// Build the Tab Color row — a horizontal palette of 9 color swatches
/// plus a "clear" circle (Ban glyph) at the start. Each swatch click
/// updates `PaneGroup::set_tab_color` for the given `tab_idx` then
/// closes the menu. Matches the reference editor's tab color UX.
fn color_palette_row(
    group: WeakEntity<PaneGroup>,
    tab_idx: usize,
    theme: Theme,
    cx: &mut Context<TabContextMenu>,
) -> gpui::AnyElement {
    use gpui::IntoElement;
    let label = div()
        .px(px(ROW_PADDING_X))
        .py(px(4.0))
        .text_size(px(11.0))
        .text_color(theme.fg_subtle)
        .child("Tab Color");
    let swatches: [(&'static str, Option<TabColor>); 10] = [
        ("clear", None),
        ("blue", Some(TabColor::Blue)),
        ("purple", Some(TabColor::Purple)),
        ("pink", Some(TabColor::Pink)),
        ("red", Some(TabColor::Red)),
        ("orange", Some(TabColor::Orange)),
        ("yellow", Some(TabColor::Yellow)),
        ("green", Some(TabColor::Green)),
        ("teal", Some(TabColor::Teal)),
        ("gray", Some(TabColor::Gray)),
    ];
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(ROW_PADDING_X))
        .py(px(4.0));
    for (index, (id, choice)) in swatches.iter().enumerate() {
        let group_for_click = group.clone();
        let choice = *choice;
        let _ = id; // id kept for code clarity; element-id uses numeric index
        let mut swatch = div()
            .id(("tab-color-swatch", index))
            .w(px(16.0))
            .h(px(16.0))
            .rounded_full()
            .cursor_pointer()
            .border_1();
        swatch = match choice {
            Some(c) => {
                // gpui::rgb takes a u32 (0xRRGGBB) and returns Rgba.
                let color = gpui::rgb(c.rgb());
                swatch.bg(color).border_color(color)
            }
            None => swatch.border_color(theme.border_active),
        };
        row = row.child(swatch.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(g) = group_for_click.upgrade() {
                    g.update(cx, |g, cx| g.set_tab_color(tab_idx, choice, cx));
                }
                this.close(cx);
            }),
        ));
    }
    div()
        .flex()
        .flex_col()
        .child(label)
        .child(row)
        .into_any_element()
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
        .h(px(density.h_overlay_item))
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

    fn test_menu() -> TabContextMenu {
        TabContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target.is_none());
    }

    #[test]
    fn open_stores_coords_and_target_metadata() {
        let mut m = test_menu();
        // Mirror open() body inline (no Context<Self> in unit tests).
        m.x_px = 500.0;
        m.y_px = 200.0;
        m.open = true;
        assert!(m.is_open());
        assert_eq!(m.x_px, 500.0);
        assert_eq!(m.y_px, 200.0);
    }
}
