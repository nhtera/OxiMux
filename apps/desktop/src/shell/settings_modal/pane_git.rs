//! Git & Source Control pane — how OxiMux names the branches it creates, where
//! it puts worktrees, and whether it freshens the default branch first.
//!
//! Every control applies immediately: it mutates the working copy and writes
//! `git.toml`. The preview line reads that working copy — not the saved
//! global — which is what lets it answer on the same frame as the keystroke,
//! while still running the resolver the create path runs.

use gpui::{
    Anchor, AnyElement, ClickEvent, Entity, IntoElement, ParentElement, SharedString, Styled, div,
    px,
};
use gpui_component::button::Button;
// A plain `Button` carrying its own menu, not `DropdownButton` — see the
// schedules pane for why: that widget hangs the menu off the chevron alone.
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{Sizable as _, input::Input};
use oximux_settings::{
    Density, OpenInApp, Theme, Typography, git::BranchPrefixMode, git::GitSettings,
};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{
    SettingEntry, entries_card, entry, entry_stacked, entry_stacked_hinted, hint_text, notice_text,
};
use super::segmented::{Segment, segmented};
use crate::shell::left_rail::open_in;
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

    rows.extend(open_in_entries(modal, theme, density, typography, cx));

    rows
}

/// The `Open in` section: the add form under its own label, then one row per
/// app the workspace row menu will offer, each with `Remove`.
///
/// The rows show the *effective* list — the built-in one until the user
/// edits it — so what the pane lists is what the menu offers. The first
/// edit materialises the built-in list into `git.toml`; `Reset` empties it
/// again, which is the spelling of "use the built-in list".
fn open_in_entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut rows: Vec<SettingEntry> = Vec::new();
    let configured = !modal.git.open_in.is_empty();

    if let (Some(name), Some(command)) =
        (modal.git_open_in_name_input.as_ref(), modal.git_open_in_cmd_input.as_ref())
    {
        let form = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(8.0))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(name).small().text_size(px(typography.t_body_sm)).into_any_element(),
                ),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(command)
                        .small()
                        .text_size(px(typography.t_body_sm))
                        .into_any_element(),
                ),
            )
            .child(presets_dropdown(cx.entity()))
            .child(value_chip(
                "git-open-in-add",
                "Add",
                theme,
                density,
                typography,
                |this: &mut SettingsModal, window, cx| this.add_open_in_app(window, cx),
                cx,
            ));
        let under = match &modal.git_open_in_notice {
            Some(reason) => notice_text(false, reason.clone(), theme, typography),
            None if configured => hint_text(
                "Your own list. Remove every app, or Reset, to go back to the built-in one.",
                theme,
                typography,
            ),
            None => hint_text(
                "The built-in list: your file manager plus the editors found on this machine. \
                 Adding or removing an app makes the list your own.",
                theme,
                typography,
            ),
        };
        rows.push(entry_stacked_hinted(
            "Open in",
            "Apps a workspace row's Open in \u{25b8} menu offers. The worktree directory is \
             passed as the last argument; wrap an argument with spaces in double quotes.",
            form,
            under,
        ));
    }

    for (idx, app) in modal.git_open_in_shown.iter().enumerate() {
        rows.push(entry(
            app.name.clone(),
            app.command.clone(),
            value_chip(
                ("git-open-in-remove", idx),
                "Remove",
                theme,
                density,
                typography,
                move |this: &mut SettingsModal, _w, cx| this.remove_open_in_app(idx, cx),
                cx,
            ),
        ));
    }

    if configured {
        rows.push(entry(
            "Built-in list",
            "Forget the apps above and offer the built-in list again.",
            value_chip(
                "git-open-in-reset",
                "Reset",
                theme,
                density,
                typography,
                |this: &mut SettingsModal, _w, cx| this.reset_open_in_apps(cx),
                cx,
            ),
        ));
    }

    rows
}

/// The `Presets ▾` dropdown beside the add form: one row per editor this
/// build knows how to launch. Picking one fills the two fields; `Add` is
/// still the commit.
fn presets_dropdown(entity: Entity<SettingsModal>) -> AnyElement {
    let presets = open_in::presets();
    Button::new(SharedString::from("git-open-in-presets"))
        .label("Presets")
        .small()
        .outline()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            for app in presets.clone() {
                let entity = entity.clone();
                let label = app.name.clone();
                menu = menu.item(
                    PopupMenuItem::element(move |_w, _cx| {
                        div().min_w(px(96.0)).child(label.clone())
                    })
                    .on_click(window.listener_for(
                        &entity,
                        move |m: &mut SettingsModal, _ev: &ClickEvent, window, cx| {
                            m.prefill_open_in_app(&app, window, cx);
                        },
                    )),
                );
            }
            menu
        })
        .into_any_element()
}

/// Append `name` / `command` to `list`, materialising the built-in list
/// first when `list` is empty so the addition lands *beside* the defaults
/// rather than replacing them. Refuses a blank name, a blank command, or a
/// command that cannot be split into a program (an unterminated quote).
pub(super) fn add_open_in(
    list: &mut Vec<OpenInApp>,
    name: &str,
    command: &str,
    defaults: impl FnOnce() -> Vec<OpenInApp>,
) -> Result<(), String> {
    let name = name.trim();
    let command = command.trim();
    if name.is_empty() {
        return Err("Give the app a name.".to_string());
    }
    if command.is_empty() {
        return Err("Give the app a command.".to_string());
    }
    if open_in::parse_command(command).is_none() {
        return Err("The command needs a program name, with every quote closed.".to_string());
    }
    if list.is_empty() {
        *list = defaults();
    }
    list.push(OpenInApp { name: name.to_string(), command: command.to_string() });
    Ok(())
}

/// Remove the `idx`-th app of the effective list, materialising the built-in
/// list first when `list` is empty — the index the pane clicked is an index
/// into what it showed. Out of range is a no-op.
pub(super) fn remove_open_in(
    list: &mut Vec<OpenInApp>,
    idx: usize,
    defaults: impl FnOnce() -> Vec<OpenInApp>,
) {
    if list.is_empty() {
        *list = defaults();
    }
    if idx < list.len() {
        list.remove(idx);
    }
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

#[cfg(test)]
mod open_in_tests {
    use super::*;

    fn app(name: &str, command: &str) -> OpenInApp {
        OpenInApp { name: name.into(), command: command.into() }
    }

    fn built_in() -> Vec<OpenInApp> {
        vec![app("Finder", "open"), app("Zed", "zed")]
    }

    /// Adding to an untouched list keeps the built-in apps: the user asked
    /// for one more editor, not for a list with only that editor in it.
    #[test]
    fn the_first_add_materialises_the_built_in_list_beside_the_new_app() {
        let mut list = Vec::new();
        add_open_in(&mut list, "Cursor", "cursor", built_in).expect("added");
        assert_eq!(list, vec![app("Finder", "open"), app("Zed", "zed"), app("Cursor", "cursor")]);
    }

    #[test]
    fn a_later_add_appends_without_touching_the_defaults() {
        let mut list = vec![app("Only", "only")];
        add_open_in(&mut list, " Cursor ", " cursor ", || panic!("defaults not consulted"))
            .expect("added");
        assert_eq!(list, vec![app("Only", "only"), app("Cursor", "cursor")]);
    }

    #[test]
    fn a_blank_name_or_command_is_refused_and_writes_nothing() {
        let mut list = Vec::new();
        assert!(add_open_in(&mut list, "", "code", built_in).is_err());
        assert!(add_open_in(&mut list, "VS Code", "   ", built_in).is_err());
        assert!(list.is_empty(), "a refused add must not materialise the defaults");
    }

    /// The one command shape `launch` cannot split is refused here, so the
    /// menu never offers an entry that fails on click.
    #[test]
    fn an_unterminated_quote_is_refused() {
        let mut list = Vec::new();
        let err = add_open_in(&mut list, "Code", "open -a \"Visual Studio", built_in).unwrap_err();
        assert!(err.contains("quote"), "{err}");
        assert!(list.is_empty());
    }

    /// `""` and `" "` are non-blank text whose program is nothing; the
    /// parser refuses them, so the pane must too rather than adding an entry
    /// that fails on click.
    #[test]
    fn a_quoted_empty_program_is_refused() {
        let mut list = Vec::new();
        assert!(add_open_in(&mut list, "App", "\"\"", built_in).is_err());
        assert!(add_open_in(&mut list, "App", "\" \"", built_in).is_err());
        assert!(list.is_empty());
    }

    /// Removing from the built-in list must remove the app the user clicked,
    /// which means materialising the same list the pane showed.
    #[test]
    fn removing_from_the_built_in_list_materialises_it_first() {
        let mut list = Vec::new();
        remove_open_in(&mut list, 0, built_in);
        assert_eq!(list, vec![app("Zed", "zed")]);
    }

    #[test]
    fn removing_out_of_range_is_a_no_op() {
        let mut list = vec![app("Only", "only")];
        remove_open_in(&mut list, 5, || panic!("defaults not consulted"));
        assert_eq!(list, vec![app("Only", "only")]);
    }

    /// Removing the last app empties the list, and an empty list means the
    /// built-in one — the documented way back, stated in the pane's hint.
    #[test]
    fn removing_the_last_app_returns_to_the_built_in_list() {
        let mut list = vec![app("Only", "only")];
        remove_open_in(&mut list, 0, || panic!("defaults not consulted"));
        assert!(list.is_empty());
        assert_eq!(open_in::effective_apps(&GitSettings::shipped()), open_in::default_apps());
    }
}
