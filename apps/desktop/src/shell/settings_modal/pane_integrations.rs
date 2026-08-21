//! Integrations pane — the external CLIs OxiMux depends on, and how to fix
//! the ones that are missing.
//!
//! **The remediation is the point.** A status list that diagnoses a problem
//! and then tells you to go and solve it elsewhere has moved the work without
//! reducing it. So the row that says `gh` is missing is the row that installs
//! it, the row that says it is signed out is the row that tells you the one
//! command that signs it in, and the row that cannot do either still hands you
//! the exact command to paste.
//!
//! **Nothing here installs itself.** Every install is a click, and the button
//! says which package manager it will use, because a settings pane that runs a
//! package manager on its own would be a surprise on someone else's machine.
//!
//! The probing lives in [`crate::shell::integrations`]; this file is the
//! surface and the copy.

use gpui::{
    AnyElement, ClipboardItem, Context, Hsla, IntoElement, ParentElement, SharedString, Styled,
    div, prelude::FluentBuilder as _, px,
};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::value_chip;
use super::layout::{SettingEntry, entries_card, entry, section_title};
use crate::shell::integrations::catalog::Tool;
use crate::shell::integrations::install::InstallUi;
use crate::shell::integrations::probe::Health;
use crate::shell::integrations::{IntegrationRow, unhealthy_count};

/// Status word and colour for one tool.
///
/// `Checking` is grey rather than amber on purpose: the first paint of the
/// pane shows it for every row, and four amber pills that resolve to green a
/// moment later teach the user to ignore the colour.
pub(super) fn health_pill(row: &IntegrationRow, theme: Theme) -> (String, Hsla) {
    match &row.health {
        Health::Checking => ("Checking…".to_string(), theme.fg_subtle),
        Health::Ready { version } => (
            match version {
                Some(v) => format!("Ready · {v}"),
                None => "Ready".to_string(),
            },
            theme.status_ok,
        ),
        Health::SignedOut { .. } => ("Not signed in".to_string(), theme.status_warn),
        Health::Missing if row.tool.is_required() => {
            ("Not installed".to_string(), theme.status_error)
        }
        Health::Missing => ("Not installed".to_string(), theme.status_warn),
    }
}

/// The sentence under a row: what is wrong, or what the tool is for.
///
/// When something is wrong the description is *replaced*, not appended to. A
/// row that says both "here is what this does" and "here is why it is broken"
/// buries the second in the first, and the second is the only reason the user
/// is reading this pane.
pub(super) fn row_detail(row: &IntegrationRow) -> String {
    match &row.health {
        Health::Checking => format!("Looking for {}…", row.tool.binary()),
        Health::Ready { .. } => row.tool.needed_for().to_string(),
        Health::SignedOut { .. } => match row.tool.sign_in_command() {
            Some(cmd) => format!(
                "Installed, but not signed in — every call fails until it is. \
                 Run `{cmd}` in a terminal."
            ),
            None => "Installed, but not signed in.".to_string(),
        },
        Health::Missing => format!("Not on PATH. {}", row.tool.needed_for()),
    }
}

/// The header line above the cards.
pub(super) fn summary(rows: &[IntegrationRow]) -> String {
    if rows.iter().any(|r| matches!(r.health, Health::Checking)) {
        return "Checking what this machine has…".to_string();
    }
    match unhealthy_count(rows) {
        0 => "Everything OxiMux shells out to is present and working.".to_string(),
        1 => "1 tool needs attention — the surfaces below it will stay quiet until it does."
            .to_string(),
        n => format!(
            "{n} tools need attention — the surfaces below them will stay quiet until they do."
        ),
    }
}

/// Label for the install button, naming the manager so the click is not a
/// surprise.
pub(super) fn install_label(row: &IntegrationRow) -> Option<String> {
    row.can_install()
        .then(|| row.tool.install_recipe().map(|r| format!("Install with {}", r.manager)))
        .flatten()
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let rows = entries(modal, theme, density, typography, cx);
    let recheck = value_chip(
        "integrations-recheck",
        "Re-check",
        theme,
        density,
        typography,
        |m: &mut SettingsModal, _w, cx| m.refresh_integrations(cx),
        cx,
    );
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap(px(density.gap_inline))
                .w_full()
                // The heading claims the free space and is allowed to shrink;
                // without both, a long summary pushes the chip off the card.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(section_title(
                            "Integrations",
                            summary(&modal.integrations),
                            theme,
                            typography,
                        )),
                )
                .child(div().flex_none().child(recheck)),
        )
        .child(super::layout::card_surface(
            theme,
            density,
            entries_card(theme, density, typography, rows),
        ))
        .child(footnote(theme, typography))
        .into_any_element()
}

/// The one thing the cards cannot say for themselves.
fn footnote(theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(
            "OxiMux never installs anything on its own. Each button runs the command \
             it names, and nothing else.",
        )
        .into_any_element()
}

pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    modal
        .integrations
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            entry(
                row.tool.name(),
                row_detail(row),
                controls(modal, idx, row, theme, density, typography, cx),
            )
        })
        .collect()
}

/// The right-hand side of one row: the status pill, then whatever action the
/// state calls for.
#[allow(clippy::too_many_arguments)]
fn controls(
    modal: &SettingsModal,
    idx: usize,
    row: &IntegrationRow,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let tool = row.tool;
    let (label, color) = health_pill(row, theme);
    let install_ui = modal.integration_install.get(&idx);
    let running = matches!(install_ui, Some(InstallUi::Running));

    let mut col = div()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(color)
                .child(label),
        );

    // A failed install keeps the manager's own words. Above the buttons, not
    // in a toast: the user is about to decide between retrying and doing it by
    // hand, and that decision needs the reason in front of it.
    if let Some(InstallUi::Failed { message }) = install_ui {
        col = col.child(
            div()
                .max_w(px(320.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.status_error)
                .child(SharedString::from(message.clone())),
        );
    }

    let mut actions = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .justify_end()
        .items_center()
        .gap(px(density.gap_inline));

    if running {
        actions = actions
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(SharedString::from(format!(
                        "Installing {}…",
                        tool.binary()
                    ))),
            )
            .child(value_chip(
                SharedString::from(format!("integration-cancel-{idx}")),
                "Cancel",
                theme,
                density,
                typography,
                move |m: &mut SettingsModal, _w, cx| m.cancel_integration_install(idx, cx),
                cx,
            ));
    } else {
        if let Some(label) = install_label(row) {
            actions = actions.child(value_chip(
                SharedString::from(format!("integration-install-{idx}")),
                label,
                theme,
                density,
                typography,
                move |m: &mut SettingsModal, _w, cx| m.start_integration_install(idx, cx),
                cx,
            ));
        }
        // Always offered when there is a recipe, whether or not the manager is
        // here. On a machine with no package manager this *is* the
        // remediation; on one with a manager it is the escape hatch for an
        // install that failed for a reason the pane cannot fix (elevation, a
        // corporate proxy).
        if let Some(recipe) = tool.install_recipe().filter(|_| !row.health.is_ready()) {
            let command = recipe.command_line();
            actions = actions.child(value_chip(
                SharedString::from(format!("integration-copy-{idx}")),
                "Copy command",
                theme,
                density,
                typography,
                move |m: &mut SettingsModal, _w, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                    m.integration_copied = Some(idx);
                    cx.notify();
                },
                cx,
            ));
        }
        actions = actions.child(value_chip(
            SharedString::from(format!("integration-docs-{idx}")),
            "Docs",
            theme,
            density,
            typography,
            move |_m: &mut SettingsModal, _w, cx| {
                crate::shell::open_url::open_url(tool.docs_url(), cx);
            },
            cx,
        ));
    }

    col = col.child(actions).when(
        modal.integration_copied == Some(idx) && !running,
        |c| {
            c.child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.status_ok)
                    .child("Copied"),
            )
        },
    );
    col.into_any_element()
}

/// How often a running install is checked for completion.
///
/// Half a second: a package manager takes tens of seconds, so this is about
/// how quickly the card stops saying "Installing…" once it is done, not about
/// tracking progress. There is no progress to track — the managers report
/// theirs to a console this process does not have.
const INSTALL_POLL: std::time::Duration = std::time::Duration::from_millis(500);

impl SettingsModal {
    /// Re-probe every tool on a background thread.
    ///
    /// Called on each modal open, for the same reason the driver status is:
    /// "not installed" is precisely the answer most likely to have gone stale,
    /// because the user's response to seeing it is to go and install the thing.
    pub(crate) fn refresh_integrations(&mut self, cx: &mut Context<Self>) {
        self.sweep_integrations(Vec::new(), cx);
    }

    /// The sweep, with the rows an install just claimed to have fixed.
    ///
    /// `expect_ready` is what turns a successful-but-invisible install into a
    /// sentence instead of a shrug. Without it, a `winget` that exits zero and
    /// leaves the row still saying "Not installed" tells the user nothing about
    /// which of the two things went wrong.
    fn sweep_integrations(&mut self, expect_ready: Vec<usize>, cx: &mut Context<Self>) {
        if self.integrations.is_empty() {
            self.integrations = Tool::ALL.into_iter().map(IntegrationRow::checking).collect();
        }
        self.integration_copied = None;
        let refresh_path = !expect_ready.is_empty();
        cx.spawn(async move |weak, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move {
                    if refresh_path {
                        crate::shell::integrations::refresh_path_and_probe_all()
                    } else {
                        crate::shell::integrations::probe_all()
                    }
                })
                .await;
            let _ = weak.update(cx, |modal, cx| {
                modal.integrations = rows;
                for idx in expect_ready {
                    match modal.integrations.get(idx) {
                        // The install worked and the tool is visible. The green
                        // pill is the whole report.
                        Some(row) if row.health.is_ready() => {
                            modal.integration_install.remove(&idx);
                        }
                        Some(row) => {
                            // Exit zero, still nothing there. On Windows this
                            // is a PATH that even the refresh could not pick
                            // up; anywhere else it is a package that installed
                            // something under a name we do not look for.
                            modal.integration_install.insert(
                                idx,
                                InstallUi::Failed {
                                    message: format!(
                                        "Installed, but `{}` still is not on PATH.                                          Restarting OxiMux usually picks it up.",
                                        row.tool.binary()
                                    ),
                                },
                            );
                        }
                        None => {}
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Run the row's install recipe. A second click while one is running is a
    /// no-op — package managers take a machine-wide lock, and two racing
    /// installs produce an error that describes neither.
    pub(super) fn start_integration_install(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.integration_handles.contains_key(&idx) {
            return;
        }
        let Some(row) = self.integrations.get(idx) else {
            return;
        };
        if !row.can_install() {
            return;
        }
        let Some(recipe) = row.tool.install_recipe() else {
            return;
        };
        tracing::info!(
            tool = row.tool.binary(),
            command = %recipe.command_line(),
            "integrations: starting install"
        );
        self.integration_handles
            .insert(idx, crate::shell::integrations::install::begin(recipe));
        self.integration_install.insert(idx, InstallUi::Running);
        self.integration_copied = None;
        self.start_integration_poll(cx);
        cx.notify();
    }

    pub(super) fn cancel_integration_install(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(handle) = self.integration_handles.get(&idx) {
            handle.cancel();
        }
        cx.notify();
    }

    /// Keep one poll loop alive while any install is running.
    fn start_integration_poll(&mut self, cx: &mut Context<Self>) {
        if self.integration_poll_running {
            return;
        }
        self.integration_poll_running = true;
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(INSTALL_POLL).await;
                match weak.update(cx, |modal, cx| modal.pump_integration_installs(cx)) {
                    Ok(true) => continue,
                    // Either everything settled, or the modal is gone.
                    _ => break,
                }
            }
            let _ = weak.update(cx, |modal, _| modal.integration_poll_running = false);
        })
        .detach();
    }

    /// Collect finished installs. Returns whether any are still running.
    fn pump_integration_installs(&mut self, cx: &mut Context<Self>) -> bool {
        let finished: Vec<(usize, Result<(), String>)> = self
            .integration_handles
            .iter()
            .filter_map(|(idx, handle)| {
                crate::shell::integrations::install::poll(handle).map(|result| (*idx, result))
            })
            .collect();
        let mut installed: Vec<usize> = Vec::new();
        for (idx, result) in &finished {
            self.integration_handles.remove(idx);
            match result {
                Ok(()) => {
                    // No success banner: the pill going green *is* the result,
                    // and it is the thing the user was watching. Held open
                    // until the re-probe confirms it, though — a manager's exit
                    // code is a claim, not an observation.
                    self.integration_install.remove(idx);
                    installed.push(*idx);
                }
                Err(message) => {
                    self.integration_install
                        .insert(*idx, InstallUi::Failed { message: message.clone() });
                }
            }
        }
        if !finished.is_empty() {
            // One sweep covers the whole batch. It is also what turns a
            // successful install into a green pill without a restart: the
            // install wrote a binary, and only a fresh probe — behind a fresh
            // PATH — knows that.
            self.sweep_integrations(installed, cx);
        }
        !self.integration_handles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::integrations::IntegrationRow;

    fn row(tool: Tool, health: Health, manager_ready: bool) -> IntegrationRow {
        IntegrationRow {
            tool,
            health,
            manager_ready,
        }
    }

    fn theme() -> Theme {
        Theme::charcoal()
    }

    #[test]
    fn a_ready_tool_shows_its_version_in_the_pill() {
        let r = row(
            Tool::Git,
            Health::Ready {
                version: Some("2.35.0".into()),
            },
            false,
        );
        let (label, color) = health_pill(&r, theme());
        assert_eq!(label, "Ready · 2.35.0");
        assert_eq!(color, theme().status_ok);
    }

    #[test]
    fn a_missing_required_tool_reads_as_an_error_not_a_warning() {
        // Without git the app is broken, not merely diminished.
        let (_, git) = health_pill(&row(Tool::Git, Health::Missing, false), theme());
        let (_, gh) = health_pill(&row(Tool::GithubCli, Health::Missing, false), theme());
        assert_eq!(git, theme().status_error);
        assert_eq!(gh, theme().status_warn);
    }

    #[test]
    fn checking_is_grey_so_the_first_paint_does_not_cry_wolf() {
        let (label, color) = health_pill(&row(Tool::Git, Health::Checking, false), theme());
        assert_eq!(label, "Checking…");
        assert_eq!(color, theme().fg_subtle);
    }

    #[test]
    fn a_broken_row_replaces_its_description_rather_than_appending() {
        let ready = row(Tool::GithubCli, Health::Ready { version: None }, false);
        let missing = row(Tool::GithubCli, Health::Missing, true);
        assert_eq!(row_detail(&ready), Tool::GithubCli.needed_for());
        let broken = row_detail(&missing);
        assert!(broken.starts_with("Not on PATH"), "{broken}");
    }

    #[test]
    fn a_signed_out_row_names_the_command_that_fixes_it() {
        let detail = row_detail(&row(
            Tool::GithubCli,
            Health::SignedOut { version: None },
            false,
        ));
        assert!(
            detail.contains("gh auth login"),
            "the fix is one command; not naming it wastes the whole row: {detail}"
        );
    }

    #[test]
    fn the_install_button_names_the_manager_it_will_run() {
        let r = row(Tool::GithubCli, Health::Missing, true);
        let label = install_label(&r).expect("installable");
        assert!(
            label.contains("winget") || label.contains("brew"),
            "a button that runs a package manager must say which: {label}"
        );
    }

    #[test]
    fn no_install_button_without_a_manager_to_run_it() {
        assert!(install_label(&row(Tool::GithubCli, Health::Missing, false)).is_none());
    }

    #[test]
    fn no_install_button_for_a_tool_that_is_merely_signed_out() {
        let r = row(Tool::GithubCli, Health::SignedOut { version: None }, true);
        assert!(
            install_label(&r).is_none(),
            "it is installed; re-installing it changes nothing"
        );
    }

    #[test]
    fn the_summary_counts_only_settled_problems() {
        let checking: Vec<_> = Tool::ALL
            .into_iter()
            .map(|t| row(t, Health::Checking, false))
            .collect();
        assert!(summary(&checking).contains("Checking"));

        let healthy: Vec<_> = Tool::ALL
            .into_iter()
            .map(|t| row(t, Health::Ready { version: None }, false))
            .collect();
        assert!(summary(&healthy).contains("present and working"));
    }

    #[test]
    fn the_summary_is_plural_aware() {
        let one = vec![
            row(Tool::Git, Health::Ready { version: None }, false),
            row(Tool::GithubCli, Health::Missing, true),
        ];
        assert!(summary(&one).starts_with("1 tool needs"), "{}", summary(&one));

        let two = vec![
            row(Tool::GithubCli, Health::Missing, true),
            row(Tool::GitlabCli, Health::SignedOut { version: None }, false),
        ];
        assert!(summary(&two).starts_with("2 tools need"), "{}", summary(&two));
    }
}
