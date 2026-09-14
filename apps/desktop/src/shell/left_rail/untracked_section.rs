//! The collapsed `Untracked (N)` disclosure: worktrees git lists for a
//! project that no `workspaces` row points at.
//!
//! Shaped like the `Archived (N)` section beside it — one per project in
//! grouped mode, one cross-project section at the end of the flat list — but
//! its rows are not workspace cards: there is no row to activate, no agent,
//! no stats. Each shows the directory's name, its branch, and its path (the
//! path especially, since the whole point is that these came from
//! elsewhere), and offers `Adopt` on hover. The header offers `Hide` for the
//! project, the per-project opt-out for a repo where the group is noise.

use gpui::{
    Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, div, prelude::FluentBuilder as _, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::LeftRail;
use crate::shell::workspace::discovery::UntrackedWorktree;
use crate::workspace_root::WorkspaceRoot;

/// Reserved [`LeftRail::toggle_untracked_expanded`] key for the flat list's
/// single cross-project disclosure. Project ids are UUIDs, so no collision.
pub(crate) const FLAT_UNTRACKED_KEY: &str = "flat:untracked";

const HEADER_HEIGHT: f32 = 24.0;
const INDENT: f32 = 12.0;
const CHEVRON_ICON_SIZE: f32 = 12.0;
/// Untracked rows are muted like archived ones: present, not the user's yet.
const ROW_OPACITY: f32 = 0.8;

/// Render the disclosure for `rows`; nothing at all when there are none.
///
/// `Hide` appears only for a per-project section (`toggle_key` is the
/// project id): in the flat list the rows span projects, so "hide these" has
/// no single project to record the preference on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_untracked_section(
    toggle_key: &str,
    rows: Vec<UntrackedWorktree>,
    expanded: bool,
    rail: &Entity<LeftRail>,
    weak_root: &WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let count = rows.len();
    if count == 0 {
        return div();
    }
    let per_project = toggle_key != FLAT_UNTRACKED_KEY;
    let header_id: SharedString = format!("untracked-header-{toggle_key}").into();
    let chevron_path = if expanded {
        "icons/chevron-down.svg"
    } else {
        "icons/chevron-right.svg"
    };
    let rail_for_toggle = rail.clone();
    let key = toggle_key.to_string();
    let hide_root = weak_root.clone();
    let hide_project = toggle_key.to_string();
    let header = div()
        .id(header_id)
        .group("untracked-header")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(HEADER_HEIGHT))
        .pl(px(INDENT))
        .pr(px(density.gap_inline))
        .cursor_pointer()
        .hover(|st| st.bg(theme.hover_overlay))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted)
        .child(
            svg()
                .path(chevron_path)
                .size(px(CHEVRON_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .child(div().flex_1().child(SharedString::from(format!("Untracked ({count})"))))
        .when(per_project, |h| {
            h.child(
                div()
                    .id(SharedString::from(format!("untracked-hide-{toggle_key}")))
                    .invisible()
                    .group_hover("untracked-header", |s| s.visible())
                    .px(px(5.0))
                    .rounded(px(density.r_xs))
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                    .child("Hide")
                    .on_click(move |_ev, _window, cx| {
                        cx.stop_propagation();
                        let _ = hide_root.update(cx, |root, cx| {
                            root.set_hide_untracked_for_project(&hide_project, true, cx);
                        });
                    }),
            )
        })
        .on_click(move |_ev, _window, cx| {
            rail_for_toggle.update(cx, |r, cx| {
                r.toggle_untracked_expanded(&key);
                cx.notify();
            });
        });

    let mut section = div().flex().flex_col().w_full().child(header);
    if !expanded {
        return section;
    }
    let mut list = div().flex().flex_col().w_full().opacity(ROW_OPACITY);
    for (ix, u) in rows.into_iter().enumerate() {
        list = list.child(render_untracked_row(ix, u, weak_root, theme, density, typography));
    }
    section = section.child(list);
    section
}

/// One untracked worktree: name, branch chip, path; `Adopt` on hover.
fn render_untracked_row(
    ix: usize,
    u: UntrackedWorktree,
    weak_root: &WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let row_id: SharedString = format!("untracked-{}-{ix}", u.project_id).into();
    let group: SharedString = format!("{row_id}-g").into();
    let name = u.dir_name();
    let path: SharedString = u.path.display().to_string().into();
    let branch_chip = u.branch.clone().map(|b| {
        div()
            .flex()
            .items_center()
            .min_w_0()
            .flex_shrink()
            .px(px(5.0))
            .h(px(15.0))
            .rounded(px(density.r_chip))
            .bg(theme.bg_overlay)
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child(div().min_w_0().truncate().child(b))
    });
    let adopt_root = weak_root.clone();
    let adopt_target = u.clone();
    let adopt = div()
        .id(SharedString::from(format!("{row_id}-adopt")))
        .invisible()
        .group_hover(group.clone(), |s| s.visible())
        .flex_shrink_0()
        .px(px(6.0))
        .h(px(18.0))
        .flex()
        .items_center()
        .rounded(px(density.r_xs))
        .border_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_overlay))
        .child("Adopt")
        .on_click(move |_ev, _window, cx| {
            cx.stop_propagation();
            let _ = adopt_root.update(cx, |root, cx| root.adopt_untracked(adopt_target.clone(), cx));
        });

    div()
        .id(row_id)
        .group(group)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .h(px(density.h_row * 2.2))
        .px(px(density.pad_panel))
        .pl(px(INDENT + density.pad_panel))
        .hover(|s| s.bg(theme.hover_overlay))
        .child(
            svg()
                .path("icons/folder.svg")
                .size(px(14.0))
                .flex_shrink_0()
                .text_color(theme.fg_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .min_w_0()
                        .overflow_hidden()
                        .gap(px(density.gap_inline))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(typography.t_body_sm))
                                .text_color(theme.fg_base)
                                .child(name),
                        )
                        .children(branch_chip),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("untracked-{}-{ix}-path", u.project_id)))
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(path.clone())
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(path.clone()).build(window, cx)
                        }),
                ),
        )
        .child(adopt)
}
