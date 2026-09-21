//! The rows inside an expanded stash: one per file, plus the one-line
//! placeholders for the states that are not a file list.
//!
//! # A stash file is not a changed file
//!
//! The shape is copied from the "Committed on Branch" row
//! (`branch_commits.rs`) — status-tinted file glyph, leaf name, dimmed parent
//! directory, trailing status letter — because both are read-only rows that
//! open a diff, and two read-only file lists in one panel that look different
//! read as two different kinds of thing. What is NOT copied is the `+A −B`
//! line-count cluster: `stash_files` is a name-status query, so we do not
//! have those numbers, and inventing a second query per file to fill a
//! decorative column would pay for the whole lazy-fetch design to be
//! discarded.
//!
//! # Every state paints a row
//!
//! `Loading` and `Failed` render a line rather than nothing. An expansion
//! that renders nothing is indistinguishable from a stash that touched no
//! files, and a *failed* expansion that renders nothing looks exactly like a
//! successful one — the user is told their stash is empty, which is the worst
//! possible way to be wrong about a stash.
//!
//! # Two layouts, one row
//!
//! Tree mode adds folder rows and an indent; it does not add a second file
//! row. [`StashPanel::render_file_row`] takes the indent as a parameter and
//! both layouts call it, so a file cannot come to look — or behave — like a
//! different kind of thing depending on a toggle. The tree's shape comes from
//! [`super::tree_view`].

use crate::shell::file_explorer::file_icon::icon_for_name;
use crate::shell::source_control::style::ScmStyle;
use crate::shell::source_control::tree::{NodeKind, RenderRow};
use crate::shell::stash_panel::keyboard::StashCursor;
use crate::shell::stash_panel::{
    ShowStashFileRequested, StashFilesState, StashPanel, tree_view,
};
use gpui::{
    AnyElement, ClickEvent, Context, ElementId, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement as _, Styled, div,
    prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{Icon, IconName};
use oximux_core::{DiffStatus, StashEntry, StashFile, ViewMode};

/// How far a file row is inset from the stash row above it, as a multiple of
/// the panel's own horizontal padding — enough to read as "inside that
/// stash", not so much that a nested path loses its width.
///
/// Also the BASE of the tree layout's indent ([`tree_view::indent_steps`]),
/// so a top-level tree row lands exactly here and flipping the toggle does
/// not slide the block sideways.
pub(super) const FLAT_INDENT_STEPS: f32 = 2.0;

impl StashPanel {
    /// The block rendered under an expanded stash row.
    pub(super) fn render_files(
        &self,
        entry: &StashEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.files_for(&entry.sha) {
            None | Some(StashFilesState::Loading) => self.file_note("Loading files…", false),
            Some(StashFilesState::Failed(err)) => {
                self.file_note(&format!("Could not read this stash's files: {err}"), true)
            }
            Some(StashFilesState::Ready(files)) if files.is_empty() => {
                // Reachable: `git stash store` can park a commit that changes
                // nothing. Say so, rather than leaving a gap that reads as a
                // rendering fault.
                self.file_note("No files in this stash", false)
            }
            Some(StashFilesState::Ready(files)) => match self.view_mode() {
                ViewMode::Flat => {
                    let mut col = div().flex().flex_col().w_full();
                    for file in files.clone() {
                        col = col.child(self.render_file_row(entry, file, FLAT_INDENT_STEPS, cx));
                    }
                    col.into_any_element()
                }
                ViewMode::Tree => self.render_file_tree(entry, &files.clone(), cx),
            },
        }
    }

    /// The tree layout: folder rows interleaved with the same file rows the
    /// flat list paints, each indented by its depth.
    fn render_file_tree(
        &self,
        entry: &StashEntry,
        files: &[StashFile],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut col = div().flex().flex_col().w_full();
        for row in tree_view::rows(files, &self.collapsed_for(&entry.sha)) {
            let indent = tree_view::indent_steps(row.depth);
            match row.kind {
                NodeKind::Dir => col = col.child(self.render_dir_row(entry, &row, indent, cx)),
                // A tree row is a path; the `StashFile` behind it carries the
                // status and the origin, and the row is skipped rather than
                // synthesised if the two ever disagree — a file row that
                // guessed its origin would route its diff at the wrong
                // revision.
                NodeKind::File => {
                    if let Some(file) = files.iter().find(|f| f.path == row.path) {
                        col = col.child(self.render_file_row(entry, file.clone(), indent, cx));
                    }
                }
            }
        }
        col.into_any_element()
    }

    /// One folder inside a stash's tree: chevron, name, child count.
    ///
    /// Deliberately thinner than the CHANGES panel's folder row, which carries
    /// a hover cluster of bulk stage/discard verbs. A stash has no bulk verb
    /// that operates on a subtree — there is no "restore this folder" — so the
    /// row is a disclosure triangle and nothing else. See the carried-forward
    /// note in the phase file.
    fn render_dir_row(
        &self,
        entry: &StashEntry,
        row: &RenderRow,
        indent_steps: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let style = ScmStyle::new(density, &self.typography);
        let collapsed = self.is_dir_collapsed(&entry.sha, &row.path);
        let name = row
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let sha = entry.sha.clone();
        let dir = row.path.clone();
        // Same keying rule as a file row: sha AND index, so one folder path
        // open in two expanded stashes does not share element state.
        let id = ElementId::Name(
            format!(
                "stash-dir-{}-{}-{}",
                entry.sha,
                entry.stash_ref.index,
                row.path.display()
            )
            .into(),
        );

        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .h(px(density.h_row))
            .pl(px(density.pad_panel * indent_steps))
            .pr(px(density.pad_panel))
            .text_size(px(style.body_text))
            .text_color(theme.fg_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                panel.toggle_dir(&sha, dir.clone(), cx);
            }))
            .child(
                Icon::default()
                    .path(if collapsed {
                        "icons/chevron-right.svg"
                    } else {
                        "icons/chevron-down.svg"
                    })
                    .size(px(style.icon))
                    .text_color(theme.fg_muted),
            )
            .child(div().flex_1().min_w(px(0.0)).truncate().child(name))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(style.graph_meta_text))
                    .text_color(theme.fg_subtle)
                    .child(format!("{}", row.child_count)),
            )
    }

    /// One clickable file inside a stash.
    fn render_file_row(
        &self,
        entry: &StashEntry,
        file: StashFile,
        indent_steps: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let style = ScmStyle::new(density, &self.typography);
        let (badge, badge_color) = status_badge(&file.status, &theme);

        let leaf = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let parent = file
            .path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty());

        let icon: AnyElement = match icon_for_name(&leaf) {
            Some(path) => svg()
                .path(path)
                .size(px(style.icon))
                .text_color(badge_color)
                .into_any_element(),
            None => Icon::new(IconName::File)
                .size_3()
                .text_color(badge_color)
                .into_any_element(),
        };

        // Keyed by the parent's address AND the path: a path can appear in
        // two expanded stashes at once, and an id shared between them would
        // hand both rows one element state. The index rather than the sha
        // alone, because a sha is not unique on the stack — two entries can
        // name one commit, and their file lists are then identical.
        let id = ElementId::Name(
            format!(
                "stash-file-{}-{}-{}",
                entry.sha,
                entry.stash_ref.index,
                file.path.display()
            )
            .into(),
        );
        let cursored = self.cursor()
            == Some(&StashCursor::File {
                sha: entry.sha.clone(),
                path: file.path.clone(),
            });
        let cursor_sha = entry.sha.clone();
        let cursor_path = file.path.clone();
        let request = ShowStashFileRequested {
            sha: entry.sha.clone(),
            path: file.path.clone(),
            origin: file.origin,
            label: crate::shell::stash_panel::list_render::row_message(entry),
        };
        // The right-click payload. Carries the parent stash's own fields as
        // well as the path: the menu is one entity with two shapes, and the
        // shape is chosen by `file_path` being `Some`.
        let menu = crate::actions::OpenStashContextMenuAt {
            x: 0.0,
            y: 0.0,
            sha: entry.sha.clone(),
            index: entry.stash_ref.index,
            message: entry.message.clone(),
            relative: entry.relative.clone(),
            branch: entry.branch.clone(),
            file_path: Some(file.path.display().to_string()),
            // Decides whether the menu offers Restore at all — see the
            // field's own note on why origin has to travel with the click.
            file_untracked: file.origin == oximux_core::StashFileOrigin::Untracked,
        };

        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .h(px(density.h_row))
            .pl(px(density.pad_panel * indent_steps))
            .pr(px(density.pad_panel))
            .text_size(px(style.body_text))
            .text_color(theme.fg_base)
            .cursor_pointer()
            // The keyboard cursor, painted as a background rather than a
            // border: a border would change the row's height and make arrowing
            // through a list nudge everything below it.
            .when(cursored, |row| row.bg(theme.selection))
            .hover(|s| s.bg(theme.hover_overlay))
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                // Clicking is also a cursor move, so arrowing continues from
                // the row the user last touched rather than from the top.
                //
                // It does NOT try to keep keyboard focus in the panel: this
                // click opens a diff tab, and the tab takes focus. See
                // `keyboard.rs` on what that costs the arrow keys.
                panel.set_cursor(
                    StashCursor::File {
                        sha: cursor_sha.clone(),
                        path: cursor_path.clone(),
                    },
                    cx,
                );
                cx.emit(request.clone());
            }))
            .on_mouse_down(
                MouseButton::Right,
                move |ev: &MouseDownEvent, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::actions::OpenStashContextMenuAt {
                            x: ev.position.x.into(),
                            y: ev.position.y.into(),
                            ..menu.clone()
                        }),
                        cx,
                    );
                },
            )
            .child(icon)
            .child(
                // Name and parent share one shrinkable cluster so the path
                // ellipsises instead of pushing the status letter off a
                // narrow panel — the same collapse priority the changed-files
                // and branch rows use.
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(density.gap_inline))
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(div().flex_shrink_0().child(leaf))
                    .children(parent.map(|parent| {
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(style.graph_meta_text))
                            .text_color(theme.fg_subtle)
                            .child(parent)
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(style.graph_meta_text))
                    .text_color(badge_color)
                    .child(badge),
            )
    }

    /// A single indented line standing in for a file list — loading, failed,
    /// or genuinely empty. Indented to the same step as a file row so the
    /// expansion reads as one block either way.
    fn file_note(&self, msg: &str, is_error: bool) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let style = ScmStyle::new(density, &self.typography);
        div()
            .flex()
            .items_center()
            .h(px(density.h_row))
            .pl(px(density.pad_panel * FLAT_INDENT_STEPS))
            .pr(px(density.pad_panel))
            .text_size(px(style.graph_meta_text))
            .text_color(if is_error {
                theme.status_error
            } else {
                theme.fg_subtle
            })
            .child(msg.to_string())
            .into_any_element()
    }
}

/// Single-letter status badge + colour. Mirrors the branch-section mapping
/// (`branch_commits::status_badge`) so one file reads the same wherever the
/// panel shows it.
fn status_badge(status: &DiffStatus, theme: &oximux_settings::Theme) -> (&'static str, gpui::Hsla) {
    match status {
        DiffStatus::Added => ("A", theme.git.added),
        DiffStatus::Modified | DiffStatus::ModeChanged { .. } => ("M", theme.status_warn),
        DiffStatus::Deleted => ("D", theme.git.deleted),
        DiffStatus::Renamed { .. } => ("R", theme.status_info),
        DiffStatus::Copied { .. } => ("C", theme.status_info),
        DiffStatus::Binary => ("B", theme.fg_subtle),
    }
}
