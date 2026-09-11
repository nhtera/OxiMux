//! Git & Source Control pane — how OxiMux names the branches it creates, where
//! it puts worktrees, and whether it freshens the default branch first.
//!
//! Every control applies immediately: it mutates the working copy and writes
//! `git.toml`. The preview line reads that working copy — not the saved
//! global — which is what lets it answer on the same frame as the keystroke,
//! while still running the resolver the create path runs.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{Sizable as _, input::Input};
use oximux_settings::{Density, Theme, Typography, git::BranchPrefixMode, git::GitSettings};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{
    SettingEntry, entries_card, entry, entry_stacked, entry_stacked_hinted, hint_text, notice_text,
};
use super::segmented::{Segment, segmented};
use crate::shell::workspace::configured_locator::{ConfiguredLocator, DEFAULT_ROOT_UNDER_HOME};

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
                     Existing branches are never renamed and existing worktrees are never moved.",
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
    // Against the window's active project, so `Git username` resolves the same
    // repository the New Workspace dialog will. Previewing against the global
    // git config instead showed `oximux/…` here and `ada-lovelace/…` there for
    // one setting — correct in both places and unreadable as anything but a
    // bug. `None` (no project open) still falls back to the global config.
    let preview =
        crate::git_settings::branch_for(&modal.git, modal.project_root.as_deref(), "fix-login", cx);
    rows.push(entry(
        "Preview",
        "What a worktree named \"Fix login\" would be branched as.",
        hint_text(preview, theme, typography),
    ));

    // The directory field, with Browse beside it and one line under it: the
    // refusal reason while the text is unacceptable, otherwise where the next
    // worktree will land. Validated by the same function the create path
    // runs, so a directory the pane accepts is one a create will accept.
    //
    // The line under the field goes through the entry's hint slot, NOT into
    // a flex column of our own around the field: nested that way, the
    // wrapping hint measured against an indefinite width and pushed the
    // keep-default toggle and the card's caption clean off the pane. The
    // field row itself is the schedules pane's working-directory shape.
    //
    // And that line is ONE line, truncated, not a wrapping paragraph: it
    // carries a filesystem path, which has no break opportunities, and a
    // long unbreakable token in a wrapping hint made every row after it —
    // the keep-default toggle, the card's caption — disappear from the pane
    // while the hint itself painted fine. Found live; the test text system
    // does not reproduce it. A single ellipsized line inside its own flex
    // row is the shape the codebase already knows works for nowrap text.
    if let Some(state) = modal.git_dir_input.as_ref() {
        let under = match &modal.git_dir_notice {
            Some(reason) => notice_text(false, reason.clone(), theme, typography),
            None => hint_text(landing_hint(&modal.git_dir_text(cx)), theme, typography),
        };
        let under = div()
            .flex()
            .flex_row()
            .w_full()
            .min_w_0()
            .child(div().min_w_0().truncate().child(under));
        let field_row = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(8.0))
            .child(
                div().flex_1().child(
                    Input::new(state)
                        .small()
                        .text_size(px(typography.t_body_sm))
                        .into_any_element(),
                ),
            )
            .child(value_chip(
                "git-dir-browse",
                "Browse…",
                theme,
                density,
                typography,
                |this: &mut SettingsModal, _w, cx| this.browse_git_dir(cx),
                cx,
            ));
        rows.push(entry_stacked_hinted(
            "Worktree directory",
            "Where new worktrees are created, as <directory>/<project>/<slug>. \
             Leave empty for the default. Existing worktrees stay where they are.",
            field_row,
            under,
        ));
    }

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

/// The placeholder for an empty directory field: the default root, spelled
/// the way a user would type it.
pub(super) fn default_root_placeholder() -> String {
    format!("~/{DEFAULT_ROOT_UNDER_HOME}")
}

/// Why `text` is refused as a worktree directory, or `None` when it would be
/// accepted. Empty text is the default root, which is validated too — a home
/// that is itself a git repository is refused here as it would be at create.
///
/// Runs what the create path runs, through the configured locator's own
/// root resolution, so the pane and the create cannot disagree about a
/// directory. `commit` selects the full check with the write probe; the
/// per-keystroke caller passes `false` and gets the shape rules only, which
/// read the disk and write nothing.
pub(super) fn refusal(text: &str, commit: bool) -> Option<String> {
    let settings = GitSettings {
        worktree_dir: (!text.trim().is_empty()).then(|| text.to_string()),
        ..GitSettings::shipped()
    };
    let home = dirs::home_dir();
    let root = ConfiguredLocator::root_from(&settings, home.as_deref());
    let locator = ConfiguredLocator::new(root, crate::app_paths::data_dir(), Vec::new());
    let verdict = if commit { locator.validated_root() } else { locator.root_shape() };
    verdict.err().map(|err| err.to_string())
}

/// Where the next worktree will land for the accepted `text`, with the home
/// directory spelled `~` — the way the placeholder spells it, and short enough
/// to read at a glance.
fn landing_hint(text: &str) -> String {
    let settings = GitSettings {
        worktree_dir: (!text.trim().is_empty()).then(|| text.to_string()),
        ..GitSettings::shipped()
    };
    let home = dirs::home_dir();
    let root = ConfiguredLocator::root_from(&settings, home.as_deref())
        .map(|r| match home.as_deref().and_then(|h| r.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => r.display().to_string(),
        })
        .unwrap_or_else(default_root_placeholder);
    format!("New worktrees go to {root}/<project>/<slug>.")
}
