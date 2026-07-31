//! Card builders + low-level menu_row primitives for the
//! [`crate::shell::git_panel::row_context_menu::GitRowContextMenu`]
//! entity. Split out so the menu's own module stays under the 500-LOC
//! warn cap — entity state + render dispatch live there; this file
//! owns the per-variant item lists.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::discard_confirm::DiscardAllArea;
use crate::shell::git_panel::row_context_menu::GitRowContextMenu;
use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};

pub(super) const ROW_PADDING_X: f32 = 10.0;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_single_card(
    mut card: gpui::Div,
    path: PathBuf,
    is_staged: bool,
    workdir: Option<&PathBuf>,
    panel: WeakEntity<GitPanel>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<GitRowContextMenu>,
) -> gpui::Div {
    // Panel rows carry repo-relative paths (porcelain v2 reports them
    // that way) and the git ops below want them that way. Anything
    // that touches the filesystem — open, reveal, copy-absolute —
    // needs the workdir rejoined first.
    let abs_path = absolute_path(&path, workdir);

    // ── 1. Open in editor — files only, and only while the file is
    //   still on disk. A deleted / staged-deletion row has nothing to
    //   open: the editor would mount a tab that reads "File not found
    //   on disk". Greyed out rather than silently inert so the row
    //   says why it does nothing. Matches `DiffView::open_file_in_editor`,
    //   which skips the same case.
    let open_path = abs_path.clone();
    card = card.child(menu_row(
        "git-row-ctx-open",
        "Open in editor",
        exists_on_disk(&abs_path),
        theme.fg_base,
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
    card = card.child(separator(theme));

    // ── 2. Clipboard ops.
    let copy_abs = abs_path.clone();
    card = card.child(menu_row(
        "git-row-ctx-copy-abs",
        "Copy absolute path",
        true,
        theme.fg_base,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(
                copy_abs.to_string_lossy().into_owned(),
            ));
            this.close(cx);
        }),
    ));
    if let Some(rel) = relative_path_string(&path, workdir) {
        card = card.child(menu_row(
            "git-row-ctx-copy-rel",
            "Copy relative path",
            !rel.is_empty(),
            theme.fg_base,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(rel.clone()));
                this.close(cx);
            }),
        ));
    }

    // ── 3. Reveal in Finder.
    let reveal_path = abs_path;
    card = card.child(menu_row(
        "git-row-ctx-reveal",
        "Reveal in Finder",
        true,
        theme.fg_base,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            spawn_reveal(&reveal_path);
            this.close(cx);
        }),
    ));
    card = card.child(separator(theme));

    // ── 4. Stage / Unstage. Mirrors the hover-button surface but
    //   accessible from any pointer position. Staged rows show
    //   Unstage; Unstaged + Untracked rows show Stage.
    let stage_label = if is_staged { "Unstage" } else { "Stage" };
    let stage_path = path.clone();
    let stage_panel = panel.clone();
    let stage_is_staged = is_staged;
    card = card.child(menu_row(
        "git-row-ctx-stage",
        stage_label,
        true,
        theme.fg_base,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            let p = stage_path.clone();
            this.close(cx);
            let _ = stage_panel.update(cx, |panel, cx| {
                if stage_is_staged {
                    panel.unstage_path(p, cx);
                } else {
                    panel.stage_path(p, cx);
                }
            });
        }),
    ));

    // ── 5. Discard… — opens the type-to-confirm discard dialog.
    //   Hidden for the Staged side because discarding a staged file
    //   should go through "Unstage" first (preserves the area-discard
    //   sequence the section header already implements). The modal
    //   copy (Delete / Discard / Restore) is resolved by `GitPanel`
    //   from the live `FileStatus`, so untracked rows automatically
    //   read "Delete" without us threading the section flag through
    //   here.
    if !is_staged {
        let discard_path = path;
        let discard_panel = panel;
        card = card.child(menu_row(
            "git-row-ctx-discard",
            "Discard\u{2026}",
            true,
            theme.status_error,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                let p = discard_path.clone();
                this.close(cx);
                let _ = discard_panel.update(cx, |panel, cx| {
                    panel.discard_path(p, cx);
                });
            }),
        ));
    }
    card
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_multi_card(
    mut card: gpui::Div,
    paths: Vec<PathBuf>,
    all_staged: bool,
    all_untracked: bool,
    panel: WeakEntity<GitPanel>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<GitRowContextMenu>,
) -> gpui::Div {
    let count = paths.len();

    // ── Stage / Unstage N selected. When the right-clicked row is
    //   from Staged we offer Unstage; otherwise Stage (the safer
    //   no-op default for any selected row that's already staged).
    let primary_label: SharedString = if all_staged {
        format!("Unstage {count} selected").into()
    } else {
        format!("Stage {count} selected").into()
    };
    let primary_paths = paths.clone();
    let primary_panel = panel.clone();
    let primary_is_unstage = all_staged;
    card = card.child(menu_row(
        "git-row-ctx-multi-primary",
        primary_label,
        true,
        theme.fg_base,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            let ps = primary_paths.clone();
            this.close(cx);
            let _ = primary_panel.update(cx, |panel, cx| {
                if primary_is_unstage {
                    panel.unstage_paths_bulk(ps, cx);
                } else {
                    panel.stage_paths_bulk(ps, cx);
                }
            });
        }),
    ));

    // ── Discard N selected. Routes through `discard_area` so the
    //   user gets the type-to-confirm modal. Area is inferred from
    //   the right-clicked row's section flags — pure-untracked →
    //   Untracked (uses `git clean`), pure-staged → Staged (unstage
    //   then discard), otherwise → Unstaged (plain `git restore`).
    let area = if all_untracked {
        DiscardAllArea::Untracked
    } else if all_staged {
        DiscardAllArea::Staged
    } else {
        DiscardAllArea::Unstaged
    };
    let discard_label: SharedString = format!("Discard {count} selected\u{2026}").into();
    let discard_paths = paths;
    let discard_panel = panel;
    card = card.child(menu_row(
        "git-row-ctx-multi-discard",
        discard_label,
        true,
        theme.status_error,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            let ps = discard_paths.clone();
            this.close(cx);
            let _ = discard_panel.update(cx, |panel, cx| {
                panel.discard_area(area, ps, cx);
            });
        }),
    ));
    card
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_folder_card(
    mut card: gpui::Div,
    leaves: Vec<PathBuf>,
    is_staged_section: bool,
    is_untracked_section: bool,
    panel: WeakEntity<GitPanel>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<GitRowContextMenu>,
) -> gpui::Div {
    if leaves.is_empty() {
        return card;
    }
    let count = leaves.len();

    // Primary: Stage all / Unstage all in folder. Mirrors the hover
    // cluster on folder rows in `tree_render::folder_hover_cluster`.
    let primary_label: SharedString = if is_staged_section {
        format!("Unstage all in folder ({count})").into()
    } else {
        format!("Stage all in folder ({count})").into()
    };
    let primary_leaves = leaves.clone();
    let primary_panel = panel.clone();
    let primary_is_unstage = is_staged_section;
    card = card.child(menu_row(
        "git-row-ctx-folder-primary",
        primary_label,
        true,
        theme.fg_base,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            let ls = primary_leaves.clone();
            this.close(cx);
            let _ = primary_panel.update(cx, |panel, cx| {
                if primary_is_unstage {
                    panel.unstage_paths_bulk(ls, cx);
                } else {
                    panel.stage_paths_bulk(ls, cx);
                }
            });
        }),
    ));

    // Destructive: Discard / Delete all in folder. Area follows the
    // section semantics so partial-stage rows + the unstage-first
    // sequence on Staged behave identically to the section-level
    // "Discard all" path.
    let area = if is_untracked_section {
        DiscardAllArea::Untracked
    } else if is_staged_section {
        DiscardAllArea::Staged
    } else {
        DiscardAllArea::Unstaged
    };
    let discard_label: SharedString = match area {
        DiscardAllArea::Untracked => format!("Delete all in folder ({count})\u{2026}").into(),
        _ => format!("Discard all in folder ({count})\u{2026}").into(),
    };
    let discard_leaves = leaves;
    let discard_panel = panel;
    card = card.child(menu_row(
        "git-row-ctx-folder-discard",
        discard_label,
        true,
        theme.status_error,
        theme,
        density,
        typography.clone(),
        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
            let ls = discard_leaves.clone();
            this.close(cx);
            let _ = discard_panel.update(cx, |panel, cx| {
                panel.discard_area(area, ls, cx);
            });
        }),
    ));
    card
}

/// Absolute on-disk path for a row. Panel paths arrive repo-relative,
/// so filesystem consumers (open in editor, Reveal in Finder, copy
/// absolute path) have to rejoin the workdir — a bare "test.txt"
/// resolves against the process CWD and reads as a missing file.
/// Already-absolute paths pass through so no caller can double-join.
pub(super) fn absolute_path(path: &Path, workdir: Option<&PathBuf>) -> PathBuf {
    match workdir {
        Some(root) if path.is_relative() => root.join(path),
        _ => path.to_path_buf(),
    }
}

/// Whether `path` is a file the editor can actually open. A row for a
/// deleted (or staged-deletion) path fails this — git still lists it,
/// but there is nothing on disk behind it.
pub(super) fn exists_on_disk(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// Compute the path relative to the repo workdir. Relative paths are
/// already in that form and pass through untouched. Falls back to the
/// file name when an absolute path sits outside the workdir (rare — a
/// symlinked file or stale state).
pub(super) fn relative_path_string(path: &Path, workdir: Option<&PathBuf>) -> Option<String> {
    if path.is_relative() {
        return Some(path.to_string_lossy().into_owned()).filter(|s| !s.is_empty());
    }
    let rel = workdir
        .and_then(|root| path.strip_prefix(root.as_path()).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
    rel.filter(|s| !s.is_empty())
}

/// Spawn `open -R <path>` and drop the child handle. Failures land in
/// tracing because Finder errors aren't actionable for the user.
fn spawn_reveal(path: &Path) {
    if let Err(err) = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
    {
        tracing::warn!(
            ?err,
            path = %path.display(),
            "reveal in finder failed"
        );
    }
}

pub(super) fn separator(theme: Theme) -> impl IntoElement {
    div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive)
}

/// Single low-level menu-row factory. Card builders call this
/// directly with `theme.fg_base` for normal items and
/// `theme.status_error` for destructive ones; the wrapper helpers
/// the original FileTreeContextMenu carries (menu_row /
/// menu_row_destructive / *_dynamic) are folded into one here to
/// stay under the 500-LOC warn cap.
#[allow(clippy::too_many_arguments)]
fn menu_row<H>(
    row_id: &'static str,
    label: impl Into<SharedString>,
    enabled: bool,
    fg_when_enabled: gpui::Hsla,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
{
    let fg = if enabled { fg_when_enabled } else { theme.fg_subtle };
    let mut row = div()
        .id(SharedString::from(row_id))
        .flex()
        .flex_row()
        .items_center()
        .h(px(density.h_overlay_item))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(fg)
        .child(label.into());
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

    /// An absolute path spelled for the running platform.
    ///
    /// `/repo` is not absolute on Windows — `Path::is_absolute` wants a prefix —
    /// so `relative_path_string` took its relative branch and passed the whole
    /// thing through, which is the *correct* behaviour for a relative path (see
    /// `relative_path_passes_panel_paths_through_whole`). Only the fixture was
    /// Unix-only. Nothing here touches the filesystem, so a synthetic drive is
    /// fine and `C:` need not exist.
    fn abs(rest: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", rest.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{rest}"))
        }
    }

    /// A repo-relative path in the separator the platform's `strip_prefix` +
    /// `to_string_lossy` will actually produce.
    fn rel(parts: &[&str]) -> String {
        parts
            .iter()
            .fold(PathBuf::new(), |acc, part| acc.join(part))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn relative_path_strips_workdir_prefix() {
        let workdir = abs("repo");
        let path = abs("repo/src/lib.rs");
        assert_eq!(
            relative_path_string(&path, Some(&workdir)),
            Some(rel(&["src", "lib.rs"]))
        );
    }

    #[test]
    fn relative_path_falls_back_to_basename_when_outside_workdir() {
        let workdir = abs("repo");
        let path = abs("elsewhere/notes.md");
        assert_eq!(
            relative_path_string(&path, Some(&workdir)).as_deref(),
            Some("notes.md")
        );
    }

    #[test]
    fn relative_path_falls_back_to_basename_with_no_workdir() {
        let path = abs("abs/src/lib.rs");
        assert_eq!(
            relative_path_string(&path, None).as_deref(),
            Some("lib.rs")
        );
    }

    #[test]
    fn relative_path_passes_panel_paths_through_whole() {
        // Panel rows are already repo-relative — the old workdir
        // strip_prefix missed and the basename fallback flattened
        // "src/lib.rs" to "lib.rs".
        let workdir = PathBuf::from("/repo");
        let path = PathBuf::from("src/lib.rs");
        assert_eq!(
            relative_path_string(&path, Some(&workdir)).as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn absolute_path_rejoins_workdir_for_panel_paths() {
        let workdir = PathBuf::from("/repo");
        assert_eq!(
            absolute_path(&PathBuf::from("test.txt"), Some(&workdir)),
            PathBuf::from("/repo/test.txt")
        );
        assert_eq!(
            absolute_path(&PathBuf::from("src/lib.rs"), Some(&workdir)),
            PathBuf::from("/repo/src/lib.rs")
        );
    }

    #[test]
    fn absolute_path_never_double_joins() {
        let workdir = PathBuf::from("/repo");
        let already_absolute = PathBuf::from("/repo/src/lib.rs");
        assert_eq!(
            absolute_path(&already_absolute, Some(&workdir)),
            already_absolute
        );
    }

    #[test]
    fn absolute_path_is_identity_without_workdir() {
        let path = PathBuf::from("test.txt");
        assert_eq!(absolute_path(&path, None), path);
    }

    #[test]
    fn exists_on_disk_gates_open_in_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("live.txt");
        std::fs::write(&file, "x").expect("write");
        assert!(exists_on_disk(&file));
        // Deleted row: git still lists the path, disk doesn't have it.
        assert!(!exists_on_disk(&dir.path().join("gone.txt")));
        // A directory is not something the editor can open either.
        assert!(!exists_on_disk(dir.path()));
    }
}
