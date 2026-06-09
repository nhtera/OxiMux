//! "Committed on Branch" section — the files this branch has changed in
//! its own (typically unpushed) commits, relative to its base.
//!
//! Self-contained section component (like the commit graph / stash panel),
//! NOT folded into the staging-centric `GitPanel`: these rows are READ-ONLY
//! — staging an already-committed change is meaningless — so a click opens
//! a range diff (`merge_base..HEAD` for that file) instead of offering
//! stage/unstage/discard.
//!
//! Data arrives via [`set_state`] from the source-control poll observer
//! (`GitState::branch_committed` + `branch_range`). The section hides
//! entirely when there's nothing committed ahead of the base. A row click
//! emits [`ShowBranchFileRequested`]; the workspace subscribes and opens a
//! `PaneGroupTabKind::BranchFile` tab via
//! `ProjectPanes::open_or_activate_branch_diff_tab`.

use crate::shell::file_explorer::file_icon::icon_for_name;
use crate::shell::source_control::style as sc_style;
use gpui::{
    Animation, AnimationExt, ClickEvent, Context, EventEmitter, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement as _, Styled, Window, div,
    ease_out_quint, px, svg,
};
use gpui_component::{Icon, IconName};
use oximux_core::{BranchCommittedFile, BranchRange, DiffStatus};
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;

/// Emitted when a branch-committed row is clicked. Carries the revision
/// range (`merge_base..head`) plus the file path so the host can open the
/// per-file range diff.
#[derive(Debug, Clone)]
pub struct ShowBranchFileRequested {
    pub base: String,
    pub head: String,
    pub path: PathBuf,
}

pub struct BranchCommitsPanel {
    theme: Theme,
    density: Density,
    typography: Typography,
    files: Vec<BranchCommittedFile>,
    range: Option<BranchRange>,
    collapsed: bool,
}

/// Emitted when the section header's "View all" CTA is clicked. Carries the
/// `merge_base..head` range so the host opens a combined read-only diff of
/// every file committed on the branch.
#[derive(Debug, Clone)]
pub struct ShowBranchDiffAllRequested {
    pub base: String,
    pub head: String,
}

impl EventEmitter<ShowBranchFileRequested> for BranchCommitsPanel {}
impl EventEmitter<ShowBranchDiffAllRequested> for BranchCommitsPanel {}

impl BranchCommitsPanel {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            theme,
            density,
            typography,
            files: Vec::new(),
            range: None,
            // Expanded by default — matches the reference, which shows the
            // branch's committed work without an extra click.
            collapsed: false,
        }
    }

    /// Feed the latest branch-committed snapshot. Equality-guarded so an
    /// unchanged poll tick doesn't churn a render.
    pub fn set_state(
        &mut self,
        files: Vec<BranchCommittedFile>,
        range: Option<BranchRange>,
        cx: &mut Context<Self>,
    ) {
        if self.files != files || self.range != range {
            self.files = files;
            self.range = range;
            cx.notify();
        }
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        cx.notify();
    }
}

impl Render for BranchCommitsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // Hide the whole section when the branch carries no committed work
        // ahead of its base — no empty header, matching the file sections.
        if self.files.is_empty() {
            return div().into_any_element();
        }
        // Header styled to match git_panel's `section()` headers (CHANGES /
        // UNTRACKED) exactly: Icon chevron, h_tab height, PAD_H, rounded,
        // semibold caps, fg_muted → fg_base on hover. Keeps the section
        // visually unified with the file sections it docks beneath.
        let chevron = if self.collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        // "View all" — opens a combined read-only diff of every file
        // committed on the branch. Hover-revealed (like git_panel's section
        // clusters), only when a range is known. Stops click propagation so
        // it doesn't toggle the section collapse.
        let r_xs = self.density.r_xs;
        let view_all = self.range.as_ref().map(|range| {
            let base = range.merge_base.clone();
            let head = range.head.clone();
            div()
                .id("branch-commits-view-all")
                .ml_auto()
                .invisible()
                .group_hover("branch-commits-header", |s| s.visible())
                .px(px(6.0))
                .py(px(1.0))
                .rounded(px(r_xs))
                .text_size(px(sc_style::GRAPH_META_TEXT))
                .text_color(theme.fg_muted)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt).text_color(theme.fg_base))
                .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    cx.emit(ShowBranchDiffAllRequested {
                        base: base.clone(),
                        head: head.clone(),
                    });
                }))
                .child("View all")
        });
        let header = div()
            .id("branch-commits-header")
            .group("branch-commits-header")
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(self.density.h_tab))
            .px(px(sc_style::PAD_H))
            .rounded(px(self.density.r_xs))
            .text_size(px(sc_style::CAPS_TEXT))
            .font_weight(self.typography.w_semibold)
            .text_color(theme.fg_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_panel_alt).text_color(theme.fg_base))
            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle(cx)))
            .child(Icon::new(chevron).size_3().text_color(theme.fg_subtle))
            .child("COMMITTED ON BRANCH")
            .child(
                div()
                    .text_color(theme.fg_subtle)
                    .child(format!("{}", self.files.len())),
            )
            .children(view_all);

        // `pt(pad_panel)` mirrors the inter-section breathing room git_panel's
        // `section()` adds to every non-topmost file section, so the gap above
        // COMMITTED ON BRANCH equals the gap above UNTRACKED FILES.
        let mut col = div()
            .flex()
            .flex_col()
            .w_full()
            .pt(px(self.density.pad_panel))
            .child(header);
        if !self.collapsed {
            // Same expand-fade convention as the file sections: wrap the row
            // set in one opacity layer, keyed on a stable id so it replays on
            // each expand (body unmounts while collapsed) but settles across
            // re-renders. Opacity-only — no geometry shift on this list.
            let motion = crate::motion_settings::active(cx);
            let mut body = div().flex().flex_col().w_full();
            for f in &self.files {
                body = body.child(self.row(f, cx));
            }
            col = col.child(body.with_animation(
                "branch-commits-body",
                Animation::new(motion.m_collapse).with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            ));
        }
        col.into_any_element()
    }
}

impl BranchCommitsPanel {
    fn row(&self, f: &BranchCommittedFile, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let pad = px(sc_style::pad_h(self.density));
        let (badge, badge_color) = status_badge(&f.status, &theme);

        let leaf = f
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let parent = f
            .path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty());

        // Leading file-type icon, colored by status — matches the
        // changed-files rows (row_renderer) so the section reads uniformly.
        let icon_el: gpui::AnyElement = match icon_for_name(&leaf) {
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

        let id = gpui::ElementId::Name(format!("branch-file-{}", f.path.display()).into());
        let range = self.range.clone();
        let path = f.path.clone();

        let row = div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(self.density.h_row))
            .px(pad)
            .text_size(px(sc_style::BODY_TEXT))
            .text_color(theme.fg_base)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_panel_alt))
            .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                if let Some(r) = range.as_ref() {
                    cx.emit(ShowBranchFileRequested {
                        base: r.merge_base.clone(),
                        head: r.head.clone(),
                        path: path.clone(),
                    });
                }
            }))
            .child(icon_el)
            .child(
                // Name + parent path share a shrinkable cluster: the path
                // truncates with an ellipsis instead of pushing the trailing
                // counts/badge off a narrow panel. Same collapse priority as
                // the changed-files rows (row_renderer) this section mirrors.
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(div().flex_shrink_0().child(leaf))
                    .children(parent.map(|parent| {
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(sc_style::GRAPH_META_TEXT))
                            .text_color(theme.fg_subtle)
                            .child(parent)
                    })),
            )
            // Counts + status badge pinned to the trailing edge (never shrink).
            .child(line_counts(f.added, f.removed, &theme))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(sc_style::GRAPH_META_TEXT))
                    .text_color(badge_color)
                    .child(badge),
            );
        row
    }
}

/// `+A` (green) `-B` (red) cluster. Zeroes are shown so the column stays
/// stable across rows (an all-context rename still reads as `+0 -0`).
fn line_counts(added: u32, removed: u32, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .gap(px(sc_style::LINE_COUNT_GAP))
        .text_size(px(sc_style::GRAPH_META_TEXT))
        .child(
            div()
                .text_color(theme.git.added)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(theme.git.deleted)
                .child(format!("-{removed}")),
        )
}

/// Map a diff status to a single-letter badge + color, mirroring the SCM
/// file-row convention (A added, M modified, D deleted, R renamed, C
/// copied).
fn status_badge(status: &DiffStatus, theme: &Theme) -> (&'static str, Hsla) {
    match status {
        DiffStatus::Added => ("A", theme.git.added),
        DiffStatus::Modified | DiffStatus::ModeChanged { .. } => ("M", theme.status_warn),
        DiffStatus::Deleted => ("D", theme.git.deleted),
        DiffStatus::Renamed { .. } => ("R", theme.status_info),
        DiffStatus::Copied { .. } => ("C", theme.status_info),
        DiffStatus::Binary => ("B", theme.fg_subtle),
    }
}
