//! Right-click context menu for the stash section — for a stash row and for
//! a file row inside an expanded stash.
//!
//! # Why this exists before it is convenient
//!
//! Every verb on a stash row used to be a visible text button, because
//! hiding one would have left Drop — destructive, irreversible in the UI —
//! with no way in. That is the constraint this menu lifts: with a guaranteed
//! non-hover path to every action, `row.rs` can hide its cluster until hover
//! and the row at rest is finally just a stash. The cluster and the menu are
//! therefore one change, not two; shipping the cluster alone would strand a
//! verb for anyone on a touchpad-less, hover-less or keyboard-driven path.
//!
//! # One entity, two shapes
//!
//! A stash row and a file row inside it open the same menu with different
//! items, the same way [`GitRowContextMenu`](crate::shell::git_panel) covers
//! a file, a folder and a multi-selection. Two entities would mean two mounts
//! at the workspace root, two close-on-open lists to keep in sync, and two
//! chances for both to be open at once.
//!
//! # Items whose phase has not landed are absent
//!
//! Branch from Stash, Rename, Open All Changes and Restore This File belong
//! to later phases. They are omitted rather than rendered disabled: a menu
//! item that does nothing when clicked is exactly the defect Drop shipped
//! with in Phase 3, and a greyed row still invites the click that teaches the
//! user the menu lies.
//!
//! Mounted as a child of `WorkspaceRoot` (same pattern as
//! `CommitContextMenu`) so close-on-outside-click and peer-overlay
//! close-on-open cycle through one shared overlay z-band. Opened via the
//! [`OpenStashContextMenuAt`](crate::actions::OpenStashContextMenuAt) action,
//! whose root-level handler closes peer overlays first.

use gpui::{
    ClipboardItem, Context, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;

use crate::shell::source_control::commit_context_menu::MENU_WIDTH;
use crate::shell::stash_panel::{DropStashRequested, StashPanel};
use crate::ui::FloatingSurface;

const ROW_PADDING_X: f32 = 10.0;

/// What the right-click landed on.
///
/// The stash arm carries a whole [`DropStashRequested`] rather than five
/// loose fields. That event needs every one of them — the confirm dialog's
/// copy names the stash, because an index tells the user nothing about what
/// is about to disappear — and every other item on the menu needs a subset,
/// so one struct keeps them from drifting apart.
///
/// `sha` is the identity every op resolves from: a row's painted `stash@{N}`
/// addresses a stack shared with every worktree and with the user's own
/// terminal, so it can name a different entry by the time a menu item is
/// clicked. The index rides along as a tiebreaker only; see
/// `stash_panel/ops.rs`.
#[derive(Debug, Clone)]
pub enum StashContextTarget {
    Stash(DropStashRequested),
    File { sha: String, path: PathBuf },
}

/// State of the open menu — owned by `WorkspaceRoot`.
///
/// `panel` is a weak handle: the source control panel re-creates its entities
/// on a workspace switch, and a strong handle here would keep a dead panel
/// alive and fire its ops into a view nobody is looking at. Every item
/// upgrades at click time and silently dismisses when the upgrade fails.
pub struct StashContextMenu {
    open: bool,
    x_px: f32,
    y_px: f32,
    target: Option<StashContextTarget>,
    panel: Option<WeakEntity<StashPanel>>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl StashContextMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            x_px: 0.0,
            y_px: 0.0,
            target: None,
            panel: None,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the menu at the cursor for one right-clicked target.
    pub fn open(
        &mut self,
        x_px: f32,
        y_px: f32,
        target: StashContextTarget,
        panel: WeakEntity<StashPanel>,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(target);
        self.panel = Some(panel);
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }
}

impl Render for StashContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(
            &mut self.theme,
            &mut self.density,
            &mut self.typography,
            cx,
        );
        let (true, Some(target)) = (self.open, self.target.clone()) else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;

        let card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg();
        let card = match target {
            StashContextTarget::Stash(stash) => self.stash_items(card, stash, cx),
            StashContextTarget::File { sha, path } => self.file_items(card, sha, path, cx),
        };

        // Right edge hugs the cursor, matching every peer menu so the eye-line
        // is the same wherever the user right-clicks. Clamped to 0 so a click
        // near the panel's left edge does not push the card off-screen.
        let card_container = div()
            .absolute()
            .top(px(self.y_px))
            .left(px((self.x_px - MENU_WIDTH).max(0.0)))
            .w(px(MENU_WIDTH))
            .child(card);

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(card_container)
            .into_any_element()
    }
}

impl StashContextMenu {
    /// The stash-row item set.
    fn stash_items(
        &self,
        mut card: gpui::Div,
        stash: DropStashRequested,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let sha = stash.sha.clone();
        let index = stash.stash_ref.index;

        // The address the row deliberately stopped painting. It drives no
        // decision at a glance, but once a menu is open the user is about to
        // act on a specific entry — and `stash@{N}` is the name they would
        // type in a terminal to act on the same one.
        card = card.child(
            div()
                .px(px(ROW_PADDING_X))
                .py(px(density.pad_row))
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(format!("stash@{{{index}}}")),
        );

        // ── 1. The three ways to bring a stash back. Apply first (leaves the
        //   entry), Pop second (consumes it), Apply with index third — it is
        //   Apply with one more promise, so it sits under the verb it refines
        //   rather than at the top competing with it.
        card = card.child(self.verb(
            "stash-ctx-apply",
            "Apply",
            theme.fg_base,
            {
                let sha = sha.clone();
                move |panel, cx| panel.apply(sha.clone(), index, false, cx)
            },
            cx,
        ));
        card = card.child(self.verb(
            "stash-ctx-pop",
            "Pop",
            theme.fg_base,
            {
                let sha = sha.clone();
                move |panel, cx| panel.pop(sha.clone(), index, cx)
            },
            cx,
        ));
        card = card.child(self.verb(
            "stash-ctx-apply-index",
            "Apply with index",
            theme.fg_base,
            {
                let sha = sha.clone();
                move |panel, cx| panel.apply(sha.clone(), index, true, cx)
            },
            cx,
        ));
        card = card.child(separator(theme));

        // ── 2. Clipboard. Copy Message is omitted, not disabled, when the
        //   stash carries none: the row paints "(no message)" as a stand-in
        //   for empty, and putting that stand-in on the clipboard would hand
        //   the user a string git never wrote.
        let trimmed = stash.message.trim().to_string();
        if !trimmed.is_empty() {
            card = card.child(menu_row(
                "stash-ctx-copy-message",
                "Copy Message",
                theme.fg_base,
                theme,
                density,
                typography.clone(),
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(trimmed.clone()));
                    this.close(cx);
                }),
            ));
        }
        let copy_sha = sha.clone();
        card = card.child(menu_row(
            "stash-ctx-copy-sha",
            "Copy SHA",
            theme.fg_base,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_sha.clone()));
                this.close(cx);
            }),
        ));
        card = card.child(separator(theme));

        // ── 3. Drop, alone below a rule and tinted. It does NOT call
        //   `drop_confirmed`: the panel never drops on its own, it emits and
        //   the host mounts the confirm dialog. Routing the menu through the
        //   same event is what keeps one confirm step for both entry points.
        let panel = self.panel.clone();
        card.child(menu_row(
            "stash-ctx-drop",
            "Drop…",
            theme.status_error,
            theme,
            density,
            typography,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(strong) = panel.as_ref().and_then(|p| p.upgrade()) {
                    let ev = stash.clone();
                    strong.update(cx, |_panel, cx| cx.emit(ev));
                }
                this.close(cx);
            }),
        ))
    }

    /// The file-row item set. Deliberately short: a file inside a stash is
    /// read-only until Phase 8 lands a restore.
    fn file_items(
        &self,
        card: gpui::Div,
        sha: String,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let panel = self.panel.clone();

        let open_path = path.clone();
        let open_sha = sha.clone();
        let card = card.child(menu_row(
            "stash-file-ctx-open",
            "Open Changes",
            theme.fg_base,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(strong) = panel.as_ref().and_then(|p| p.upgrade()) {
                    let (sha, path) = (open_sha.clone(), open_path.clone());
                    // The panel resolves `origin` from its own cached file
                    // list. It selects the revision the diff is read from and
                    // cannot be re-derived here.
                    strong.update(cx, |panel, cx| panel.request_file_diff(&sha, &path, cx));
                }
                this.close(cx);
            }),
        ));

        // The path git itself uses — repo-relative, forward slashes on every
        // platform. What the row paints is the same string split in two, so
        // copying reassembles what the user can see.
        let copy_path = path.to_string_lossy().replace('\\', "/");
        card.child(menu_row(
            "stash-file-ctx-copy-path",
            "Copy Relative Path",
            theme.fg_base,
            theme,
            density,
            typography,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                this.close(cx);
            }),
        ))
    }

    /// A row that runs one stash op through the weak panel handle and closes.
    ///
    /// Every op-dispatching item has the same three steps — upgrade, run,
    /// close — and the upgrade is the one that must not be forgotten: a menu
    /// still open across a workspace switch holds a handle to a panel that no
    /// longer renders.
    fn verb<F>(
        &self,
        id: &'static str,
        label: &'static str,
        fg: Hsla,
        run: F,
        cx: &mut Context<Self>,
    ) -> impl IntoElement
    where
        F: Fn(&mut StashPanel, &mut Context<StashPanel>) + 'static,
    {
        let panel = self.panel.clone();
        menu_row(
            id,
            label,
            fg,
            self.theme,
            self.density,
            self.typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if let Some(strong) = panel.as_ref().and_then(|p| p.upgrade()) {
                    strong.update(cx, |panel, cx| run(panel, cx));
                }
                this.close(cx);
            }),
        )
    }
}

/// Single menu row. Same shape as `CommitContextMenu::menu_row` so the menus
/// read as one component family; `fg` is the one addition, because Drop is
/// the first context-menu item in the cockpit that has to signal danger.
fn menu_row<H>(
    row_id: &'static str,
    label: &'static str,
    fg: Hsla,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
{
    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .h(px(density.h_overlay_item))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(fg)
        .cursor_pointer()
        // Not a red hover for Drop: a destructive tint on the hover
        // background conflicts with the focus cursor and reads as "the button
        // broke". Same call `oximux_ui::danger_ghost` makes.
        .hover(move |s| s.bg(theme.hover_overlay))
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label)
}

fn separator(theme: Theme) -> impl IntoElement {
    div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> StashContextMenu {
        StashContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn a_new_menu_is_closed_and_targets_nothing() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target.is_none());
        assert!(m.panel.is_none());
    }

    // The two shapes are told apart by the target, not by which entity was
    // opened — so the discriminant is worth pinning.
    #[test]
    fn the_two_targets_are_distinct() {
        let stash = StashContextTarget::Stash(DropStashRequested {
            stash_ref: oximux_core::StashRef { index: 0 },
            sha: "abc".into(),
            message: "wip".into(),
            relative: "2 hours ago".into(),
            branch: "main".into(),
        });
        let file = StashContextTarget::File {
            sha: "abc".into(),
            path: PathBuf::from("src/main.rs"),
        };
        assert!(matches!(stash, StashContextTarget::Stash(_)));
        assert!(matches!(file, StashContextTarget::File { .. }));
    }
}
