//! Git & Source Control pane — how OxiMux names the branches it creates, where
//! it puts worktrees, and whether it freshens the default branch first.
//!
//! Every control applies immediately: it mutates the working copy and writes
//! `git.toml`. The preview line reads that working copy — not the saved
//! global — which is what lets it answer on the same frame as the keystroke,
//! while still running the resolver the create path runs.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{Sizable as _, input::Input};
use oximux_settings::{Density, Theme, Typography, git::BranchPrefixMode};

use super::SettingsModal;
use super::controls::toggle_switch;
use super::layout::{SettingEntry, entries_card, entry, entry_stacked, hint_text};
use super::segmented::{Segment, segmented};

/// Render the Git pane: the settings rows plus a quiet save-location caption.
pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(entries_card(
            theme,
            density,
            typography,
            entries(modal, theme, density, typography, cx),
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(
                    "Changes save to git.toml and apply to the next worktree you create. \
                     Existing branches are never renamed.",
                ),
        )
        .into_any_element()
}

/// The Git pane's settings as reusable entries. Used by the pane render and by
/// global search.
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut rows: Vec<SettingEntry> = Vec::new();

    let mode = modal.git.branch_prefix;
    let prefix_control = segmented(
        "git-branch-prefix",
        BranchPrefixMode::ALL
            .iter()
            .map(|m| {
                let m = *m;
                Segment::new(m.label(), mode == m, move |this: &mut SettingsModal, _w, cx| {
                    this.git.branch_prefix = m;
                    this.persist_git(cx);
                })
            })
            .collect(),
        theme,
        density,
        typography,
        cx,
    );
    rows.push(entry(
        "Branch prefix",
        "What goes in front of the slug when OxiMux creates a worktree branch.",
        prefix_control,
    ));

    // The custom field is offered in every mode rather than hidden outside
    // `Custom`: a control that vanishes when you click away from it takes the
    // text with it, and the setting deliberately keeps that text. It is
    // disabled instead, which says the same thing without destroying anything.
    if let Some(state) = modal.git_prefix_input.as_ref() {
        rows.push(entry_stacked(
            "Custom prefix",
            "Used when Branch prefix is set to Custom.",
            Input::new(state)
                .small()
                .disabled(mode != BranchPrefixMode::Custom)
                .text_size(px(typography.t_body_sm))
                .into_any_element(),
        ));
    }

    // Previewed from the WORKING COPY, not the saved global — that is what
    // makes it live. Every keystroke in the custom field and every segment
    // click mutates `modal.git`, so the line answers on the same frame. It is
    // still the one resolver the create path runs, over the same cached
    // username, so agreeing here is not a coincidence.
    //
    // No project root: the pane is app-wide and does not know which project a
    // window is on, so `Git username` previews against the global git config.
    // A repo that overrides `user.name` will differ, and the dialog's own
    // preview line — which does have the root — is the one that governs.
    let preview = crate::git_settings::branch_for(&modal.git, None, "fix-login", cx);
    rows.push(entry(
        "Preview",
        "What a worktree named \"Fix login\" would be branched as.",
        hint_text(preview, theme, typography),
    ));

    // Phase 6 owns the seam this field writes to. Rendered disabled rather
    // than hidden: a pane that grows a field later reads as less finished than
    // one that shows what is coming, and the note says why it does nothing.
    rows.push(entry(
        "Worktree directory",
        "Where new worktrees are created. Not configurable yet — worktrees go in \
         OxiMux's own data directory.",
        hint_text(
            modal
                .git
                .worktree_dir
                .clone()
                .unwrap_or_else(|| "OxiMux data directory".to_string()),
            theme,
            typography,
        ),
    ));

    let keep_fresh = toggle_switch(
        "git-keep-default-fresh",
        modal.git.keep_default_up_to_date,
        theme,
        |this: &mut SettingsModal, _w, cx| {
            this.git.keep_default_up_to_date = !this.git.keep_default_up_to_date;
            this.persist_git(cx);
        },
        cx,
    );
    rows.push(entry(
        "Keep default branch up to date",
        "Before creating a worktree, fetch and fast-forward the default branch. \
         Skipped when it has uncommitted changes or local-only commits.",
        keep_fresh,
    ));

    rows
}
