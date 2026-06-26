//! Per-row rendering for the changed-files list.
//!
//! Split from `changed_files.rs` so the main module stays under the
//! 500-LOC warn cap as Phase 02 layers three new decorations onto rows:
//! - `+A −B` line counts vs HEAD (greenish / reddish)
//! - rename arrow rendered as `old → new`, with `old` dimmed
//! - one-line conflict-kind sub-label under the file name (e.g.
//!   "both modified") for unmerged rows
//!
//! Each helper is independent and returns `Option<AnyElement>` where
//! "no decoration" is a legal outcome — the row builder concatenates
//! the live ones and the row layout has no holes when, say, a regular
//! modified row carries no conflict sub-label.

use crate::shell::file_explorer::file_icon::icon_for_name;
use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::changed_files::{RenderCtx, RowKindForActions};
use crate::shell::source_control::style as sc_style;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{Icon, IconName};
use oximux_core::{ConflictKind, FileStatus, IndexStatus, RenameInfo, WorktreeStatus};
use oximux_settings::{Theme, Typography};

/// Which SCM section a row was sourced from. Drives the status badge
/// letter + colour and the action-cluster routing. Kept private —
/// external consumers (e.g. `row_actions::action_cluster`) take the
/// `RowKindForActions` mirror defined in `changed_files`.
#[derive(Copy, Clone)]
pub(super) enum RowKind {
    Staged,
    Unstaged,
    Untracked,
}

impl From<RowKind> for RowKindForActions {
    fn from(k: RowKind) -> Self {
        match k {
            RowKind::Staged => RowKindForActions::Staged,
            RowKind::Unstaged => RowKindForActions::Unstaged,
            RowKind::Untracked => RowKindForActions::Untracked,
        }
    }
}

/// One-letter status badge plus the theme colour function for it. The
/// staged side falls back to "M" when the index is `Unmodified` — that
/// happens on partial-stage rows mirrored into the Staged section.
fn status_badge_for(f: &FileStatus, kind: RowKind) -> (&'static str, fn(&Theme) -> gpui::Hsla) {
    match kind {
        RowKind::Staged => match f.index {
            IndexStatus::Added => ("A", |t| t.git.added),
            IndexStatus::Deleted => ("D", |t| t.git.deleted),
            IndexStatus::Renamed => ("R", |t| t.git.renamed),
            IndexStatus::Copied => ("C", |t| t.git.copied),
            IndexStatus::Modified => ("M", |t| t.git.modified),
            IndexStatus::Untracked => ("U", |t| t.git.untracked),
            IndexStatus::Unmerged => ("!", |t| t.status_error),
            IndexStatus::Ignored => ("I", |t| t.git.ignored),
            IndexStatus::Unmodified => ("M", |t| t.git.modified),
        },
        RowKind::Unstaged => match f.worktree {
            WorktreeStatus::Deleted => ("D", |t| t.git.deleted),
            WorktreeStatus::Modified => ("M", |t| t.git.modified),
            WorktreeStatus::Renamed => ("R", |t| t.git.renamed),
            WorktreeStatus::Untracked => ("U", |t| t.git.untracked),
            WorktreeStatus::Unmerged => ("!", |t| t.status_error),
            WorktreeStatus::Ignored => ("I", |t| t.git.ignored),
            WorktreeStatus::Unmodified => ("M", |t| t.git.modified),
        },
        RowKind::Untracked => ("U", |t| t.git.untracked),
    }
}

/// `+A` (green) and `−B` (red) fragments, side by side with a tight gap.
///
/// Returns `None` when there's nothing meaningful to show — `line_counts`
/// is `None` (no HEAD / binary / pre-numstat) or both columns are zero
/// (mode-only or whitespace-only change). The caller is expected to
/// suppress the entire slot in that case rather than render an empty
/// span that would still consume layout width.
pub fn render_diff_counts(
    line_counts: Option<(u32, u32)>,
    theme: &Theme,
    typography: &Typography,
) -> Option<AnyElement> {
    let (added, removed) = line_counts?;
    if added == 0 && removed == 0 {
        return None;
    }
    // Use `−` (U+2212 minus sign), not ASCII `-`. Matches typographic
    // convention and aligns visually with the `+` width better than `-`.
    let mut row = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(sc_style::LINE_COUNT_GAP))
        .flex_shrink_0()
        .text_size(px(sc_style::GRAPH_META_TEXT))
        .font_weight(typography.w_medium);
    if added > 0 {
        row = row.child(
            div()
                .text_color(theme.status_added)
                .child(format!("+{added}")),
        );
    }
    if removed > 0 {
        row = row.child(
            div()
                .text_color(theme.status_removed)
                .child(format!("−{removed}")),
        );
    }
    Some(row.into_any_element())
}

/// File-name span. For ordinary rows: bare name. For rename rows:
/// `oldName → newName` with `oldName` dimmed via `fg_subtle`.
///
/// The arrow is U+2192 (`→`). On a renamed row we still display the
/// *new* name in the primary text colour so the eye lands on the file
/// the row actually represents.
pub fn render_name_with_rename(
    file_name: &str,
    rename: Option<&RenameInfo>,
    theme: &Theme,
    typography: &Typography,
) -> AnyElement {
    if let Some(info) = rename {
        let old_name = info
            .orig_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| info.orig_path.display().to_string());
        return div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(4.0))
            .flex_shrink_0()
            .child(
                div()
                    .text_color(theme.fg_subtle)
                    .font_weight(typography.w_medium)
                    .child(old_name),
            )
            // Arrow sits at `fg_muted` — one step brighter than the dim
            // `oldName` so the directional cue reads at a glance, one
            // step muted from `fg_base` so the row's centre of gravity
            // remains the new file name.
            .child(div().text_color(theme.fg_muted).child("→"))
            .child(
                div()
                    .text_color(theme.fg_base)
                    .font_weight(typography.w_medium)
                    .child(file_name.to_string()),
            )
            .into_any_element();
    }
    div()
        .flex_shrink_0()
        .text_color(theme.fg_base)
        .font_weight(typography.w_medium)
        .child(file_name.to_string())
        .into_any_element()
}

/// 14px-tall muted conflict-kind label, e.g. "both modified". Returns
/// `None` for non-conflict rows so the caller can omit the slot
/// entirely (no zero-height div, no layout jitter).
pub fn render_conflict_sub_label(kind: Option<ConflictKind>, theme: &Theme) -> Option<AnyElement> {
    let kind = kind?;
    Some(
        div()
            .text_size(px(sc_style::SUB_LABEL_TEXT))
            .text_color(theme.fg_subtle)
            .child(kind.label())
            .into_any_element(),
    )
}

/// Build one row: status badge / icon / name (+ optional rename arrow) /
/// optional conflict sub-label / optional `+A −B` counts / parent path /
/// hover-action cluster. Owned here so `changed_files.rs` stays under the
/// 500-LOC warn cap.
pub(super) fn row(
    f: &FileStatus,
    kind: RowKind,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> impl IntoElement {
    let is_selected = rctx.selected.contains(&f.path);
    let bg = if is_selected {
        rctx.theme.selection
    } else {
        rctx.theme.bg_panel
    };
    let theme = rctx.theme;
    let click_path = f.path.clone();
    let click_staged = matches!(kind, RowKind::Staged);
    let click_untracked = matches!(kind, RowKind::Untracked);
    let click_should_open_editor = !matches!(f.worktree, WorktreeStatus::Deleted)
        && !matches!(f.index, IndexStatus::Deleted);

    let file_name = f
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| f.path.display().to_string());
    let parent = f
        .path
        .parent()
        .and_then(|p| {
            let s = p.display().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
        .unwrap_or_default();

    let (badge_text, badge_color_fn) = status_badge_for(f, kind);
    let badge_color = badge_color_fn(&rctx.theme);

    let icon_el: gpui::AnyElement = match icon_for_name(&file_name) {
        Some(path) => svg()
            .path(path)
            .size(px(14.0))
            .text_color(badge_color)
            .into_any_element(),
        None => Icon::new(IconName::File)
            .size_3()
            .text_color(badge_color)
            .into_any_element(),
    };

    let row_id = format!(
        "git-row-{}-{}",
        match kind {
            RowKind::Staged => "s",
            RowKind::Unstaged => "u",
            RowKind::Untracked => "n",
        },
        f.path.display()
    );

    let is_discarding = rctx.in_flight_discards.contains(&f.path);
    let action_cluster = crate::shell::git_panel::row_actions::action_cluster(
        f.path.clone(),
        kind.into(),
        gpui::SharedString::from(row_id.clone()),
        is_discarding,
        cx,
    );

    let name_el = render_name_with_rename(&file_name, f.rename.as_ref(), &theme, rctx.typography);
    let sub_label = render_conflict_sub_label(f.conflict_kind, &theme);
    // Each section shows its own side of a partial stage: the Staged row
    // the index-vs-HEAD counts, the Changes/Untracked rows the
    // worktree-vs-index (or whole-file) counts.
    let section_counts = match kind {
        RowKind::Staged => f.staged_line_counts,
        RowKind::Unstaged | RowKind::Untracked => f.line_counts,
    };
    let diff_counts = render_diff_counts(section_counts, &theme, rctx.typography);

    // Name-and-meta cluster: file name (+ rename arrow) and the parent
    // directory share one baseline row; the conflict sub-label, when
    // present, hangs underneath as a second line.
    let name_row = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(6.0))
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .child(name_el)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                // Ellipsis (…) rather than a hard clip so a deep path reads as
                // truncated, not broken mid-word, when the panel is narrow.
                .truncate()
                .text_size(px(sc_style::GRAPH_META_TEXT))
                .text_color(rctx.theme.fg_subtle)
                .child(parent),
        );

    let name_block = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .child(name_row)
        .children(sub_label);

    // Bump the row height a touch when a sub-label is present so the
    // second line doesn't crowd the parent path. `density.h_row + 2`
    // is the existing single-line height; sub-label rows add 12px
    // (sub-label font-size + the 2px gap above).
    let has_sub_label = f.conflict_kind.is_some();
    let row_height = rctx.density.h_row
        + 2.0
        + if has_sub_label { 12.0 } else { 0.0 };

    let right_click_path = f.path.clone();
    div()
        .id(gpui::SharedString::from(row_id.clone()))
        .group(gpui::SharedString::from(row_id.clone()))
        .flex()
        .items_center()
        .gap(px(rctx.density.gap_inline))
        .h(px(row_height))
        .px(px(sc_style::PAD_H))
        .bg(bg)
        .text_size(px(sc_style::BODY_TEXT))
        .text_color(rctx.theme.fg_base)
        .when(!is_selected, |s| s.hover(|s| s.bg(theme.hover_overlay)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, ev: &MouseDownEvent, window, cx| {
                // Cmd-click (macOS `platform`) toggles a row in/out of
                // the selection — additive. Shift+Click replaces with
                // the inclusive range from the anchor to this row.
                // Bare click replaces with a single-row selection AND
                // is the only modifier path that opens the diff tab —
                // additive / range operations are about building a
                // multi-row set, not navigating.
                if ev.modifiers.platform {
                    panel.toggle_selection(click_path.clone(), cx);
                } else if ev.modifiers.shift {
                    panel.extend_range_to(click_path.clone(), cx);
                } else {
                    panel.select_only(click_path.clone(), cx);
                    if click_should_open_editor
                        && let Some(cb) = panel.on_open.clone()
                    {
                        (cb)(
                            click_path.clone(),
                            click_staged,
                            click_untracked,
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            }),
        )
        // Right-click — open the shared `GitRowContextMenu` with the
        // right scope variant. Multi-select aware: when the
        // right-clicked row is itself part of a >1 selection, the
        // selection set is bundled into `selection_paths` so the menu
        // pluralises labels and dispatches over the whole set. When
        // the right-clicked row is NOT in the current selection, the
        // menu opens in Single scope against just this row — the
        // standard list behaviour where right-clicking an unselected
        // row implicitly targets that row.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |panel, ev: &MouseDownEvent, window, cx| {
                let is_in_selection = panel.selected.contains(&right_click_path);
                let selection_paths: Vec<String> =
                    if is_in_selection && panel.selected.len() > 1 {
                        panel
                            .selected
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect()
                    } else {
                        Vec::new()
                    };
                let x: f32 = ev.position.x.into();
                let y: f32 = ev.position.y.into();
                window.dispatch_action(
                    Box::new(crate::actions::OpenGitRowContextMenuAt {
                        x,
                        y,
                        path: right_click_path.to_string_lossy().into_owned(),
                        is_staged: click_staged,
                        is_untracked: click_untracked,
                        is_folder: false,
                        selection_paths,
                        folder_leaves: Vec::new(),
                    }),
                    cx,
                );
                cx.stop_propagation();
            }),
        )
        .child(icon_el)
        .child(name_block)
        // `+A −B` slot, only when there's a number to show.
        .children(diff_counts)
        .child(
            // Right slot stacks badge + action cluster; group_hover toggles
            // which is visible. Same width budget as before.
            div()
                .relative()
                .w(px(38.0))
                .h(px(rctx.density.h_row))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(rctx.typography.t_label_caps))
                        .font_weight(rctx.typography.w_semibold)
                        .text_color(badge_color)
                        .group_hover(row_id.clone(), |s| s.invisible())
                        .child(badge_text),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_end()
                        .invisible()
                        .group_hover(row_id, |s| s.visible())
                        .child(action_cluster),
                ),
        )
}
