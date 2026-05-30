//! Changed-files view — section routing + section header. Pure helpers;
//! the stateful entity lives in `super::GitPanel`. Per-row rendering
//! (status badge, name + rename arrow, diff counts, conflict sub-label,
//! hover-action cluster) lives in `row_renderer`. Splitting them keeps
//! this module under the 500-LOC warn cap as Phase 02 piles three new
//! decorations onto each row.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::row_renderer::{RowKind, row};
use crate::shell::source_control::style as sc_style;
use gpui::{
    ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, px,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// Public mirror of `row_renderer::RowKind` for the per-row action
/// cluster. The variants match one-to-one; the duplication is so the
/// private `RowKind` can stay an implementation detail of the renderer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RowKindForActions {
    Staged,
    Unstaged,
    Untracked,
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
    /// Paths whose `confirmed_discard_path` op is still on the tokio
    /// runtime. Borrowed from `GitPanel::in_flight_discards`; row
    /// renderer reads this to swap the revert icon for a spinner.
    pub in_flight_discards: &'a HashSet<PathBuf>,
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
