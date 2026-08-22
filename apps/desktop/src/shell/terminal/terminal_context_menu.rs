//! Terminal GRID right-click context menu — Copy / Paste / Select All /
//! Clear / link actions / send-to-agent / split / tab ops.
//!
//! Mirrors `TabContextMenu`: one shared entity owned by `WorkspaceRoot`,
//! opened via the `OpenTerminalContextMenuAt` payload action. The menu holds
//! a `WeakEntity<TerminalView>` so grid operations (copy, paste, clear, …)
//! call the right view directly even across splits — the documented
//! overlay-action-dispatch fragility (`oximux-overlay-dispatch-needs-root-
//! fallback`) is sidestepped by mutating the view, not dispatching down into
//! it. Genuine tab/split operations (Split, Set Title, Close Tab) dispatch
//! workspace-level actions up the focused element path, which DOES resolve.

use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{CloseTab, RequestRenameTabAt, SplitSubPaneDown, SplitSubPaneRight};
use crate::shell::terminal_view::TerminalView;
use crate::ui::FloatingSurface;

/// Width of the dropdown card. Slightly wider than the tab menu to fit the
/// "Send Last Output to Agent" row plus its shortcut hint.
pub const MENU_WIDTH: f32 = 248.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;

/// What the menu acts on: the right-clicked terminal view plus the resolved
/// (group, tab) location (for tab-scoped rows) and the click-time selection /
/// link state that drives conditional rows.
#[derive(Clone)]
struct TerminalContextTarget {
    /// The right-clicked view. Weak so the menu never keeps a torn-down pane
    /// alive; rows upgrade-or-bail.
    view: WeakEntity<TerminalView>,
    /// Group id of the tab hosting the view — forwarded to `RequestRenameTabAt`
    /// so "Set Title…" targets the right tab regardless of focus.
    group_id: u64,
    /// Tab index of the hosting tab within its group.
    tab_idx: u32,
    /// Whether a selection was active at right-click time (drives the Copy /
    /// Send-Selection enabled state). Auto-word-select on the view side
    /// guarantees this is usually true.
    has_selection: bool,
    /// The link token under the cursor (URL or `path:line:col`), if any. Drives
    /// the Open Link / Copy Link rows.
    link: Option<String>,
}

pub struct TerminalContextMenu {
    open: bool,
    /// Absolute window x of the click — drives left-shift positioning so the
    /// card stays inside the right edge.
    x_px: f32,
    /// Absolute window y of the click.
    y_px: f32,
    target: Option<TerminalContextTarget>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl TerminalContextMenu {
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
        view: WeakEntity<TerminalView>,
        group_id: u64,
        tab_idx: u32,
        has_selection: bool,
        link: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(TerminalContextTarget {
            view,
            group_id,
            tab_idx,
            has_selection,
            link,
        });
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        cx.notify();
    }
}

impl Render for TerminalContextMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
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
        let has_sel = target.has_selection;

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg();

        let divider =
            || div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive);

        // --- Clipboard rows -------------------------------------------------
        let view_copy = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-copy",
            "Copy",
            Some("⌘C"),
            has_sel,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(v) = view_copy.upgrade() {
                    v.update(cx, |v, cx| {
                        v.copy_selection(cx);
                    });
                }
                this.close(cx);
            }),
        ));

        let view_paste = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-paste",
            "Paste",
            Some("⌘V"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(v) = view_paste.upgrade() {
                    v.update(cx, |v, cx| v.paste_from_clipboard(cx));
                }
                this.close(cx);
            }),
        ));

        let view_paste_text = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-paste-text",
            "Paste Text",
            None,
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(v) = view_paste_text.upgrade() {
                    v.update(cx, |v, cx| v.paste_text(cx));
                }
                this.close(cx);
            }),
        ));

        let view_select_all = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-select-all",
            "Select All",
            Some("⌘A"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(v) = view_select_all.upgrade() {
                    v.update(cx, |v, cx| v.select_all(cx));
                }
                this.close(cx);
            }),
        ));

        // --- Link rows (only when right-clicked on a link) ------------------
        if let Some(link) = target.link.clone() {
            card = card.child(divider());
            let view_open = target.view.clone();
            let link_open = link.clone();
            card = card.child(menu_row(
                "term-ctx-open-link",
                "Open Link",
                None,
                true,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    if let Some(v) = view_open.upgrade() {
                        let s = link_open.clone();
                        v.update(cx, |v, cx| v.open_link_string(&s, window, cx));
                    }
                    this.close(cx);
                }),
            ));
            let link_copy = link.clone();
            card = card.child(menu_row(
                "term-ctx-copy-link",
                "Copy Link",
                None,
                true,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(link_copy.clone()));
                    this.close(cx);
                }),
            ));
        }

        // --- Search + send-to-agent rows ------------------------------------
        card = card.child(divider());
        let view_search = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-search",
            "Search…",
            Some("⌘F"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(v) = view_search.upgrade() {
                    v.update(cx, |v, cx| v.open_search(cx));
                }
                this.close(cx);
            }),
        ));

        let view_send_sel = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-send-selection",
            "Send Selection to Agent",
            Some("⌘⇧I"),
            has_sel,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(v) = view_send_sel.upgrade() {
                    v.update(cx, |v, cx| v.send_selection_to_agent(window, cx));
                }
                this.close(cx);
            }),
        ));

        let view_send_out = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-send-output",
            "Send Last Output to Agent",
            Some("⌘⇧O"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(v) = view_send_out.upgrade() {
                    v.update(cx, |v, cx| v.send_last_output_to_agent(window, cx));
                }
                this.close(cx);
            }),
        ));

        // --- Split rows -----------------------------------------------------
        card = card.child(divider());
        card = card.child(menu_row(
            "term-ctx-split-right",
            "Split Right",
            Some("⌘D"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(SplitSubPaneRight), cx);
                this.close(cx);
            }),
        ));
        card = card.child(menu_row(
            "term-ctx-split-down",
            "Split Down",
            Some("⌘⇧D"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(SplitSubPaneDown), cx);
                this.close(cx);
            }),
        ));

        // --- Tab rows -------------------------------------------------------
        card = card.child(divider());
        let rename_group = target.group_id;
        let rename_tab = target.tab_idx;
        card = card.child(menu_row(
            "term-ctx-set-title",
            "Set Title…",
            None,
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                // Close FIRST so the rename modal can take focus without the
                // dismiss-overlay's mouse-down closing it on the next tick.
                this.close(cx);
                window.dispatch_action(
                    Box::new(RequestRenameTabAt {
                        group_id: rename_group,
                        tab_idx: rename_tab,
                    }),
                    cx,
                );
            }),
        ));

        let view_clear = target.view.clone();
        card = card.child(menu_row(
            "term-ctx-clear",
            "Clear",
            None,
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(v) = view_clear.upgrade() {
                    v.update(cx, |v, cx| v.clear_terminal(cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "term-ctx-close-tab",
            "Close Tab",
            Some("⌘W"),
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                // The right-clicked grid belongs to the visible (active) tab,
                // so `CloseTab` (focused-tab close) targets the right one.
                window.dispatch_action(Box::new(CloseTab), cx);
                this.close(cx);
            }),
        ));

        // Smart placement: open down-right from the cursor, but flip UP when a
        // bottom-edge click would clip the card, and flip LEFT near the right
        // edge — so a right-click anywhere in the grid shows the whole menu
        // (the naive top-left-at-cursor placement clipped it off the bottom).
        // Height is estimated from the row + divider count (the link rows add
        // two rows + a divider) against the live density metrics.
        let has_link = target.link.is_some();
        let (rows, dividers) = if has_link { (14.0, 4.0) } else { (12.0, 3.0) };
        // Each divider is 1px rule + 4px top/bottom margin = 9px; card adds
        // pad_overlay top and bottom.
        let est_height =
            2.0 * density.pad_overlay + rows * density.h_overlay_item + dividers * 9.0;
        let viewport = window.viewport_size();
        let vp_h = f32::from(viewport.height);
        let vp_w = f32::from(viewport.width);
        let top_px = if y_px + est_height > vp_h {
            // Not enough room below — anchor the card's bottom at the cursor,
            // clamped so it never runs off the top either.
            (y_px - est_height).max(0.0)
        } else {
            y_px
        };
        let left_px = if x_px + MENU_WIDTH > vp_w {
            (x_px - MENU_WIDTH).max(0.0)
        } else {
            x_px
        };
        let card_container = div()
            .absolute()
            .top(px(top_px))
            .left(px(left_px))
            .w(px(MENU_WIDTH))
            .child(card);

        // Full-window invisible overlay for click-outside dismiss.
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

/// One menu row: a label, an optional right-aligned shortcut hint, hover +
/// click when enabled. A disabled row renders muted and inert.
#[allow(clippy::too_many_arguments)]
fn menu_row<H>(
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
        .gap(px(12.0))
        .h(px(density.h_overlay_item))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(fg)
        .child(div().flex_1().child(label));
    if let Some(sc) = shortcut {
        row = row.child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(sc),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> TerminalContextMenu {
        TerminalContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target.is_none());
    }

    #[test]
    fn open_stores_coords() {
        let mut m = test_menu();
        // Mirror open() body inline (no Context<Self> in unit tests).
        m.x_px = 400.0;
        m.y_px = 150.0;
        m.open = true;
        assert!(m.is_open());
        assert_eq!(m.x_px, 400.0);
        assert_eq!(m.y_px, 150.0);
    }
}
