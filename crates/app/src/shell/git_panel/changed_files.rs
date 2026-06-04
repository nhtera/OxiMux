//! Changed-files view — section routing + section header. Pure helpers;
//! the stateful entity lives in `super::GitPanel`. Per-row rendering
//! (status badge, name + rename arrow, diff counts, conflict sub-label,
//! hover-action cluster) lives in `row_renderer`. Splitting them keeps
//! this module under the 500-LOC warn cap as Phase 02 piles three new
//! decorations onto each row.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::discard_confirm::DiscardAllArea;
use crate::shell::git_panel::row_renderer::{RowKind, row};
use crate::shell::source_control::style as sc_style;
use gpui::{
    AnyElement, ClickEvent, Context, EventEmitter, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Styled, div, px,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_core::{CombinedDiffScope, FileStatus, IndexStatus, ViewMode, WorktreeStatus};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::PathBuf;

/// Emitted when a section's "View all" CTA is clicked. The host opens a
/// combined multi-file diff tab for `scope` (CHANGES → all working-tree
/// changes, STAGED → staged only, UNTRACKED → untracked only). The right
/// SCM panel stays the primary navigator alongside it.
#[derive(Debug, Clone)]
pub struct ShowCombinedDiffRequested {
    pub scope: CombinedDiffScope,
}

impl EventEmitter<ShowCombinedDiffRequested> for GitPanel {}

/// The combined-diff scope a section's "View all" opens. Each section scopes
/// to its OWN files — CHANGES to unstaged tracked changes, STAGED to staged,
/// UNTRACKED to untracked — so "View all" never widens past the section the
/// user clicked.
fn combined_scope_for(kind: RowKind) -> CombinedDiffScope {
    match kind {
        RowKind::Unstaged => CombinedDiffScope::Unstaged,
        RowKind::Staged => CombinedDiffScope::Staged,
        RowKind::Untracked => CombinedDiffScope::Untracked,
    }
}

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
    /// Multi-select path set. The row renderer paints the selection
    /// background for every path that lands in this set; Phase 02's
    /// single-row highlight was `Option<&Path>` and lit at most one row
    /// at a time. Borrowed from `GitPanel::selected` for one render
    /// pass.
    pub selected: &'a HashSet<PathBuf>,
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
    /// `true` when the changed-files filter input has a non-empty
    /// query. Section-header hover buttons are suppressed in that
    /// case — the user filtered down to a subset of rows, and a
    /// "Stage all" / "Discard all" action against the unfiltered
    /// section would be surprising.
    pub filter_active: bool,
    /// `true` while a discard-confirm modal is mounted (single-row
    /// or area). The section-header destructive button (Discard /
    /// Delete all) disables in this state so a second click can't
    /// queue a second modal — `GitPanel::discard_area` rejects the
    /// click silently, but a disabled button gives the user honest
    /// feedback for why it's not firing.
    pub discard_pending: bool,
    /// Current section-rendering mode. `Flat` renders one row per file
    /// in per-status sections; `Tree` groups files under their directory
    /// ancestors via [`tree::build_tree`] and dispatches to
    /// [`crate::shell::git_panel::tree_render`].
    ///
    /// [`tree::build_tree`]: crate::shell::source_control::tree::build_tree
    pub view_mode: ViewMode,
    /// Folder paths whose subtree is currently collapsed in Tree mode.
    /// Ignored in Flat mode. Borrowed from `GitPanel::collapsed_dirs`
    /// for the duration of one render pass; toggled by clicking a
    /// folder row in the tree renderer.
    pub collapsed_dirs: &'a HashSet<PathBuf>,
}

/// Map a `RowKind` to the matching `DiscardAllArea` for section-header
/// "Discard all" / "Delete all" actions.
fn area_for(kind: RowKind) -> DiscardAllArea {
    match kind {
        RowKind::Unstaged => DiscardAllArea::Unstaged,
        RowKind::Staged => DiscardAllArea::Staged,
        RowKind::Untracked => DiscardAllArea::Untracked,
    }
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
    //
    // `is_topmost` flag suppresses the leading `pad_panel` on whichever
    // section ends up rendering first — without it, the composer's
    // "Stage All" button sits a `pad_panel` (8px) gap above the
    // CHANGES header, which reads as dead space on a clean composer.
    // Empty sections collapse to a zero-height div, so the next
    // populated section inherits the "topmost" role naturally.
    let unstaged_empty = sections.unstaged.is_empty();
    let staged_empty = sections.staged.is_empty();
    div()
        .flex()
        .flex_col()
        .child(section(
            "CHANGES",
            &sections.unstaged,
            RowKind::Unstaged,
            true,
            rctx,
            cx,
        ))
        .child(section(
            "STAGED CHANGES",
            &sections.staged,
            RowKind::Staged,
            unstaged_empty,
            rctx,
            cx,
        ))
        .child(section(
            "UNTRACKED FILES",
            &sections.untracked,
            RowKind::Untracked,
            unstaged_empty && staged_empty,
            rctx,
            cx,
        ))
        .into_any_element()
}

fn section(
    title: &'static str,
    rows: &[&FileStatus],
    kind: RowKind,
    is_topmost: bool,
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
    // Also doubles as the `group()` scope so the hover action cluster
    // appears on header hover (no row hover triggers it).
    let header_id = gpui::SharedString::from(format!("git-section-{title}"));
    // Hover cluster is suppressed when a filter is active — the
    // section's row set is a subset, and "Stage all" / "Discard all"
    // would silently act on rows the user filtered out.
    let cluster_paths: Vec<PathBuf> = rows.iter().map(|f| f.path.clone()).collect();
    let hover_cluster = if rctx.filter_active {
        None
    } else {
        Some(section_hover_cluster(
            kind,
            cluster_paths,
            header_id.clone(),
            theme,
            rctx.discard_pending,
            cx,
        ))
    };
    let header = div()
        .id(header_id.clone())
        .group(header_id.clone())
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
        // Right-aligned hover slot: empty by default, fades in the
        // section action cluster (Stage all / Unstage all / Discard
        // all / Delete all) when the user hovers the header. Replaces
        // the always-disabled "View all" placeholder Phase 02 carried;
        // Phase 11 will reintroduce a real "View all" once the
        // multi-diff opener ships.
        .children(hover_cluster.map(|c| {
            div()
                .ml_auto()
                .invisible()
                .group_hover(header_id.clone(), |s| s.visible())
                // Eat MouseDown inside the cluster so the parent header's
                // MouseDown handler does NOT fire — without this, clicking
                // a cluster button (Stage all / Discard all) would also
                // toggle the section's collapsed state because GPUI fires
                // `on_mouse_down` on the parent before the Button's
                // `on_click` (MouseUp) lands.
                .on_mouse_down(MouseButton::Left, |_ev, _win, cx| {
                    cx.stop_propagation();
                })
                .child(c)
        }));
    let mut col = div().flex().flex_col();
    if !is_topmost {
        // Inter-section breathing room. The topmost section skips this
        // so its header reads as continuous with whatever sits above it
        // in the panel (composer's Stage All button, conflict banner,
        // etc.) — see render_sections' is_topmost wiring.
        col = col.pt(px(rctx.density.pad_panel));
    }
    col = col.child(header);
    if !is_collapsed {
        match rctx.view_mode {
            ViewMode::Flat => {
                for f in rows {
                    col = col.child(row(f, kind, rctx, cx));
                }
            }
            ViewMode::Tree => {
                // Borrowed-iterator path: `rows` is `&[&FileStatus]`, so
                // `iter().copied()` produces `&FileStatus` without an
                // owned intermediate Vec. The tree node itself owns its
                // PathBuf children; the flatten pass returns
                // RenderRow-by-value (no borrow into `rows`).
                //
                // The tree handle is passed by reference into each
                // tree-row render so folder hover-actions and modifier
                // clicks can compute subtree leaves / sibling dirs at
                // render time and capture them into 'static click
                // handlers — `section_rows` itself is borrowed for the
                // render pass and can't be moved into a listener.
                //
                // TODO(perf): leaf-row lookup in `tree_render::leaf_row`
                // does an O(N) linear scan over `rows` per leaf, total
                // O(N²) per section. Pre-build a `HashMap<&Path,
                // &FileStatus>` here if a future profile shows it.
                let tree = crate::shell::source_control::tree::build_tree(
                    rows.iter().copied(),
                    crate::shell::git_panel::tree_render::section_for(kind),
                );
                let flat_rows = crate::shell::source_control::tree::flatten(
                    &tree,
                    rctx.collapsed_dirs,
                );
                for flat_row in flat_rows {
                    col = col.child(
                        crate::shell::git_panel::tree_render::render_tree_row(
                            flat_row, &tree, rows, kind, rctx, cx,
                        ),
                    );
                }
            }
        }
    }
    col.into_any_element()
}

/// Per-section action cluster: 2 ghost icon buttons.
/// - CHANGES (Unstaged): `+ Stage all` · `↺ Discard all`
/// - STAGED:             `− Unstage all` · `↺ Discard all`
/// - UNTRACKED:          `+ Stage all` · `🗑 Delete all`
///
/// `paths` is the section's full path list (already filtered out of
/// collapsed-section invisibility by the caller's `filter_active`
/// check). The Stage / Unstage buttons fire-and-forget through
/// `stage_paths_bulk` / `unstage_paths_bulk`. Discard / Delete opens
/// the type-to-confirm modal via `discard_area`.
///
/// `discard_pending` disables the destructive button (Discard /
/// Delete all) when a confirm modal is already mounted — without
/// the guard the button looks active but the click would be silently
/// dropped by `discard_area`'s single-flight check.
#[allow(clippy::too_many_arguments)]
fn section_hover_cluster(
    kind: RowKind,
    paths: Vec<PathBuf>,
    header_id: gpui::SharedString,
    theme: Theme,
    discard_pending: bool,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    let area = area_for(kind);
    let mut cluster = div().flex().flex_row().items_center().gap(px(2.0));

    // "View all" — opens this section's files in one combined diff tab. The
    // right SCM panel stays the primary navigator; this is the full-pane
    // review surface for scanning every change without clicking file by
    // file. Leftmost so it reads before the staging actions. Emits an event
    // the workspace turns into a combined-diff tab.
    let view_all_scope = combined_scope_for(kind);
    let view_all_id =
        gpui::SharedString::from(format!("git-section-viewall-{}", header_id.as_ref()));
    cluster = cluster.child(
        Button::new(view_all_id)
            .ghost()
            .xsmall()
            .label("View all")
            .tooltip("Open all changes in a combined diff")
            .on_click(cx.listener(move |_panel, _: &ClickEvent, _window, cx| {
                cx.emit(ShowCombinedDiffRequested {
                    scope: view_all_scope.clone(),
                });
            })),
    );

    // Additive button on the left for CHANGES / UNTRACKED, "Unstage
    // all" for STAGED. All three live in the same visual slot.
    let primary_paths = paths.clone();
    let primary_id =
        gpui::SharedString::from(format!("git-section-primary-{}", header_id.as_ref()));
    cluster = cluster.child(match kind {
        RowKind::Unstaged | RowKind::Untracked => Button::new(primary_id)
            .ghost()
            .xsmall()
            .icon(
                Icon::default()
                    .path("icons/plus.svg")
                    .size(px(SECTION_BTN_PX))
                    .text_color(theme.status_added),
            )
            .tooltip("Stage all")
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                panel.stage_paths_bulk(primary_paths.clone(), cx);
            }))
            .into_any_element(),
        RowKind::Staged => Button::new(primary_id)
            .ghost()
            .xsmall()
            .icon(
                Icon::default()
                    .path("icons/minus.svg")
                    .size(px(SECTION_BTN_PX))
                    .text_color(theme.status_removed),
            )
            .tooltip("Unstage all")
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                panel.unstage_paths_bulk(primary_paths.clone(), cx);
            }))
            .into_any_element(),
    });

    // Destructive button on the right. `trash` for Untracked (the
    // copy reads "Delete"), `undo` for Staged + Unstaged (copy reads
    // "Discard"). Icon stays muted — the colour cue is the area
    // copy, not the glyph, so the user reads the modal title before
    // the worktree mutates.
    let destructive_paths = paths.clone();
    let destructive_id =
        gpui::SharedString::from(format!("git-section-destructive-{}", header_id.as_ref()));
    // `delete.svg` ships in the gpui-component asset bundle as the
    // canonical "trash" glyph; `trash.svg` isn't bundled. The
    // destructive button on Staged + Unstaged keeps `undo.svg` (used
    // by the per-row revert affordance — same visual language).
    let (dest_icon, dest_tooltip) = match area {
        DiscardAllArea::Untracked => ("icons/delete.svg", "Delete all"),
        _ => ("icons/undo.svg", "Discard all"),
    };
    let mut destructive_btn = Button::new(destructive_id)
        .ghost()
        .xsmall()
        .icon(
            Icon::default()
                .path(dest_icon)
                .size(px(SECTION_BTN_PX))
                .text_color(theme.fg_muted),
        )
        .tooltip(dest_tooltip);
    if discard_pending {
        destructive_btn = destructive_btn.disabled(true);
    } else {
        destructive_btn = destructive_btn.on_click(cx.listener(
            move |panel, _: &ClickEvent, _window, cx| {
                panel.discard_area(area, destructive_paths.clone(), cx);
            },
        ));
    }
    cluster = cluster.child(destructive_btn);

    cluster.into_any_element()
}

/// Section-header action button icon size. Slightly larger than the
/// per-row action cluster (`row_actions::ACTION_BTN_W` = 16px) so the
/// section-level affordances read distinct from row affordances.
const SECTION_BTN_PX: f32 = 14.0;

fn empty_state(rctx: &RenderCtx<'_>) -> impl IntoElement {
    // Two-line empty state: a strong "No changes on this branch" headline
    // followed by a muted subline that surfaces the branch name when known.
    // Falls back to a single-line headline when branch is unknown (detached
    // HEAD, pre-first-poll, etc.) — avoids "no changes ahead of None".
    let headline = div()
        .text_size(px(sc_style::BODY_TEXT))
        .font_weight(rctx.typography.w_medium)
        .text_color(rctx.theme.fg_base)
        .child("No changes on this branch");
    // `filter(!is_empty)`: same defensive guard used in the branch
    // toolbar suffix — never trust `Some("")` for the subline either.
    let subline = rctx.branch.filter(|b| !b.is_empty()).map(|b| {
        div()
            .mt(px(4.0))
            .text_size(px(sc_style::BODY_TEXT - 1.0))
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
