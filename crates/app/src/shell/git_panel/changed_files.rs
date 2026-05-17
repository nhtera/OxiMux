//! Changed-files view — section routing + row rendering. Pure helpers; the
//! stateful entity lives in `super::GitPanel`. Row click → `panel.set_selected`.
//! Stage / unstage are dispatched via the workspace `StageFile` / `UnstageFile`
//! actions (Phase 2 step 8 routes them through `GitPanel`'s action handlers).

use crate::shell::git_panel::GitPanel;
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    div, px,
};
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use oximux_settings::{Density, Theme, Typography};
use std::path::Path;

/// Borrowed-slice sectioning of `GitState::files` into Staged / Unstaged /
/// Untracked. A partially-staged file (non-`Unmodified` on both sides) appears
/// in BOTH Staged and Unstaged — matches VSCode / Magit convention.
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
    s
}

#[derive(Copy, Clone)]
enum RowKind {
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
    div()
        .flex()
        .flex_col()
        .h_full()
        .child(section(
            "STAGED",
            &sections.staged,
            RowKind::Staged,
            rctx,
            cx,
        ))
        .child(section(
            "UNSTAGED",
            &sections.unstaged,
            RowKind::Unstaged,
            rctx,
            cx,
        ))
        .child(section(
            "UNTRACKED",
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
    let mut col = div().flex().flex_col().child(
        div()
            .flex()
            .items_center()
            .h(px(rctx.density.h_tab))
            .px(px(rctx.density.pad_panel))
            .text_size(px(rctx.typography.t_label_caps))
            .font_weight(rctx.typography.w_semibold)
            .text_color(rctx.theme.fg_muted)
            .child(format!("{title} ({})", rows.len())),
    );
    for f in rows {
        col = col.child(row(f, kind, rctx, cx));
    }
    col
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
    let glyph = match kind {
        RowKind::Staged => "●",
        RowKind::Unstaged => "○",
        RowKind::Untracked => "?",
    };
    let click_path = f.path.clone();
    let label = f.path.display().to_string();
    div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_row))
        .px(px(rctx.density.pad_panel))
        .bg(bg)
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_base)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, _window, cx| {
                panel.set_selected(Some(click_path.clone()));
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(14.0))
                .text_color(rctx.theme.fg_muted)
                .child(glyph),
        )
        .child(div().flex_1().child(label))
}

fn empty_state(rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .p(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child("No changes")
}
