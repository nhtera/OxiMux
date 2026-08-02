//! Screen Control pane — the master switch, the driver's health, the projects
//! opted in, and the apps that no longer raise a card.
//!
//! Nothing here is on by default and nothing here turns itself on. The pane's
//! job is to make the state legible: whether the driver is installed and
//! trusted, which projects can use it, and exactly which apps the user has
//! already said yes to — because a grant given once in a card, months ago, is
//! only revocable if it can be found.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry, section_title};
#[cfg(target_os = "macos")]
use crate::shell::driver_install::{self, DriverInstallUi};

/// What `prepare()` reported about the installed driver, resolved when the
/// modal opens rather than per repaint — it spawns `codesign`.
///
/// macOS only. Windows has no publisher to report and no in-app installer to
/// offer, so its driver row is `pane_driver_trust`'s instead — same position in
/// the pane, an entirely different question.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverStatus {
    /// Not looked for yet (the modal has never been opened).
    Unknown,
    Ready { version: String },
    /// Named as its own state rather than folded into `Problem`: it is the one
    /// the user can fix by installing something, and it is by far the most
    /// common.
    NotInstalled,
    /// Installed and validly signed, but below the integration's floor. Its
    /// own variant (not a `Problem` string) because the fix is the same
    /// install pipeline wearing an "Update" label — string-matching the error
    /// prose to decide that would break on any rewording.
    Outdated { found: String, minimum: String },
    Problem { detail: String },
}

#[cfg(target_os = "macos")]
impl DriverStatus {
    /// Look for the driver and put it through every gate.
    pub(crate) fn resolve() -> Self {
        match oximux_computer_use::prepare() {
            Ok(driver) => DriverStatus::Ready {
                version: driver.version.to_string(),
            },
            Err(oximux_computer_use::Error::NotFound { .. }) => DriverStatus::NotInstalled,
            Err(oximux_computer_use::Error::DriverTooOld { found, minimum }) => {
                DriverStatus::Outdated {
                    found: found.to_string(),
                    minimum: minimum.to_string(),
                }
            }
            Err(err) => DriverStatus::Problem {
                detail: err.to_string(),
            },
        }
    }

    /// The install affordance this status calls for, if any.
    pub(crate) fn install_label(&self) -> Option<&'static str> {
        match self {
            DriverStatus::NotInstalled => Some("Install driver…"),
            DriverStatus::Outdated { .. } => Some("Update driver…"),
            // A `Problem` (bad signature, wrong team) is not fixed by
            // reinstalling the same artifact; it gets the manual link only.
            _ => None,
        }
    }

    pub(crate) fn summary(&self) -> SharedString {
        match self {
            DriverStatus::Unknown => "Checking…".into(),
            DriverStatus::Ready { version } => format!("Installed, verified ({version})").into(),
            DriverStatus::NotInstalled => "Not installed".into(),
            DriverStatus::Outdated { found, minimum } => {
                format!("Driver {found} is older than the required {minimum}").into()
            }
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

    body = body
        .child(section_title(
            "Projects",
            // Deliberately "get the tools" rather than "may drive apps": the
            // list decides who is handed the computer-use tools, not who can
            // reach the screen. See `footnotes`.
            //
            // The section renders even when empty. Hidden, it left a user who
            // had just turned the master switch on with nothing on screen
            // saying where the per-project half lives — and that half is not
            // optional, so the feature looked broken rather than unfinished.
            PROJECTS_BLURB,
            theme,
            typography,
        ))
        .child(entries_card(
            theme,
            density,
            typography,
            project_rows(modal, theme, density, typography, cx),
        ));

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

    body
        .children(driver_detail(modal, theme, density, typography))
        .child(footnotes(theme, typography))
        .into_any_element()
}

/// The evidence card under the Driver row: path, size, digest.
///
/// Windows only, because it is the disclosure behind an approval decision macOS
/// never has to make — there the publisher answers it and there is nothing for
/// the user to compare.
#[cfg(windows)]
fn driver_detail(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Option<AnyElement> {
    super::pane_driver_trust::detail_card(modal, theme, density, typography)
}

#[cfg(target_os = "macos")]
fn driver_detail(
    _modal: &SettingsModal,
    _theme: Theme,
    _density: Density,
    _typography: &Typography,
) -> Option<AnyElement> {
    None
}

/// The two things a user reading this pane could otherwise get wrong.
///
/// The first is the older one: an agent with a shell has routes to the same
/// driver that no list here can close, so this is a guard against mistakes.
///
/// The second was measured during the coverage spike, and it is the reason the
/// per-project wording elsewhere in this pane had to change. Turning screen
/// control on requires OxiMux to hold macOS Accessibility — it is what lets an
/// agent act on another app's UI at all. macOS attributes
/// that grant to OxiMux as the responsible process and **every descendant
/// inherits it**, which includes each agent's shell tool, in every project,
/// whether or not that project appears above. The list below controls which
/// projects are handed the screen-control *tools*. It does not, and cannot,
/// fence off the permission.
///
/// One line per sentence: prose in a settings pane does not wrap in this app, so
/// a paragraph would clip at the pane edge rather than reflow.
#[cfg(target_os = "macos")]
const FOOTNOTES: &[&str] = &[
    "Guards against an agent's mistakes, not an agent trying to get around them.",
    "Enabling this grants OxiMux macOS Accessibility, which is how agents drive apps.",
    "Every agent's shell inherits that grant — in all projects, not only those listed.",
    "Esc needs Input Monitoring as well — a separate switch, granted separately.",
];

/// The Windows set. Deliberately not a translation of the list above: none of
/// those permissions exist here, and the two things a Windows user has to know
/// instead are that the driver's publisher was never checked and that the kill
/// switch cannot be confirmed live.
#[cfg(windows)]
const FOOTNOTES: &[&str] = &[
    "Guards against an agent's mistakes, not an agent trying to get around them.",
    "The driver is unsigned — you approved the exact file, not its publisher.",
    "Every agent's shell can reach the screen, in all projects, not only those listed.",
    "Esc stops an agent, but Windows cannot confirm the key is being seen.",
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
            "Enable computer use",
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
        driver_entry(modal, theme, density, typography, cx),
    ]
    .into_iter()
    .chain(upgrade_note(modal))
    .collect()
}

/// The Driver row, which asks a different question on each platform: macOS asks
/// whether the publisher checks out, Windows asks whether *you* approved these
/// bytes. Same slot, so the pane around it does not have to know which.
#[cfg(target_os = "macos")]
fn driver_entry(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> SettingEntry {
    entry(
        "Driver",
        "Computer use runs through a separate signed helper.",
        driver_control(modal, theme, density, typography, cx),
    )
}

#[cfg(windows)]
fn driver_entry(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> SettingEntry {
    super::pane_driver_trust::driver_entry(modal, theme, density, typography, cx)
}

/// Post-upgrade only: the shared daemon is deliberately left running, so the
/// user should know old behavior may persist until it respawns. Tied to the
/// in-app installer, which is macOS-only.
#[cfg(target_os = "macos")]
fn upgrade_note(modal: &SettingsModal) -> Option<SettingEntry> {
    (modal.driver_upgraded && matches!(modal.driver_status, DriverStatus::Ready { .. }))
        .then(|| entry("Driver updated", driver_install::UPGRADE_NOTE, div()))
}

#[cfg(windows)]
fn upgrade_note(_modal: &SettingsModal) -> Option<SettingEntry> {
    None
}

/// The driver row's right-hand control.
///
/// While an install runs it shows live progress + Cancel; otherwise a status
/// line, an Install/Update button when the status calls for one, a manual
/// download link after a failure, and a re-check button so installing the
/// helper by hand does not need a restart to be noticed.
#[cfg(target_os = "macos")]
fn driver_control(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    if let DriverInstallUi::Running { stage } = &modal.driver_install_ui {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(driver_install::stage_label(stage)),
            );
        // Cancel only when this modal started the install — an observed
        // install (another window's) gets progress but no inert button.
        if modal.driver_install.is_some() {
            row = row.child(value_chip(
                "screen-driver-cancel",
                "Cancel",
                theme,
                density,
                typography,
                |this, _w, cx| {
                    if let Some(handle) = &this.driver_install {
                        handle.cancel();
                    }
                    cx.notify();
                },
                cx,
            ));
        }
        return row.into_any_element();
    }

    let colour = match modal.driver_status {
        DriverStatus::Ready { .. } => theme.status_ok,
        DriverStatus::Unknown => theme.fg_muted,
        DriverStatus::NotInstalled
        | DriverStatus::Outdated { .. }
        | DriverStatus::Problem { .. } => theme.status_warn,
    };
    let failure = match &modal.driver_install_ui {
        DriverInstallUi::Failed { message } => Some(message.clone()),
        _ => None,
    };
    let status_line = failure
        .clone()
        .map(SharedString::from)
        .unwrap_or_else(|| modal.driver_status.summary());

    let mut row = div()
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
                .text_color(if failure.is_some() {
                    theme.status_warn
                } else {
                    colour
                })
                .child(status_line),
        );
    if let Some(label) = modal.driver_status.install_label() {
        row = row.child(value_chip(
            "screen-driver-install",
            label,
            theme,
            density,
            typography,
            |this, _w, cx| this.start_driver_install(cx),
            cx,
        ));
    }
    // The manual escape hatch: after an install failure, and also for an
    // on-disk `Problem` — the state the install button is deliberately
    // withheld for, which would otherwise leave the user with no way forward.
    if failure.is_some() || matches!(modal.driver_status, DriverStatus::Problem { .. }) {
        row = row.child(value_chip(
            "screen-driver-manual",
            "Download manually",
            theme,
            density,
            typography,
            |_this, _w, cx| {
                crate::shell::open_url::open_url(driver_install::MANUAL_DOWNLOAD_URL, cx);
            },
            cx,
        ));
    }
    row.child(value_chip(
        "screen-driver-recheck",
        "Check again",
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.driver_status = DriverStatus::resolve();
            this.driver_install_ui = DriverInstallUi::Idle;
            cx.notify();
        },
        cx,
    ))
    .into_any_element()
}

#[cfg(target_os = "macos")]
impl SettingsModal {
    /// Kick off (or attach to) the driver install and start polling it.
    pub(super) fn start_driver_install(&mut self, cx: &mut gpui::Context<Self>) {
        if self.driver_install_ui.is_running() {
            return;
        }
        // Remember whether this is an upgrade before the status flips, so the
        // stale-daemon note can be shown exactly when it applies.
        self.driver_upgraded = matches!(
            self.driver_status,
            DriverStatus::Ready { .. } | DriverStatus::Outdated { .. }
        );
        let (handle, ui) = driver_install::begin();
        self.driver_install = handle;
        self.driver_install_ui = ui;
        self.spawn_driver_install_poll(cx);
        cx.notify();
    }

    /// Poll loop: pump backend events into UI state every tick until the
    /// install reaches a terminal state, then re-resolve the driver status
    /// (the pipeline's word is never trusted over a fresh `codesign` check).
    pub(super) fn spawn_driver_install_poll(&mut self, cx: &mut gpui::Context<Self>) {
        if self.driver_poll_running || !self.driver_install_ui.is_running() {
            return;
        }
        self.driver_poll_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let done = this.update(cx, |this, cx| {
                    let done =
                        driver_install::pump(&mut this.driver_install, &mut this.driver_install_ui);
                    if done {
                        this.driver_poll_running = false;
                        this.driver_status = DriverStatus::resolve();
                        // The note only matters when the upgrade actually
                        // landed — a failure means the old driver still runs
                        // and still matches what is on disk.
                        if !matches!(this.driver_status, DriverStatus::Ready { .. }) {
                            this.driver_upgraded = false;
                        }
                    }
                    cx.notify();
                    done
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
    }
}

/// Where the per-project opt-in actually is.
///
/// Names the control rather than gesturing at it ("from its own window" sent
/// the reader looking for something that did not exist). One sentence, because
/// settings prose does not wrap here.
const PROJECTS_BLURB: &str =
    "These projects get the computer-use tools. Add one from its ••• menu in the sidebar.";

/// One row per opted-in project, each with a way out. Empty renders a single
/// explanatory row rather than nothing, so the section never looks broken.
///
/// There is deliberately no "add project" affordance here: a project is opted
/// in from its own menu in the sidebar, where the user can see which project
/// they are enabling. Picking a path out of a list in a global settings pane is
/// how the wrong repository gets enabled.
fn project_rows(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    if modal.computer_use.projects.is_empty() {
        return vec![entry(
            "No projects yet",
            if modal.computer_use.enabled {
                "Right-click a project in the sidebar, or use its ••• menu, to turn it on."
            } else {
                // The per-project control is not offered while the master
                // switch is off, so pointing at it here would be a dead end.
                "Turn computer use on above first, then enable it per project."
            },
            div(),
        )];
    }
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
    /// The Windows footnotes carry the one disclosure the macOS pane does not
    /// need: the driver's publisher was never checked. Asserted here rather
    /// than trusted to review, because it is the sentence that keeps the
    /// approval honest and it is easy to lose in a reword.
    #[cfg(windows)]
    #[test]
    fn the_windows_footnotes_disclose_the_unsigned_driver() {
        assert!(
            FOOTNOTES.iter().any(|n| n.contains("unsigned")),
            "the Windows pane must say the driver is unsigned"
        );
        assert!(
            !FOOTNOTES.iter().any(|n| n.contains("Accessibility")),
            "macOS permission copy must not leak into the Windows pane"
        );
    }

    use super::*;
    use oximux_settings::ComputerUseSettings;

    /// The pane must keep saying that the permission is not project-scoped.
    ///
    /// Pinned because this is exactly the kind of copy a later tidy-up removes
    /// for being wordy — and because the sentence it replaced ("each project
    /// opts in separately") read as a guarantee the OS does not make. The claim
    /// was measured during the coverage spike: an agent's shell inherits
    /// OxiMux's Accessibility grant regardless of which projects are listed.
    /// macOS only: every claim below names a macOS permission. The Windows list
    /// makes different promises and is pinned by
    /// `the_windows_footnotes_disclose_the_unsigned_driver`.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_pane_does_not_promise_the_permission_is_per_project() {
        let shown = FOOTNOTES.join("\n");
        // "Input Monitoring" is pinned because the pane once said Accessibility
        // was what Escape needed. It is not — a keyboard tap is gated on Input
        // Monitoring — so anyone who granted the one they were told to got a
        // kill switch that silently did nothing.
        for claim in [
            "Accessibility",
            "shell inherits",
            "not only those listed",
            "Input Monitoring",
        ] {
            assert!(shown.contains(claim), "the pane must still say: {claim}");
        }
    }

    /// Split from the macOS claim list above because it is the one thing about
    /// these strings that is not platform-specific: settings prose does not wrap
    /// here, so a long line clips at the pane edge and the warning is simply not
    /// read. The Windows list has to obey it too.
    #[test]
    fn every_footnote_fits_the_pane() {
        for note in FOOTNOTES {
            assert!(note.len() <= 90, "{note:?} will clip ({} chars)", note.len());
        }
    }

    /// The blurb has to name a control that exists.
    ///
    /// It once read "Turn one on from its own window", which described nothing:
    /// there was no per-project affordance anywhere, `enable_project` had no
    /// caller, and the only way into this list was hand-editing the TOML. The
    /// sentence is the contract with that menu row — if the row moves, this
    /// fails rather than quietly pointing at nothing again.
    #[test]
    fn the_projects_blurb_names_where_the_opt_in_lives() {
        assert!(PROJECTS_BLURB.contains("menu"), "{PROJECTS_BLURB:?}");
        assert!(PROJECTS_BLURB.contains("sidebar"), "{PROJECTS_BLURB:?}");
        // Same clipping limit as the footnotes: prose does not wrap in this
        // pane, so a long line is simply not read.
        assert!(
            PROJECTS_BLURB.chars().count() <= 90,
            "will clip ({} chars)",
            PROJECTS_BLURB.chars().count()
        );
    }

    /// `DriverStatus` is the macOS driver row; Windows answers a different
    /// question in `pane_driver_trust`, which carries its own tests.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_verified_driver_reads_as_ready_with_its_version() {
        let status = DriverStatus::Ready {
            version: "0.12.6".into(),
        };
        assert!(status.summary().contains("0.12.6"));
        assert!(status.summary().contains("verified"));
    }

    /// `DriverStatus` is the macOS driver row; Windows answers a different
    /// question in `pane_driver_trust`, which carries its own tests.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_verification_failure_shows_the_specific_reason() {
        // "Unavailable" would be useless here: a wrong signing team and an
        // out-of-date driver need opposite responses from the user.
        let status = DriverStatus::Problem {
            detail: "driver is signed by team `WRONG`, expected `YCK386LBJ7`".into(),
        };
        assert!(status.summary().contains("WRONG"), "{}", status.summary());
    }

    /// `DriverStatus` is the macOS driver row; Windows answers a different
    /// question in `pane_driver_trust`, which carries its own tests.
    #[cfg(target_os = "macos")]
    #[test]
    fn not_installed_is_its_own_state() {
        assert_eq!(DriverStatus::NotInstalled.summary(), "Not installed");
    }

    /// `DriverStatus` is the macOS driver row; Windows answers a different
    /// question in `pane_driver_trust`, which carries its own tests.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unopened_pane_does_not_claim_the_driver_is_missing() {
        // The default must not read as a verdict — it has not looked yet.
        assert_eq!(DriverStatus::Unknown.summary(), "Checking…");
    }

    /// `DriverStatus` is the macOS driver row; Windows answers a different
    /// question in `pane_driver_trust`, which carries its own tests.
    #[cfg(target_os = "macos")]
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
