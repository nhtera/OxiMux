//! Empty-state card for the Source Control panel.
//!
//! Rendered when the working tree is clean (no staged / unstaged /
//! untracked files) AND there are no unresolved conflicts. Replaces
//! the bare "No changes" text the embedded `GitPanel` would otherwise
//! render with a small icon + three contextual CTAs that surface the
//! most-likely next actions for an idle repo:
//!
//! 1. **Switch Branch** — opens the existing branch picker (the same
//!    surface the toolbar's branch chip dispatches into).
//! 2. **Fetch** — runs a fetch via the commit area's single-flight
//!    remote-op pipeline.
//! 3. **View Graph** — switches the scope tab to All so the commit
//!    graph dominates the panel.
//!
//! The card is mounted in `SourceControlPanel::render` via
//! `render_empty_state(cx)` and slotted between the filter row and
//! the file list. When the tree is dirty (or a conflict is pending)
//! the card is omitted entirely and the standard file-list / conflict
//! banner takes over.

use gpui::{AnyElement, ClickEvent, Context, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use crate::shell::source_control::SourceControlPanel;
use crate::shell::source_control::commit_ops::{self, RemoteVerb};
use crate::shell::source_control::scope::SourceControlScope;

impl SourceControlPanel {
    /// Returns `true` when the tree is clean AND there's a recorded
    /// HEAD (i.e. not a brand-new repo with zero commits) AND there
    /// are no unresolved conflicts. The card stays hidden in those
    /// edge cases — a fresh repo with no commits should show the
    /// usual "Stage everything" affordance the file list provides,
    /// and a conflict state defers to the v1 ConflictSummaryCard.
    pub(super) fn should_show_empty_state(&self) -> bool {
        let Some(state) = self.git_state.as_ref() else {
            return false;
        };
        if state.head_oid.is_none() {
            return false;
        }
        let has_conflict = state.files.iter().any(|f| f.conflict_kind.is_some());
        if has_conflict {
            return false;
        }
        // Filter Ignored entries the same way `build_primary_inputs`,
        // `partition_files`, and the Uncommitted-tab badge count do.
        // `git_state.files` carries `target/`, `node_modules/`, and
        // anything else matched by `.gitignore` because the poller
        // asks for them — but the user never thinks of them as
        // "uncommitted changes". Without this filter, `state.files`
        // is never empty on a real repo and the rich empty-state
        // card (branch icon + headline + 3 CTAs) never paints; the
        // user falls through to `GitPanel`'s plainer internal
        // placeholder instead, which is what's been happening.
        use oximux_core::{IndexStatus, WorktreeStatus};
        !state.files.iter().any(|f| {
            !matches!(f.index, IndexStatus::Ignored)
                && !matches!(f.worktree, WorktreeStatus::Ignored)
        })
    }

    pub(super) fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let typography = self.typography.clone();
        let style = self.style();

        // Three CTAs: each is a small ghost button. The cluster sits
        // inside a column with the icon + "No changes" headline so the
        // user can scan top-to-bottom: icon → state → action.
        let switch_btn = Button::new("sc-empty-switch")
            .ghost()
            .xsmall()
            .label("Switch Branch")
            .tooltip("Switch to another local branch")
            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                panel.open_switch_picker(window, cx);
            }));

        let fetch_btn = Button::new("sc-empty-fetch")
            .ghost()
            .xsmall()
            .label("Fetch")
            .tooltip("Fetch from remote without merging")
            .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                panel.commit_area.update(cx, |area, cx| {
                    commit_ops::run_remote(area, RemoteVerb::Fetch, cx);
                });
            }));

        let graph_btn = Button::new("sc-empty-graph")
            .ghost()
            .xsmall()
            .label("View Graph")
            .tooltip("Switch to All tab to browse the commit graph")
            .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                panel.select_scope(SourceControlScope::All, cx);
            }));

        // Card-level spacing: 10px vertical gap between icon, headline,
        // and CTA cluster (slightly tighter than the global gap_inline
        // because the three elements visually belong together as one
        // unit); 28px outer y-padding so the card breathes inside the
        // panel without floating in the middle of a sparse layout.
        // Both values are local to this empty-state component and
        // documented inline rather than promoted to density tokens.
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .py(px(28.0))
            .px(px(style.pad_h))
            .child(
                // Branch icon stands in for "this branch is up to date".
                // 20px reads as a panel-level glyph without crowding the
                // headline beneath it.
                Icon::default()
                    .path("icons/git-branch.svg")
                    .size(px(20.0))
                    .text_color(theme.fg_subtle),
            )
            .child(
                div()
                    .text_size(px(style.body_text))
                    .text_color(theme.fg_muted)
                    .font_weight(typography.w_medium)
                    .child("No changes on this branch"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(switch_btn)
                    .child(fetch_btn)
                    .child(graph_btn),
            )
            .into_any_element()
    }
}
