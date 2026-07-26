//! Opt-in in-diff file navigator for the combined / multi-file diff.
//!
//! The persistent external SCM panel stays the PRIMARY file list; this rail
//! is the in-scroll navigator shown alongside the combined diff. Files render
//! as a collapsible FOLDER TREE under their `FileGroup` section (Unstaged /
//! Staged / Untracked / Committed) when the diff carries group tags (the
//! combined view); commit / range diffs render a single flat group. A filter
//! box narrows the rows live. Clicking a file jumps the body to its header;
//! the file at the top of the viewport highlights as the body scrolls.

use crate::shell::diff_view::DiffView;
use crate::shell::diff_view::file_header::{StickyHeader, stats_chips};
use gpui::{
    App, ClickEvent, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, WeakEntity, div, px,
};
use gpui_component::{Icon, Sizable as _};
use oximux_core::FileGroup;
use oximux_settings::{Density, Theme, Typography};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Fixed rail width when revealed — wide enough for a nested filename + the
/// `+N −N` tally without crowding the body, which keeps the remaining space.
pub const RAIL_WIDTH: f32 = 248.0;

/// Left indent per tree depth level.
const INDENT_PX: f32 = 12.0;

/// Inputs the rail needs from the view, grouped to keep the call site (and
/// this fn's arity) readable.
pub struct RailContext<'a> {
    pub headers: &'a [StickyHeader],
    pub groups: Option<&'a [FileGroup]>,
    pub active_file_idx: usize,
    /// Collapsed dir rows, keyed by bare path-from-root. Scoped views (one
    /// group) are the norm, so paths are unique; a merged multi-group view
    /// would collapse a same-named dir in every section together — acceptable
    /// until/unless such a view gets a UI entry point.
    pub collapsed_dirs: &'a HashSet<PathBuf>,
    /// Lowercased filter query; rows whose path doesn't contain it are hidden.
    pub filter: &'a str,
    pub theme: Theme,
    pub density: Density,
}

/// Build the file rail body (rows only — the host wraps it with the filter
/// box and scroll container).
pub fn file_rail(
    ctx: RailContext<'_>,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement + use<> {
    let mut col = div().flex().flex_col().w_full();
    // One contiguous section per group (combined diffs arrive ordered
    // unstaged → staged → untracked); a flat list when there are no tags.
    let mut last_group: Option<FileGroup> = None;
    let mut group_headers: Vec<&StickyHeader> = Vec::new();
    let flush = |col: gpui::Div,
                 group: Option<FileGroup>,
                 hdrs: &[&StickyHeader],
                 ctx: &RailContext<'_>,
                 typography: &Typography,
                 weak: &WeakEntity<DiffView>|
     -> gpui::Div {
        if hdrs.is_empty() {
            return col;
        }
        let mut col = col;
        if let Some(g) = group {
            col = col.child(section_header(g, ctx.theme, typography));
        }
        let tree = build_tree(hdrs, ctx.theme);
        for row in flatten(&tree, ctx.collapsed_dirs) {
            col = col.child(render_row(row, ctx, typography, weak.clone()));
        }
        col
    };
    for header in ctx.headers {
        // Filter: skip headers whose path doesn't contain the query.
        if !ctx.filter.is_empty()
            && !header
                .path
                .as_ref()
                .to_lowercase()
                .contains(ctx.filter)
        {
            continue;
        }
        let g = ctx
            .groups
            .and_then(|groups| groups.get(header.file_idx).copied());
        if g != last_group && !group_headers.is_empty() {
            col = flush(col, last_group, &group_headers, &ctx, typography, &weak);
            group_headers.clear();
        }
        last_group = g;
        group_headers.push(header);
    }
    flush(col, last_group, &group_headers, &ctx, typography, &weak)
}

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

/// A leaf file's render data, resolved once at tree-build time.
struct Leaf {
    file_idx: usize,
    letter: char,
    color: Hsla,
    stats: Option<(u32, u32)>,
}

enum Node {
    Dir(BTreeMap<String, Node>),
    File(Leaf),
}

/// One flattened, depth-tagged render row.
enum Row {
    Dir {
        path: PathBuf,
        name: String,
        depth: usize,
        collapsed: bool,
        child_count: usize,
    },
    File {
        leaf_idx: usize,
        name: String,
        depth: usize,
        letter: char,
        color: Hsla,
        stats: Option<(u32, u32)>,
    },
}

/// Build a path trie from a group's headers. Directory keys are immediate
/// path components (`BTreeMap` → lexical order); the final component is the
/// file leaf.
fn build_tree(headers: &[&StickyHeader], theme: Theme) -> BTreeMap<String, Node> {
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for h in headers {
        let comps: Vec<String> = h
            .path
            .as_ref()
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if comps.is_empty() {
            continue;
        }
        let (letter, color) = status_glyph(&h.label, theme);
        insert(
            &mut root,
            &comps,
            Leaf {
                file_idx: h.file_idx,
                letter,
                color,
                stats: h.stats,
            },
        );
    }
    root
}

fn insert(level: &mut BTreeMap<String, Node>, comps: &[String], leaf: Leaf) {
    let (head, rest) = comps.split_first().expect("non-empty components");
    if rest.is_empty() {
        level.insert(head.clone(), Node::File(leaf));
        return;
    }
    let entry = level
        .entry(head.clone())
        .or_insert_with(|| Node::Dir(BTreeMap::new()));
    if let Node::Dir(children) = entry {
        insert(children, rest, leaf);
    }
    // A path component that is a file in one entry and a dir in another can't
    // happen for distinct file paths, so the non-Dir case is unreachable.
}

/// Depth-first flatten in display order; a collapsed dir hides its subtree.
fn flatten(root: &BTreeMap<String, Node>, collapsed: &HashSet<PathBuf>) -> Vec<Row> {
    let mut rows = Vec::new();
    walk(root, &PathBuf::new(), 0, collapsed, &mut rows);
    rows
}

fn walk(
    level: &BTreeMap<String, Node>,
    prefix: &Path,
    depth: usize,
    collapsed: &HashSet<PathBuf>,
    rows: &mut Vec<Row>,
) {
    for (name, node) in level {
        let path = prefix.join(name);
        match node {
            Node::File(leaf) => rows.push(Row::File {
                leaf_idx: leaf.file_idx,
                name: name.clone(),
                depth,
                letter: leaf.letter,
                color: leaf.color,
                stats: leaf.stats,
            }),
            Node::Dir(children) => {
                let is_collapsed = collapsed.contains(&path);
                rows.push(Row::Dir {
                    path: path.clone(),
                    name: name.clone(),
                    depth,
                    collapsed: is_collapsed,
                    child_count: count_files(children),
                });
                if !is_collapsed {
                    walk(children, &path, depth + 1, collapsed, rows);
                }
            }
        }
    }
}

fn count_files(level: &BTreeMap<String, Node>) -> usize {
    level
        .values()
        .map(|n| match n {
            Node::File(_) => 1,
            Node::Dir(c) => count_files(c),
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_row(
    row: Row,
    ctx: &RailContext<'_>,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> gpui::Stateful<gpui::Div> {
    match row {
        Row::Dir {
            path,
            name,
            depth,
            collapsed,
            child_count,
        } => dir_row(path, name, depth, collapsed, child_count, ctx.theme, typography, weak),
        Row::File {
            leaf_idx,
            name,
            depth,
            letter,
            color,
            stats,
        } => file_row(
            leaf_idx,
            name,
            depth,
            letter,
            color,
            stats,
            leaf_idx == ctx.active_file_idx,
            ctx.theme,
            ctx.density,
            typography,
            weak,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn dir_row(
    path: PathBuf,
    name: String,
    depth: usize,
    collapsed: bool,
    child_count: usize,
    theme: Theme,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> gpui::Stateful<gpui::Div> {
    let id = ElementId::Name(format!("diff-rail-dir-{}", path.display()).into());
    let chevron = if collapsed {
        "icons/chevron-right.svg"
    } else {
        "icons/chevron-down.svg"
    };
    let hover_bg = theme.hover_overlay;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(20.0))
        .w_full()
        .pr(px(8.0))
        .pl(px(8.0 + depth as f32 * INDENT_PX))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted)
        .hover(move |s| s.bg(hover_bg))
        .on_click(move |_: &ClickEvent, _window, cx: &mut App| {
            let dir = path.clone();
            let _ = weak.update(cx, |view, cx| view.toggle_rail_dir(dir, cx));
        })
        .child(
            Icon::default()
                .path(chevron)
                .xsmall()
                .text_color(theme.fg_subtle),
        )
        .child(div().flex_shrink().min_w(px(0.0)).truncate().child(name))
        .child(
            div()
                .text_color(theme.fg_subtle)
                .text_size(px(typography.t_body_sm))
                .child(format!("{child_count}")),
        )
}

#[allow(clippy::too_many_arguments)]
fn file_row(
    file_idx: usize,
    name: String,
    depth: usize,
    letter: char,
    color: Hsla,
    stats: Option<(u32, u32)>,
    active: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> gpui::Stateful<gpui::Div> {
    let id = ElementId::Name(format!("diff-rail-row-{file_idx}").into());
    let hover_bg = theme.hover_overlay;
    let mut row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .h(px(density.h_row))
        .w_full()
        .pr(px(8.0))
        // Indent past the dir chevron column so files line up under their dir.
        .pl(px(8.0 + (depth as f32 * INDENT_PX) + 16.0))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .on_click(move |_: &ClickEvent, _window, cx: &mut App| {
            let _ = weak.update(cx, |view, cx| {
                view.scroll_to_file(file_idx);
                cx.notify();
            });
        });
    if active {
        row = row.bg(theme.selection).font_weight(typography.w_medium);
    } else {
        row = row.hover(move |s| s.bg(hover_bg));
    }
    row = row
        .child(
            div()
                .flex_shrink_0()
                .w(px(12.0))
                .font(typography.mono_font())
                .text_color(color)
                .child(letter.to_string()),
        )
        .child(div().flex_shrink().min_w(px(0.0)).truncate().child(name));
    if let Some((added, removed)) = stats {
        row = row
            .child(div().flex_1())
            .child(stats_chips(added, removed, theme, typography));
    }
    row
}

/// A group divider — small-caps label introducing the files beneath it.
fn section_header(group: FileGroup, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w_full()
        .px(px(8.0))
        .pt(px(8.0))
        .pb(px(2.0))
        .text_size(px(typography.t_label_caps))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_subtle)
        .child(group_label(group))
}

/// One-letter git status + its color, derived from the file's display label
/// (`"Modified"`, `"Added"`, `"Renamed from …"`, …).
fn status_glyph(label: &str, theme: Theme) -> (char, Hsla) {
    match label.chars().next() {
        Some('A') => ('A', theme.git.added),
        Some('D') => ('D', theme.git.deleted),
        Some('R') => ('R', theme.status_info),
        Some('C') => ('C', theme.status_info),
        Some('B') => ('B', theme.fg_muted),
        // "Modified" and "Mode NNN → NNN" (mode-only change) both start 'M'.
        Some('M') => ('M', theme.git.modified),
        _ => ('•', theme.fg_muted),
    }
}

fn group_label(group: FileGroup) -> &'static str {
    match group {
        FileGroup::Unstaged => "Unstaged",
        FileGroup::Staged => "Staged",
        FileGroup::Untracked => "Untracked",
        FileGroup::Committed => "Committed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(file_idx: usize, path: &str) -> StickyHeader {
        StickyHeader {
            file_idx,
            path: path.to_string().into(),
            label: "Modified".to_string(),
            stats: Some((1, 0)),
            folded: false,
        }
    }

    #[test]
    fn tree_nests_by_directory_in_lexical_order() {
        let h = [hdr(0, "docs/a.md"), hdr(1, "docs/b.md"), hdr(2, "src/x.rs")];
        let refs: Vec<&StickyHeader> = h.iter().collect();
        let tree = build_tree(&refs, Theme::charcoal());
        let rows = flatten(&tree, &HashSet::new());
        // docs/ (dir), a.md, b.md, src/ (dir), x.rs = 5 rows.
        assert_eq!(rows.len(), 5);
        assert!(matches!(&rows[0], Row::Dir { name, child_count: 2, .. } if name == "docs"));
        assert!(matches!(&rows[1], Row::File { name, depth: 1, .. } if name == "a.md"));
        assert!(matches!(&rows[3], Row::Dir { name, .. } if name == "src"));
    }

    #[test]
    fn collapsed_dir_hides_its_subtree() {
        let h = [hdr(0, "docs/a.md"), hdr(1, "src/x.rs")];
        let refs: Vec<&StickyHeader> = h.iter().collect();
        let tree = build_tree(&refs, Theme::charcoal());
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("docs"));
        let rows = flatten(&tree, &collapsed);
        // docs/ (collapsed, no a.md), src/, x.rs = 3 rows.
        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[0], Row::Dir { name, collapsed: true, .. } if name == "docs"));
        assert!(matches!(&rows[1], Row::Dir { name, .. } if name == "src"));
        assert!(matches!(&rows[2], Row::File { name, .. } if name == "x.rs"));
    }

    #[test]
    fn root_level_file_has_no_dir_row() {
        let h = [hdr(0, "README.md")];
        let refs: Vec<&StickyHeader> = h.iter().collect();
        let tree = build_tree(&refs, Theme::charcoal());
        let rows = flatten(&tree, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], Row::File { name, depth: 0, .. } if name == "README.md"));
    }
}
