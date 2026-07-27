//! Screen Control pane — the master switch, the driver's health, the projects
//! opted in, and the apps that no longer raise a card.
//!
//! Nothing here is on by default and nothing here turns itself on. The pane's
//! job is to make the state legible: whether the driver is installed and
//! trusted, which projects can use it, and exactly which apps the user has
//! already said yes to — because a grant given once in a card, months ago, is
//! only revocable if it can be found.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry, section_title};

/// What `prepare()` reported about the installed driver, resolved when the
/// modal opens rather than per repaint — it spawns `codesign`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverStatus {
    /// Not looked for yet (the modal has never been opened).
    Unknown,
    Ready { version: String },
    /// Named as its own state rather than folded into `Problem`: it is the one
    /// the user can fix by installing something, and it is by far the most
    /// common.
    NotInstalled,
    Problem { detail: String },
}

impl DriverStatus {
    /// Look for the driver and put it through every gate.
    pub(crate) fn resolve() -> Self {
        match oximux_computer_use::prepare() {
            Ok(driver) => DriverStatus::Ready {
                version: driver.version.to_string(),
            },
            Err(oximux_computer_use::Error::NotFound { .. }) => DriverStatus::NotInstalled,
            Err(err) => DriverStatus::Problem {
                detail: err.to_string(),
            },
        }
    }

    fn summary(&self) -> SharedString {
        match self {
            DriverStatus::Unknown => "Checking…".into(),
            DriverStatus::Ready { version } => format!("Installed, verified ({version})").into(),
            DriverStatus::NotInstalled => "Not installed".into(),
            // The specific reason, not "unavailable": "signed by team X" and
            // "older than the required version" call for opposite responses.
            DriverStatus::Problem { detail } => detail.clone().into(),
        }
    }
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(16.0))
        .child(entries_card(
            theme,
            density,
            typography,
            entries(modal, theme, density, typography, cx),
        ));

    let projects = project_rows(modal, theme, density, typography, cx);
    if !projects.is_empty() {
        body = body
            .child(section_title(
                "Projects",
                // Deliberately "get the tools" rather than "may drive apps":
                // the list decides who is handed the screen-control tools, not
                // who can reach the screen. See `footnotes`.
                "These projects get the screen-control tools. Turn one on from its own window.",
                theme,
                typography,
            ))
            .child(entries_card(theme, density, typography, projects));
    }

    let apps = app_rows(modal, theme, density, typography, cx);
    body = body
        .child(section_title(
            "Approved apps",
            if apps.is_empty() {
                "None yet. Choosing “Always allow” on a consent card adds one here."
            } else {
                "These no longer raise a card. Remove one to be asked again."
            },
            theme,
            typography,
        ))
        .child(entries_card(theme, density, typography, apps));

    body.child(footnotes(theme, typography)).into_any_element()
}

/// The two things a user reading this pane could otherwise get wrong.
///
/// The first is the older one: an agent with a shell has routes to the same
/// driver that no list here can close, so this is a guard against mistakes.
///
/// The second was measured during the coverage spike, and it is the reason the
/// per-project wording elsewhere in this pane had to change. Turning screen
/// control on requires OxiMux to hold macOS Accessibility — the Escape kill
/// switch is an event tap, and a tap does not exist without it. macOS attributes
/// that grant to OxiMux as the responsible process and **every descendant
/// inherits it**, which includes each agent's shell tool, in every project,
/// whether or not that project appears above. The list below controls which
/// projects are handed the screen-control *tools*. It does not, and cannot,
/// fence off the permission.
///
/// One line per sentence: prose in a settings pane does not wrap in this app, so
/// a paragraph would clip at the pane edge rather than reflow.
const FOOTNOTES: &[&str] = &[
    "Guards against an agent's mistakes, not an agent trying to get around them.",
    "Enabling this grants OxiMux macOS Accessibility, needed for Esc to stop an agent.",
    "Every agent's shell inherits that grant — in all projects, not only those listed.",
];

fn footnotes(theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pt(px(4.0))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .children(FOOTNOTES.iter().map(|note| div().child(*note)))
        .into_any_element()
}

pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![
        entry(
            "Enable screen control",
            // No longer "each project opts in separately" — true of the toggle,
            // but it read as a promise that the capability stays inside the
            // project, which the Accessibility grant does not honour.
            "Let agents click and type in other apps. Off until you turn it on here.",
            toggle_switch(
                "screen-enabled",
                modal.computer_use.enabled,
                theme,
                |this, _w, cx| {
                    this.computer_use.enabled = !this.computer_use.enabled;
                    this.persist_computer_use(cx);
                },
                cx,
            ),
        ),
        entry(
            "Driver",
            "Screen control runs through a separate signed helper the user installs.",
            driver_control(modal, theme, density, typography, cx),
        ),
    ]
}

/// The driver row's right-hand control: a status line, plus a re-check button
/// so installing the helper does not need a restart to be noticed.
fn driver_control(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let colour = match modal.driver_status {
        DriverStatus::Ready { .. } => theme.status_ok,
        DriverStatus::Unknown => theme.fg_muted,
        DriverStatus::NotInstalled | DriverStatus::Problem { .. } => theme.status_warn,
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                // Bounded so a long verification failure cannot push the button
                // off the pane — settings prose does not wrap here.
                .max_w(px(240.0))
                .text_size(px(typography.t_body_sm))
                .text_color(colour)
                .child(modal.driver_status.summary()),
        )
        .child(value_chip(
            "screen-driver-recheck",
            "Check again",
            theme,
            density,
            typography,
            |this, _w, cx| {
                this.driver_status = DriverStatus::resolve();
                cx.notify();
            },
            cx,
        ))
        .into_any_element()
}

/// One row per opted-in project, each with a way out.
///
/// There is deliberately no "add project" affordance here: a project is opted
/// in from its own window, where the user can see which project they are
/// enabling. Picking a path out of a list in a global settings pane is how the
/// wrong repository gets enabled.
fn project_rows(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    modal
        .computer_use
        .projects
        .iter()
        .enumerate()
        .map(|(idx, project)| {
            let path = project.clone();
            entry(
                project
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(unnamed)")
                    .to_string(),
                project.display().to_string(),
                value_chip(
                    ("screen-project-remove", idx),
                    "Remove",
                    theme,
                    density,
                    typography,
                    move |this, _w, cx| {
                        this.computer_use.disable_project(&path);
                        this.persist_computer_use(cx);
                    },
                    cx,
                ),
            )
        })
        .collect()
}

/// One row per pre-approved app. Empty renders a single explanatory row rather
/// than nothing, so the section never looks broken.
fn app_rows(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    if modal.computer_use.allowed_apps.is_empty() {
        return vec![entry(
            "No approved apps",
            "Every app will raise a consent card the first time an agent tries to drive it.",
            div(),
        )];
    }
    modal
        .computer_use
        .allowed_apps
        .iter()
        .enumerate()
        .map(|(idx, grant)| {
            let bundle_id = grant.bundle_id.clone();
            let label = if grant.name.is_empty() {
                grant.bundle_id.clone()
            } else {
                grant.name.clone()
            };
            entry(
                label,
                grant.bundle_id.clone(),
                value_chip(
                    ("screen-app-revoke", idx),
                    "Remove",
                    theme,
                    density,
                    typography,
                    move |this, _w, cx| {
                        this.computer_use.revoke(&bundle_id);
                        this.persist_computer_use(cx);
                    },
                    cx,
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::ComputerUseSettings;

    /// The pane must keep saying that the permission is not project-scoped.
    ///
    /// Pinned because this is exactly the kind of copy a later tidy-up removes
    /// for being wordy — and because the sentence it replaced ("each project
    /// opts in separately") read as a guarantee the OS does not make. The claim
    /// was measured during the coverage spike: an agent's shell inherits
    /// OxiMux's Accessibility grant regardless of which projects are listed.
    #[test]
    fn the_pane_does_not_promise_the_permission_is_per_project() {
        let shown = FOOTNOTES.join("\n");
        for claim in ["Accessibility", "shell inherits", "not only those listed"] {
            assert!(shown.contains(claim), "the pane must still say: {claim}");
        }
        // And each line has to fit: settings prose does not wrap here, so a
        // long one clips at the pane edge and the warning is simply not read.
        for note in FOOTNOTES {
            assert!(note.len() <= 90, "{note:?} will clip ({} chars)", note.len());
        }
    }

    #[test]
    fn a_verified_driver_reads_as_ready_with_its_version() {
        let status = DriverStatus::Ready {
            version: "0.12.6".into(),
        };
        assert!(status.summary().contains("0.12.6"));
        assert!(status.summary().contains("verified"));
    }

    #[test]
    fn a_verification_failure_shows_the_specific_reason() {
        // "Unavailable" would be useless here: a wrong signing team and an
        // out-of-date driver need opposite responses from the user.
        let status = DriverStatus::Problem {
            detail: "driver is signed by team `WRONG`, expected `YCK386LBJ7`".into(),
        };
        assert!(status.summary().contains("WRONG"), "{}", status.summary());
    }

    #[test]
    fn not_installed_is_its_own_state() {
        assert_eq!(DriverStatus::NotInstalled.summary(), "Not installed");
    }

    #[test]
    fn an_unopened_pane_does_not_claim_the_driver_is_missing() {
        // The default must not read as a verdict — it has not looked yet.
        assert_eq!(DriverStatus::Unknown.summary(), "Checking…");
    }

    #[test]
    fn resolving_never_panics_whatever_is_installed() {
        // Runs on every CI machine, none of which have the driver; the point is
        // that the settings pane cannot be crashed by its absence.
        let _ = DriverStatus::resolve();
    }

    #[test]
    fn the_allowlist_is_keyed_on_bundle_id_not_display_name() {
        // Two apps can share a display name; only the bundle id decides.
        let mut settings = ComputerUseSettings::default();
        settings.allow("com.apple.Safari", "Safari");
        settings.allow("com.example.Safari", "Safari");
        assert_eq!(settings.allowed_apps.len(), 2);
        assert!(settings.revoke("com.example.Safari"));
        assert!(settings.is_allowed("com.apple.Safari"));
    }
}
