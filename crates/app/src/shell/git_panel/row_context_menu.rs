//! Right-click context menu for git_panel file rows. Mirrors the
//! [`crate::shell::file_tree_context_menu`] shape: one shared entity
//! owned by `WorkspaceRoot`, opened with cursor coords + a target
//! scope, closed on any outside click via an absolute overlay.
//!
//! Three scope variants live in [`GitRowContextTarget`]:
//! - **Single** — right-click on one row. Items: Open in editor /
//!   Copy absolute path / Copy relative path / Reveal in Finder /
//!   Stage or Unstage / Discard… (Discard is hidden for the
//!   already-staged side because the area-discard sequence belongs
//!   to the Staged-section header).
//! - **Multi** — right-click on a row that's part of a >1 multi-select.
//!   Labels pluralise ("Stage 3 selected", "Discard 3 selected…") and
//!   dispatch over the entire selection set via the panel's bulk
//!   methods.
//! - **Folder** (tree-mode only) — right-click on a folder row. Items:
//!   Stage all in folder / Unstage all in folder / Discard all in
//!   folder.
//!
//! Mutating ops route through the captured [`gpui::WeakEntity`] handle
//! to [`crate::shell::git_panel::GitPanel`], so the menu doesn't need
//! workspace-level Stage/Unstage/Discard actions. Clipboard +
//! Reveal in Finder run inline. Open in editor reuses the existing
//! [`crate::actions::OpenFileFromContextMenu`] dispatch.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::row_context_menu_items::{
    build_folder_card, build_multi_card, build_single_card,
};
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;
use std::path::PathBuf;

/// Card width. Slightly wider than `FileTreeContextMenu::MENU_WIDTH`
/// because multi-select labels carry a count suffix
/// ("Stage 12 selected") that would otherwise wrap.
pub const MENU_WIDTH: f32 = 220.0;

/// What the menu is targeting at open time. Drives which items render
/// and how their click handlers dispatch through the captured
/// `WeakEntity<GitPanel>`.
#[derive(Clone, Debug)]
pub enum GitRowContextTarget {
    /// Single-row right-click. `is_staged` drives the Stage-vs-Unstage
    /// label + the hide-Discard-on-Staged rule. Untracked-vs-modified
    /// distinction is left to the discard dialog (the modal copy is
    /// resolved from the live `FileStatus` at click time), so the
    /// untracked flag doesn't need to ride through the menu scope.
    Single { path: PathBuf, is_staged: bool },
    /// Multi-select right-click. The clicked row is part of a >1
    /// selection — items pluralise and dispatch over the whole set.
    /// `all_staged` / `all_untracked` are sourced from the
    /// right-clicked row's section (not a homogeneity check across
    /// the selection); when the selection spans sections, the
    /// right-clicked row's section wins for dispatch.
    Multi {
        paths: Vec<PathBuf>,
        all_staged: bool,
        all_untracked: bool,
    },
    /// Tree-mode folder right-click. Folder ops act over the
    /// pre-computed `leaves` list from
    /// [`crate::shell::source_control::tree::subtree_leaves`].
    Folder {
        leaves: Vec<PathBuf>,
        is_staged_section: bool,
        is_untracked_section: bool,
    },
}

pub struct GitRowContextMenu {
    open: bool,
    x_px: f32,
    y_px: f32,
    target: Option<GitRowContextTarget>,
    /// Captured at `open` time so click handlers can dispatch
    /// `stage_path` / `unstage_path` / `discard_path` etc. Cleared on
    /// `close`.
    panel: Option<WeakEntity<GitPanel>>,
    /// Repo workdir for the active git surface. Used by the "Copy
    /// relative path" item; absent → relative path falls back to the
    /// path basename.
    workdir: Option<PathBuf>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl GitRowContextMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            x_px: 0.0,
            y_px: 0.0,
            target: None,
            panel: None,
            workdir: None,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the menu at cursor coords with the given target scope.
    /// `panel` is the weak handle used by click dispatch; `workdir`
    /// is the repo root used for the relative-path item.
    pub fn open(
        &mut self,
        x_px: f32,
        y_px: f32,
        target: GitRowContextTarget,
        panel: WeakEntity<GitPanel>,
        workdir: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(target);
        self.panel = Some(panel);
        self.workdir = workdir;
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.target = None;
        self.panel = None;
        self.workdir = None;
        cx.notify();
    }
}

impl Render for GitRowContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let (Some(target), Some(panel)) = (self.target.clone(), self.panel.clone()) else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let workdir = self.workdir.clone();
        let x_px = self.x_px;
        let y_px = self.y_px;

        let card_base = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg();

        let card = match target {
            GitRowContextTarget::Single { path, is_staged } => build_single_card(
                card_base,
                path,
                is_staged,
                workdir.as_ref(),
                panel,
                theme,
                density,
                &typography,
                cx,
            ),
            GitRowContextTarget::Multi {
                paths,
                all_staged,
                all_untracked,
            } => build_multi_card(
                card_base,
                paths,
                all_staged,
                all_untracked,
                panel,
                theme,
                density,
                &typography,
                cx,
            ),
            GitRowContextTarget::Folder {
                leaves,
                is_staged_section,
                is_untracked_section,
            } => build_folder_card(
                card_base,
                leaves,
                is_staged_section,
                is_untracked_section,
                panel,
                theme,
                density,
                &typography,
                cx,
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> GitRowContextMenu {
        GitRowContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target.is_none());
        assert!(m.panel.is_none());
    }

    #[test]
    fn target_single_carries_staged_flag() {
        let t = GitRowContextTarget::Single {
            path: PathBuf::from("src/lib.rs"),
            is_staged: true,
        };
        match t {
            GitRowContextTarget::Single { is_staged, .. } => {
                assert!(is_staged);
            }
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn target_multi_preserves_paths_order() {
        let paths = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let t = GitRowContextTarget::Multi {
            paths: paths.clone(),
            all_staged: false,
            all_untracked: false,
        };
        match t {
            GitRowContextTarget::Multi { paths: p, .. } => assert_eq!(p, paths),
            _ => panic!("expected Multi"),
        }
    }

    #[test]
    fn target_folder_keeps_leaves_and_section() {
        let leaves = vec![PathBuf::from("dir/a"), PathBuf::from("dir/b")];
        let t = GitRowContextTarget::Folder {
            leaves: leaves.clone(),
            is_staged_section: false,
            is_untracked_section: true,
        };
        match t {
            GitRowContextTarget::Folder {
                leaves: l,
                is_staged_section,
                is_untracked_section,
            } => {
                assert_eq!(l, leaves);
                assert!(!is_staged_section);
                assert!(is_untracked_section);
            }
            _ => panic!("expected Folder"),
        }
    }
}
