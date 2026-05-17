//! Status bar — 22px fixed-height bottom strip.
//!
//! Layout: `left | center | right`. Left zone shows brand + version. Center
//! shows the git branch chip + dirty count when a repository is mounted.
//! Right zone shows the pane count.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_core::GitState;
use oximux_git::PollState;
use oximux_settings::{Density, Theme, Typography};

/// Pure helper for the git zone text. Returns:
///   - `"<branch>  •  N changed"` (or `0 changed`) when Ready
///   - `"<branch>  •  ↑A ↓B  •  N changed"` when ahead/behind
///   - `"loading git…"` when Loading
///   - `"git: <err>"` when Failed
///   - `""` when no repo
pub fn git_zone_label(state: Option<&PollState>) -> String {
    match state {
        None => String::new(),
        Some(PollState::Loading) => "loading git…".to_string(),
        Some(PollState::Failed(e)) => format!("git: {e}"),
        Some(PollState::Ready(g)) => format_ready(g),
    }
}

fn format_ready(g: &GitState) -> String {
    let branch = g.branch.as_deref().unwrap_or("(detached)");
    let dirty = g.files.iter().filter(|f| !is_ignored(f)).count();
    let mut s = String::from(branch);
    if g.ahead != 0 || g.behind != 0 {
        s.push_str(&format!("  •  ↑{} ↓{}", g.ahead, g.behind));
    }
    s.push_str(&format!("  •  {dirty} changed"));
    s
}

fn is_ignored(f: &oximux_core::FileStatus) -> bool {
    matches!(f.index, oximux_core::IndexStatus::Ignored)
        || matches!(f.worktree, oximux_core::WorktreeStatus::Ignored)
}

pub fn view(
    theme: Theme,
    density: Density,
    typography: &Typography,
    pane_count: usize,
    git_state: Option<&PollState>,
) -> impl IntoElement {
    let pane_label = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
    };
    let git_label = git_zone_label(git_state);

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_status_bar))
        .px(px(density.pad_panel))
        .bg(theme.bg_panel)
        .border_t_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .flex()
                .flex_1()
                .items_center()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(format!("OxiMux v{}", env!("CARGO_PKG_VERSION"))),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .justify_center()
                .items_center()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(git_label),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .justify_end()
                .items_center()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.status_muted)
                .child(pane_label),
        )
}
