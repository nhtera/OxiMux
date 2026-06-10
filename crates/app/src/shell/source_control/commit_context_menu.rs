//! Right-click context menu for commit-graph rows.
//!
//! Four items in a single card: Cherry-pick / Revert / Copy SHA / Copy
//! short SHA. The first two dispatch through `CommitArea::cherry_pick` /
//! `CommitArea::revert` (single-flight on the same `in_flight` flag as
//! push/pull/commit, so a click during another op is a safe no-op); the
//! last two write directly to the clipboard.
//!
//! Mounted as a child of `WorkspaceRoot` (same pattern as
//! `FileTreeContextMenu` and `GitRowContextMenu`) so close-on-outside-
//! click and peer-overlay close-on-open cycle through one shared overlay
//! z-band. Opened via the `OpenCommitContextMenuAt` action; the
//! root-level handler closes peer overlays first and then calls
//! [`CommitContextMenu::open`].

use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;

use crate::shell::source_control::commit_area::CommitArea;
use crate::shell::source_control::commit_ops::{CommitVerb, run_commit_verb};

/// Width of the dropdown card. Matches `FileTreeContextMenu::MENU_WIDTH`
/// so the visual weight reads consistently across the cockpit.
pub const MENU_WIDTH: f32 = 200.0;
const ROW_PADDING_X: f32 = 10.0;

/// State of the open menu — owned by `WorkspaceRoot`. `commit_area` is a
/// weak handle so the menu can survive panel rebuilds (the source
/// control panel re-creates its entities on workspace switch); the
/// menu silently dismisses itself if the upgrade returns `None` at
/// fire time.
pub struct CommitContextMenu {
    open: bool,
    x_px: f32,
    y_px: f32,
    target_sha: String,
    short_sha: String,
    commit_area: Option<WeakEntity<CommitArea>>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl CommitContextMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            x_px: 0.0,
            y_px: 0.0,
            target_sha: String::new(),
            short_sha: String::new(),
            commit_area: None,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the menu at the cursor with the right-clicked commit's
    /// payload. `commit_area` is captured weak so the menu's dispatch
    /// closures can no-op silently if the source control panel was
    /// torn down between right-click and item click.
    pub fn open(
        &mut self,
        x_px: f32,
        y_px: f32,
        sha: String,
        short_sha: String,
        commit_area: WeakEntity<CommitArea>,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target_sha = sha;
        self.short_sha = short_sha;
        self.commit_area = Some(commit_area);
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }
}

impl Render for CommitContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let x_px = self.x_px;
        let y_px = self.y_px;
        let full_sha = self.target_sha.clone();
        let short_sha = self.short_sha.clone();
        let commit_area = self.commit_area.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg();

        // ── 1. History ops. Cherry-pick first (additive — most-asked-for
        //   power-user action); revert second (destructive-feeling even
        //   though it's a new-commit, not a force-rewrite). Both
        //   short-circuit when the panel was torn down between right-
        //   click and dispatch.
        let pick_sha = full_sha.clone();
        let pick_area = commit_area.clone();
        card = card.child(menu_row(
            "sc-commit-ctx-cherry-pick",
            "Cherry-pick",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(ref area) = pick_area
                    && let Some(strong) = area.upgrade()
                {
                    let sha = pick_sha.clone();
                    strong.update(cx, |a, cx| {
                        run_commit_verb(a, CommitVerb::CherryPick(sha), cx)
                    });
                }
                this.close(cx);
            }),
        ));
        let revert_sha = full_sha.clone();
        let revert_area = commit_area.clone();
        card = card.child(menu_row(
            "sc-commit-ctx-revert",
            "Revert",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(ref area) = revert_area
                    && let Some(strong) = area.upgrade()
                {
                    let sha = revert_sha.clone();
                    strong.update(cx, |a, cx| {
                        run_commit_verb(a, CommitVerb::Revert(sha), cx)
                    });
                }
                this.close(cx);
            }),
        ));
        card = card.child(separator(theme));

        // ── 2. Clipboard ops. Full SHA first (matches what
        //   `git rev-parse HEAD` returns); short SHA second (matches
        //   `git log --oneline` and the row's own SHA column).
        let copy_full = full_sha.clone();
        card = card.child(menu_row(
            "sc-commit-ctx-copy-sha",
            "Copy SHA",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_full.clone()));
                this.close(cx);
            }),
        ));
        let copy_short = short_sha.clone();
        card = card.child(menu_row(
            "sc-commit-ctx-copy-short-sha",
            "Copy short SHA",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_short.clone()));
                this.close(cx);
            }),
        ));

        // Anchor the card so its right edge hugs the cursor — matches
        // FileTreeContextMenu so users get the same eye-line for both
        // menus. Clamped to 0 so a right-click near the panel's left
        // edge doesn't push the card off-screen.
        let left_px = (x_px - MENU_WIDTH).max(0.0);
        let card_container = div()
            .absolute()
            .top(px(y_px))
            .left(px(left_px))
            .w(px(MENU_WIDTH))
            .child(card);

        // Full-window catcher dismisses on any outside click (left or
        // right). The inner card stops propagation implicitly because
        // its rows have their own MouseDown handlers.
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

/// Single menu row. Matches `FileTreeContextMenu::menu_row`'s shape so
/// the two menus feel like one component family at glance.
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

fn separator(theme: Theme) -> impl IntoElement {
    div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> CommitContextMenu {
        CommitContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target_sha.is_empty());
        assert!(m.short_sha.is_empty());
        assert!(m.commit_area.is_none());
    }
}
