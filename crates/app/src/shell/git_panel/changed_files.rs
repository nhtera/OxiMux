//! Changed-files view — section routing + row rendering. Pure helpers; the
//! stateful entity lives in `super::GitPanel`. Row click → `panel.set_selected`.
//! Stage / unstage are dispatched via the workspace `StageFile` / `UnstageFile`
//! actions (Phase 2 step 8 routes them through `GitPanel`'s action handlers).

use crate::shell::file_explorer::file_icon::icon_for_name;
use crate::shell::git_panel::GitPanel;
use crate::shell::source_control::style as sc_style;
use gpui::{
    ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::Path;

/// Borrowed-slice sectioning of `GitState::files` into Staged / Unstaged /
/// Untracked. A partially-staged file (non-`Unmodified` on both sides) appears
/// in BOTH Staged and Unstaged — the conventional SCM dual-listing so a
/// partial stage stays visible in the unstaged column for the remaining hunks.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileSections<'a> {
    pub staged: Vec<&'a FileStatus>,
    pub unstaged: Vec<&'a FileStatus>,
    pub untracked: Vec<&'a FileStatus>,
}

impl<'a> FileSections<'a> {
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }
}

/// Route each `FileStatus` into the right section(s). See the partial-stage
/// rule in the type doc above.
///
/// Filtering: `Ignored` rows are dropped from all sections. Conflicts
/// (`Unmerged`) surface in **Unstaged only** — the user must `git add` to
/// mark resolved, so showing them as already-staged would mislead.
///
/// Ordering: within Unstaged, conflict rows are pinned to the top so an
/// unresolved merge can't hide below ordinary modifications. Matches the
/// common SCM convention of surfacing conflicts first.
pub fn partition_files(files: &[FileStatus]) -> FileSections<'_> {
    let mut s = FileSections::default();
    for f in files {
        if matches!(f.index, IndexStatus::Ignored) || matches!(f.worktree, WorktreeStatus::Ignored)
        {
            continue;
        }
        if matches!(f.index, IndexStatus::Untracked)
            && matches!(f.worktree, WorktreeStatus::Untracked)
        {
            s.untracked.push(f);
            continue;
        }
        if matches!(f.index, IndexStatus::Unmerged)
            || matches!(f.worktree, WorktreeStatus::Unmerged)
        {
            s.unstaged.push(f);
            continue;
        }
        if f.is_staged() {
            s.staged.push(f);
        }
        if f.is_unstaged() {
            s.unstaged.push(f);
        }
    }
    // Stable partition: conflicts before non-conflicts; original relative
    // order preserved within each group (sort_by_key with stable sort).
    s.unstaged.sort_by_key(|f| !is_conflict(f));
    s
}

/// Convenience predicate used by sorting + row rendering to highlight
/// merge-conflict files. Either side carrying `Unmerged` flags the row.
pub(crate) fn is_conflict(f: &FileStatus) -> bool {
    matches!(f.index, IndexStatus::Unmerged) || matches!(f.worktree, WorktreeStatus::Unmerged)
}

#[derive(Copy, Clone)]
enum RowKind {
    Staged,
    Unstaged,
    Untracked,
}

/// Re-exported `RowKind` shape for `row_actions::action_cluster`. Defining
/// it here (rather than passing the private `RowKind` directly) keeps
/// `changed_files` the owner of the section taxonomy while letting the
/// out-of-file action helpers see the variants.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RowKindForActions {
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

/// Bundle of styling + selection state threaded through the render helpers.
/// Cuts down on clippy `too_many_arguments` noise once `cx` is also in scope.
pub struct RenderCtx<'a> {
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
    pub selected: Option<&'a Path>,
    /// Section titles currently collapsed. Borrowed from `GitPanel` for the
    /// duration of a single render pass — re-reading the entity from `cx`
    /// during render panics because GPUI already holds a mut borrow.
    pub collapsed: &'a HashSet<&'static str>,
    /// Current branch name from `GitState::branch`. Used by the empty
    /// state to emit "no changes ahead of {branch}" rather than the bare
    /// "No changes". `None` covers detached HEAD or pre-status states.
    pub branch: Option<&'a str>,
}

/// Top-level renderer. Builds the three sections in order; emits a single
/// "No changes" placeholder when all sections are empty.
pub fn render_sections(
    sections: &FileSections<'_>,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> impl IntoElement {
    if sections.is_empty() {
        return empty_state(rctx).into_any_element();
    }
    // Order: unstaged first (CHANGES), then staged, then untracked. Mirrors
    // the common SCM staging flow so users edit-then-stage top-to-bottom.
    //
    // No `h_full()` here: the parent scroll container needs this column to
    // grow to its natural (sum-of-rows) height so `overflow_y_scroll` sees
    // overflow and actually scrolls. With `h_full()` the column would clamp
    // to parent height, content clips silently, and the scrollbar never fires.
    div()
        .flex()
        .flex_col()
        .child(section(
            "CHANGES",
            &sections.unstaged,
            RowKind::Unstaged,
            rctx,
            cx,
        ))
        .child(section(
            "STAGED CHANGES",
            &sections.staged,
            RowKind::Staged,
            rctx,
            cx,
        ))
        .child(section(
            "UNTRACKED FILES",
            &sections.untracked,
            RowKind::Untracked,
            rctx,
            cx,
        ))
        .into_any_element()
}

fn section(
    title: &'static str,
    rows: &[&FileStatus],
    kind: RowKind,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> impl IntoElement {
    if rows.is_empty() {
        return div().into_any_element();
    }
    let is_collapsed = rctx.collapsed.contains(&title);
    let chevron = if is_collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };
    let count = rows.len();
    let theme = rctx.theme;
    // Stable id required for GPUI hover/click interactivity on raw divs.
    let header_id = format!("git-section-{title}");
    let view_all_id = format!("git-view-all-{title}");
    let header = div()
        .id(gpui::SharedString::from(header_id))
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(rctx.density.h_tab))
        .px(px(sc_style::PAD_H))
        .rounded(px(rctx.density.r_xs))
        .text_size(px(sc_style::CAPS_TEXT))
        .font_weight(rctx.typography.w_semibold)
        .text_color(rctx.theme.fg_muted)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt).text_color(theme.fg_base))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, _window, cx| {
                panel.toggle_section(title, cx);
            }),
        )
        .child(Icon::new(chevron).size_3().text_color(rctx.theme.fg_subtle))
        .child(title.to_string())
        .child(
            div()
                .text_color(rctx.theme.fg_subtle)
                .child(format!("{count}")),
        )
        // Right-aligned "View all" ghost link. Decorative until the multi-diff
        // opener ships; the tooltip is honest about scope.
        .child(
            div().ml_auto().child(
                Button::new(gpui::SharedString::from(view_all_id))
                    .ghost()
                    .xsmall()
                    .label("View all")
                    .tooltip("Open all diffs (coming soon)")
                    .disabled(true)
                    .on_click(|_: &ClickEvent, _, _| {}),
            ),
        );
    let mut col = div()
        .flex()
        .flex_col()
        .pt(px(rctx.density.pad_panel))
        .child(header);
    if !is_collapsed {
        for f in rows {
            col = col.child(row(f, kind, rctx, cx));
        }
    }
    col.into_any_element()
}

/// One status badge per row — `M`/`A`/`D`/`R`/`U`/`C`. The kind argument lets
/// the staged-side dot fall back to "M" (modified) when the index is
/// `Unmodified` but the worktree changed (a partial-stage row mirrored into
/// the Staged section).
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

fn row(
    f: &FileStatus,
    kind: RowKind,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> impl IntoElement {
    let is_selected = rctx.selected == Some(f.path.as_path());
    let bg = if is_selected {
        rctx.theme.selection
    } else {
        rctx.theme.bg_panel
    };
    let theme = rctx.theme;
    let click_path = f.path.clone();
    let click_staged = matches!(kind, RowKind::Staged);
    // Untracked rows are git-invisible — `git diff` returns nothing for
    // them. The host's `OnOpenDiff` callback routes such clicks through
    // `Repository::diff_for_untracked` instead, reading the file off disk
    // to synthesize an all-additions diff.
    let click_untracked = matches!(kind, RowKind::Untracked);
    // Skip the editor-open dispatch for rows whose worktree file no
    // longer exists (deleted in working tree) — opening would surface an
    // empty editor buffer for a path the user explicitly removed. The
    // inline mini-diff in the sidebar still loads (showing the removed
    // content as a `-` block) via `set_selected → DiffView::load`.
    let click_should_open_editor = !matches!(f.worktree, oximux_core::WorktreeStatus::Deleted)
        && !matches!(f.index, oximux_core::IndexStatus::Deleted);

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

    // Stable id (path + kind) keeps GPUI's hover interactivity working when
    // the same row exists in both Staged and Unstaged sections after a partial
    // stage. Without the kind suffix, both rows would share an id and only one
    // would react to hover.
    let row_id = format!(
        "git-row-{}-{}",
        match kind {
            RowKind::Staged => "s",
            RowKind::Unstaged => "u",
            RowKind::Untracked => "n",
        },
        f.path.display()
    );

    // Hover-only action cluster (stage / unstage / discard). On hover the
    // status badge fades out and these icons fade in within the same column
    // via `group_hover`. The set varies by row kind — see `row_actions`.
    let action_cluster = crate::shell::git_panel::row_actions::action_cluster(
        f.path.clone(),
        kind.into(),
        gpui::SharedString::from(row_id.clone()),
        cx,
    );

    div()
        .id(gpui::SharedString::from(row_id.clone()))
        .group(gpui::SharedString::from(row_id.clone()))
        .flex()
        .items_center()
        .gap(px(rctx.density.gap_inline))
        .h(px(rctx.density.h_row + 2.0))
        .px(px(sc_style::PAD_H))
        .bg(bg)
        .text_size(px(sc_style::TEXT))
        .text_color(rctx.theme.fg_base)
        .when(!is_selected, |s| s.hover(|s| s.bg(theme.bg_panel_alt)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, window, cx| {
                panel.set_selected(Some((click_path.clone(), click_staged)), cx);
                // Open a real diff tab in the main pane via the host
                // callback. The inline sidebar mini-diff also loads
                // (via `set_selected` → `DiffView::load` above) as a
                // glanceable summary alongside the main-pane tab.
                // `click_should_open_editor` gates out files that are
                // deleted on disk — the sidebar mini-diff already shows
                // the `-` block for those.
                if click_should_open_editor && let Some(cb) = panel.on_open.clone() {
                    (cb)(
                        click_path.clone(),
                        click_staged,
                        click_untracked,
                        window,
                        cx,
                    );
                }
                cx.notify();
            }),
        )
        .child(icon_el)
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(6.0))
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(rctx.theme.fg_base)
                        .font_weight(rctx.typography.w_medium)
                        .child(file_name),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_size(px(sc_style::META_TEXT))
                        .text_color(rctx.theme.fg_subtle)
                        .child(parent),
                ),
        )
        .child(
            // Right slot stacks badge + action cluster; group_hover toggles
            // which is visible. Both occupy the same slot via absolute
            // positioning so the swap is jitter-free. The slot is wider
            // (38px) than before to host up to two action buttons.
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

fn empty_state(rctx: &RenderCtx<'_>) -> impl IntoElement {
    // Two-line empty state: a strong "No changes on this branch" headline
    // followed by a muted subline that surfaces the branch name when known.
    // Falls back to a single-line headline when branch is unknown (detached
    // HEAD, pre-first-poll, etc.) — avoids "no changes ahead of None".
    let headline = div()
        .text_size(px(sc_style::TEXT))
        .font_weight(rctx.typography.w_medium)
        .text_color(rctx.theme.fg_base)
        .child("No changes on this branch");
    // `filter(!is_empty)`: same defensive guard used in the branch
    // toolbar suffix — never trust `Some("")` for the subline either.
    let subline = rctx.branch.filter(|b| !b.is_empty()).map(|b| {
        div()
            .mt(px(4.0))
            .text_size(px(sc_style::TEXT - 1.0))
            .text_color(rctx.theme.fg_subtle)
            .child(format!(
                "This workspace is clean and this branch has no changes ahead of {b}"
            ))
    });
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .h_full()
        .px(px(sc_style::PAD_H))
        .py(px(rctx.density.pad_panel))
        .child(headline)
        .children(subline)
}
