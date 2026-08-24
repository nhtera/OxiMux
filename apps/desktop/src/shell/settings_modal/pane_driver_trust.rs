//! Computer use on Windows: approving a driver that nobody signed.
//!
//! The macOS pane next door asks "is the driver installed and does it pass the
//! signature gate?", and the answer arrives from Apple. Windows has no such
//! answer to fetch — every published `cua-driver.exe` is `NotSigned` — so
//! `oximux_computer_use::trust` moves the decision to the person who can
//! actually make it, and this pane is where they make it.
//!
//! # What the screen has to show, and why each piece is load-bearing
//!
//! There is no separate confirmation dialog. Everything the decision rests on
//! is on the pane *before* the button is clicked, because a modal that appears
//! after the click would be the second thing the user reads rather than the
//! first, and this is the one approval in the product with nothing behind it
//! but the user's own care:
//!
//! - **The path**, because approving the wrong file is the failure mode this
//!   cannot otherwise detect.
//! - **The size and the full 64-character digest**, ungrouped and untruncated,
//!   so it can be compared against upstream's published `checksums.txt`. An
//!   abbreviated hash is decoration — it cannot be checked against anything.
//! - **The words "unverified publisher"**, prominently, because every other
//!   trust surface in this app means "someone vouched for this" and this one
//!   does not.
//!
//! # What approval does and does not mean
//!
//! It pins the bytes. It does not establish identity: nothing here can tell an
//! authentic driver from a hostile one the user was talked into installing.
//! What it buys is continuity — those bytes cannot change again without being
//! asked — which is the realistic threat against a long-lived install of an
//! unsigned, self-updating tool.
//!
//! Re-approval is the routine path rather than an edge case: upstream ships
//! roughly six releases a week and the driver rewrites itself in place. The
//! pane makes that one click. It deliberately does not make it zero.
//!
//! # Installing from here
//!
//! OxiMux can also fetch the driver itself (`oximux_computer_use::install`),
//! and that changes nothing above. The download is checked against the
//! checksum upstream publishes beside it, which is corruption cover and not a
//! signature — the file still arrives unsigned, and the approval it asks for
//! is the same decision, made about bytes that have not been placed yet.

use std::path::PathBuf;

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{info_row, value_chip};
use super::layout::{SettingEntry, entry, section_title};

/// Where the pin lives.
///
/// Deliberately *not* defined here. The turn glue reads this store at spawn and
/// this pane writes it; two definitions that drifted would show a driver as
/// approved in settings while every chat kept refusing it, and nothing in either
/// place would look wrong.
use crate::shell::agent_chat::computer_use::trust_store as store;

use crate::shell::driver_install::{self, DriverInstallUi};

/// What the trust gate reports about the installed driver.
///
/// Resolved when the modal opens rather than per repaint: it hashes the binary,
/// and once approved it also runs `--version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustState {
    /// The modal has not been opened yet.
    Unknown,
    /// Nothing to approve. Its own state because the fix is an install, not a
    /// decision.
    NotInstalled,
    /// Found, never approved. The ordinary first-run state.
    Unapproved {
        path: PathBuf,
        sha256: String,
        bytes: u64,
    },
    /// Approved once, and the bytes changed. The case the whole anchor exists
    /// for, and kept apart from [`TrustState::Unapproved`] because "you have
    /// not set this up" and "something replaced what you approved" are opposite
    /// messages — only one of them should alarm anyone.
    Superseded {
        path: PathBuf,
        approved: String,
        found: String,
    },
    Approved {
        path: PathBuf,
        sha256: String,
        version: String,
    },
    Problem { detail: String },
}

impl TrustState {
    pub(crate) fn resolve() -> Self {
        Self::from_prepare(oximux_computer_use::prepare(&store()))
    }

    /// The mapping, split from [`TrustState::resolve`] so it can be tested
    /// without a trust store, a `PATH`, or a driver.
    ///
    /// Worth testing on its own: every arm here is a different sentence shown
    /// to the user, and a mis-mapped one is invisible in review — the pane still
    /// renders, just with the wrong story.
    fn from_prepare(
        outcome: Result<oximux_computer_use::VerifiedDriver, oximux_computer_use::Error>,
    ) -> Self {
        use oximux_computer_use::Error;
        match outcome {
            Ok(driver) => TrustState::Approved {
                sha256: driver.sha256,
                version: driver.version.to_string(),
                path: driver.path,
            },
            Err(Error::NotFound { .. }) => TrustState::NotInstalled,
            Err(Error::NotApproved { path, sha256 }) => TrustState::Unapproved {
                bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or_default(),
                path,
                sha256,
            },
            Err(Error::TrustSuperseded {
                path,
                approved,
                found,
            }) => TrustState::Superseded {
                path,
                approved,
                found,
            },
            Err(err) => TrustState::Problem {
                detail: err.to_string(),
            },
        }
    }

    /// The one-line verdict beside the Driver row.
    pub(crate) fn summary(&self) -> SharedString {
        match self {
            TrustState::Unknown => "Checking…".into(),
            TrustState::NotInstalled => "Not installed".into(),
            TrustState::Unapproved { .. } => "Found, not yet approved".into(),
            // Names what happened rather than the state it left behind: the
            // user needs to know a binary they trusted was rewritten.
            TrustState::Superseded { .. } => "Changed since you approved it".into(),
            TrustState::Approved { version, .. } => {
                format!("Approved ({version}) — publisher unverified").into()
            }
            TrustState::Problem { detail } => detail.clone().into(),
        }
    }

    /// Status colour. Approved is deliberately *not* the success colour: green
    /// would say "verified", and nothing here verified a publisher.
    fn colour(&self, theme: Theme) -> gpui::Hsla {
        match self {
            TrustState::Approved { .. } | TrustState::Unknown => theme.fg_muted,
            TrustState::NotInstalled | TrustState::Unapproved { .. } => theme.fg_base,
            TrustState::Superseded { .. } | TrustState::Problem { .. } => theme.status_warn,
        }
    }

    /// The binary this state is about, when there is one.
    fn path(&self) -> Option<&PathBuf> {
        match self {
            TrustState::Unapproved { path, .. }
            | TrustState::Superseded { path, .. }
            | TrustState::Approved { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// The Driver row for the Windows pane.
///
/// Called from `pane_computer_use`, which owns the page — the master switch, the
/// per-project list and the approved-apps list are the same product on both
/// platforms, and only this row differs. Duplicating the page to change one row
/// would have meant two copies of the opt-in UI that decides whether agents can
/// drive anything at all.
pub(super) fn driver_entry(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> SettingEntry {
    entry(
        "Driver",
        // No longer "a helper you install yourself" — OxiMux can fetch it now,
        // and the Install button beside this says so. What the sentence keeps is
        // the part no button conveys: you approve it.
        //
        // Kept SHORTER than the string it replaced, deliberately. `entries_card`
        // sizes one label column across every row, so a long description here
        // squeezes the controls off the right edge of *other* rows — a two-clause
        // version of this sentence made the master toggle disappear entirely.
        // Found by looking at the running pane, not by reading the diff.
        "Computer use runs through a separate helper you approve.",
        driver_control(modal, theme, density, typography, cx),
    )
}

fn driver_control(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    // An install in flight owns this row. The trust verdict beside it would be
    // about the file being replaced, which is the one thing nobody is deciding
    // right now.
    match &modal.driver_install_ui {
        DriverInstallUi::Running { stage } => {
            return install_progress(modal, stage, theme, density, typography, cx);
        }
        DriverInstallUi::AwaitingApproval { .. } => {
            return staged_decision(theme, density, typography, cx);
        }
        DriverInstallUi::Failed { message } => {
            return install_failure(message, theme, density, typography, cx);
        }
        DriverInstallUi::Idle => {}
    }

    let state = &modal.driver_trust;
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                // Bounded: settings prose does not wrap here, so an unbounded
                // failure string would push the buttons off the pane.
                .max_w(px(240.0))
                .text_size(px(typography.t_body_sm))
                .text_color(state.colour(theme))
                .child(state.summary()),
        );

    // The approve affordance, worded for what it actually does in each state.
    if let Some(path) = state.path() {
        let label = match state {
            TrustState::Unapproved { .. } => Some("Approve"),
            TrustState::Superseded { .. } => Some("Approve the new file"),
            _ => None,
        };
        if let Some(label) = label {
            let path = path.clone();
            row = row.child(value_chip(
                "driver-trust-approve",
                label,
                theme,
                density,
                typography,
                move |this, _w, cx| {
                    approve(this, &path);
                    cx.notify();
                },
                cx,
            ));
        }
        if matches!(state, TrustState::Approved { .. }) {
            row = row.child(value_chip(
                "driver-trust-revoke",
                "Revoke",
                theme,
                density,
                typography,
                |this, _w, cx| {
                    revoke(this);
                    cx.notify();
                },
                cx,
            ));
        }
    }

    // Install is offered where it is the actual fix — nothing on disk — and as
    // an update where something already is. Never beside `Unapproved` or
    // `Superseded`: those are decisions about a file that is already here, and
    // burying them under a download button would answer a question the user
    // did not ask.
    if let Some(label) = install_label(state) {
        row = row.child(value_chip(
            "driver-trust-install",
            label,
            theme,
            density,
            typography,
            |this, _w, cx| this.start_driver_install(cx),
            cx,
        ));
    }

    // Re-check exists because the driver may also be installed outside OxiMux:
    // a user who installs it while this pane is open would otherwise have to
    // reopen the modal to be noticed.
    row.child(value_chip(
        "driver-trust-recheck",
        "Re-check",
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.driver_trust = TrustState::resolve();
            cx.notify();
        },
        cx,
    ))
    .into_any_element()
}

/// What the install button should say, if it should appear at all.
fn install_label(state: &TrustState) -> Option<&'static str> {
    match state {
        TrustState::NotInstalled => Some("Install"),
        TrustState::Approved { .. } => Some("Update"),
        _ => None,
    }
}

/// Live progress, plus Cancel when this modal is the surface that started it —
/// an install observed from another window gets progress but no inert button.
fn install_progress(
    modal: &SettingsModal,
    stage: &oximux_computer_use::install::InstallStage,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
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
    if modal.driver_install.is_some() {
        row = row.child(value_chip(
            "driver-install-cancel",
            "Cancel",
            theme,
            density,
            typography,
            |this, _w, _cx| {
                if let Some(handle) = &this.driver_install {
                    handle.cancel();
                }
            },
            cx,
        ));
    }
    row.into_any_element()
}

/// The decision the parked install is waiting on. The evidence for it is on the
/// card below, not behind this button — see the module doc.
fn staged_decision(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child("Downloaded — publisher unverified"),
        )
        .child(value_chip(
            "driver-install-approve",
            "Approve and install",
            theme,
            density,
            typography,
            |_this, _w, _cx| driver_install::approve(),
            cx,
        ))
        .child(value_chip(
            "driver-install-decline",
            "Don't install",
            theme,
            density,
            typography,
            |_this, _w, _cx| driver_install::decline(),
            cx,
        ))
        .into_any_element()
}

/// A failed install, with the manual route out. Nothing was placed.
fn install_failure(
    message: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                .max_w(px(240.0))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.status_warn)
                .child(SharedString::from(message.to_string())),
        )
        .child(value_chip(
            "driver-install-retry",
            "Try again",
            theme,
            density,
            typography,
            |this, _w, cx| this.start_driver_install(cx),
            cx,
        ))
        .child(value_chip(
            "driver-install-manual",
            "Download manually",
            theme,
            density,
            typography,
            |_this, _w, cx| {
                crate::shell::open_url::open_url(driver_install::MANUAL_DOWNLOAD_URL, cx);
            },
            cx,
        ))
        .into_any_element()
}

/// Record the user's approval, then re-resolve so the pane shows the result of
/// the gate rather than an assumption about it.
fn approve(modal: &mut SettingsModal, path: &std::path::Path) {
    if let Err(err) = store().approve(path) {
        modal.driver_trust = TrustState::Problem {
            detail: err.to_string(),
        };
        tracing::warn!(%err, "settings: could not record driver approval");
        return;
    }
    modal.driver_trust = TrustState::resolve();
}

fn revoke(modal: &mut SettingsModal) {
    if let Err(err) = store().revoke() {
        tracing::warn!(%err, "settings: could not revoke driver approval");
    }
    modal.driver_trust = TrustState::resolve();
}

/// The evidence the decision rests on — shown for every state that names a
/// file, including an already-approved one, so "what did I approve?" stays
/// answerable after the fact.
pub(super) fn detail_card(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Option<AnyElement> {
    // A parked install is asking about bytes that are not on disk yet, so the
    // evidence is different: there is no path to name, and what stands in for
    // it is which release these bytes came from.
    if let DriverInstallUi::AwaitingApproval {
        version,
        sha256,
        bytes,
    } = &modal.driver_install_ui
    {
        return Some(staged_evidence(
            version, sha256, *bytes, theme, density, typography,
        ));
    }

    let (path, digest, previous) = match &modal.driver_trust {
        TrustState::Unapproved { path, sha256, .. } => (path, sha256, None),
        TrustState::Approved { path, sha256, .. } => (path, sha256, None),
        TrustState::Superseded {
            path,
            approved,
            found,
        } => (path, found, Some(approved)),
        _ => return None,
    };

    let mut rows = div()
        .flex()
        .flex_col()
        .child(info_row("File", path.display().to_string(), theme, typography));

    if let TrustState::Unapproved { bytes, .. } = &modal.driver_trust {
        rows = rows.child(info_row(
            "Size",
            format!("{bytes} bytes"),
            theme,
            typography,
        ));
    }

    rows = rows.child(info_row(
        "SHA-256",
        // Full and unbroken: this is meant to be compared against upstream's
        // published checksums, and a shortened digest cannot be.
        digest.clone(),
        theme,
        typography,
    ));

    if let Some(previous) = previous {
        rows = rows.child(info_row(
            "You approved",
            previous.clone(),
            theme,
            typography,
        ));
    }

    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline))
            .child(section_title(
                "What you are approving",
                detail_blurb(&modal.driver_trust),
                theme,
                typography,
            ))
            .child(
                div()
                    .p(px(12.0))
                    .rounded(px(density.r_chip))
                    .bg(theme.bg_panel_alt)
                    .border_1()
                    .border_color(theme.border_inactive)
                    .child(rows),
            )
            .into_any_element(),
    )
}

/// What a staged, not-yet-placed download is asking the user to accept.
///
/// Says "unsigned" rather than letting the checksum imply otherwise: OxiMux did
/// check the download against upstream's published hash, but that file ships
/// from the same place as the binary, so it rules out corruption and not
/// substitution. Presenting it as verification would be the one lie this pane
/// exists to avoid.
fn staged_evidence(
    version: &oximux_computer_use::Version,
    sha256: &str,
    bytes: u64,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let rows = div()
        .flex()
        .flex_col()
        .child(info_row(
            "Release",
            format!("cua-driver {version}"),
            theme,
            typography,
        ))
        .child(info_row(
            "From",
            "github.com/trycua/cua releases".to_string(),
            theme,
            typography,
        ))
        .child(info_row(
            "Size",
            format!("{bytes} bytes"),
            theme,
            typography,
        ))
        .child(info_row("SHA-256", sha256.to_string(), theme, typography));

    div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline))
        .child(section_title(
            "What you are approving",
            "These binaries are unsigned, so nobody vouched for them. Approving pins these \
             exact bytes — if they ever change, OxiMux asks again.",
            theme,
            typography,
        ))
        .child(
            div()
                .p(px(12.0))
                .rounded(px(density.r_chip))
                .bg(theme.bg_panel_alt)
                .border_1()
                .border_color(theme.border_inactive)
                .child(rows),
        )
        .into_any_element()
}

fn detail_blurb(state: &TrustState) -> &'static str {
    match state {
        TrustState::Superseded { .. } => {
            "This file is not the one you approved. Approve it only if you updated the driver \
             yourself."
        }
        TrustState::Approved { .. } => "OxiMux will refuse this driver if these bytes change.",
        _ => "Compare this against the checksum published with the release before approving.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unapproved() -> TrustState {
        TrustState::Unapproved {
            path: PathBuf::from(r"C:\tools\cua-driver.exe"),
            sha256: "a".repeat(64),
            bytes: 1234,
        }
    }

    fn superseded() -> TrustState {
        TrustState::Superseded {
            path: PathBuf::from(r"C:\tools\cua-driver.exe"),
            approved: "a".repeat(64),
            found: "b".repeat(64),
        }
    }

    fn approved() -> TrustState {
        TrustState::Approved {
            path: PathBuf::from(r"C:\tools\cua-driver.exe"),
            sha256: "a".repeat(64),
            version: "0.14.2".into(),
        }
    }

    #[test]
    fn an_approved_driver_never_claims_to_be_verified() {
        // The single most important string in this pane. Every other trust
        // surface in the app means "someone vouched for this"; if this one
        // reads the same way, the whole anchor is misrepresented.
        let summary = approved().summary().to_string();
        assert!(
            summary.contains("unverified"),
            "approved summary must disclaim the publisher, got {summary:?}"
        );
        assert!(
            !summary.to_lowercase().contains("verified ("),
            "must not read as a verification, got {summary:?}"
        );
    }

    #[test]
    fn a_changed_binary_reads_differently_from_one_never_approved() {
        // These two states have opposite meanings and only one is alarming.
        // Collapsing them is the specific failure this pane must not have.
        assert_ne!(unapproved().summary(), superseded().summary());
        assert!(
            superseded().summary().to_lowercase().contains("changed"),
            "got {:?}",
            superseded().summary()
        );
    }

    #[test]
    fn only_a_changed_binary_gets_the_warning_colour() {
        // "Not approved yet" is the ordinary first-run state and must not be
        // dressed as a problem, or the warning colour stops meaning anything.
        let theme = Theme::default();
        assert_eq!(superseded().colour(theme), theme.status_warn);
        assert_ne!(unapproved().colour(theme), theme.status_warn);
        assert_ne!(approved().colour(theme), theme.status_warn);
    }

    #[test]
    fn an_approved_driver_is_not_painted_as_a_success() {
        // Green would say "verified" without a word being written.
        let theme = Theme::default();
        assert_ne!(approved().colour(theme), theme.status_ok);
    }

    #[test]
    fn every_state_naming_a_file_can_show_what_was_approved() {
        // The detail card renders from `path()`; a state that names a file but
        // reports none would silently drop the evidence half of the screen.
        for state in [unapproved(), superseded(), approved()] {
            assert!(state.path().is_some(), "{state:?} must expose its path");
        }
        for state in [
            TrustState::Unknown,
            TrustState::NotInstalled,
            TrustState::Problem {
                detail: "boom".into(),
            },
        ] {
            assert!(state.path().is_none(), "{state:?} names no file");
        }
    }

    mod mapping {
        use super::*;
        use oximux_computer_use::{Error, TrustBasis, VerifiedDriver, Version};
        use std::time::SystemTime;

        #[test]
        fn a_missing_driver_is_not_a_trust_problem() {
            // "Install something" and "decide something" are different asks.
            assert_eq!(
                TrustState::from_prepare(Err(Error::NotFound {
                    searched: vec!["$PATH".into()]
                })),
                TrustState::NotInstalled
            );
        }

        #[test]
        fn an_unapproved_driver_carries_the_digest_the_pane_must_show() {
            // The prompt cannot be drawn without it, and re-reading the file to
            // get it would be a second chance to read different bytes.
            let state = TrustState::from_prepare(Err(Error::NotApproved {
                path: PathBuf::from(r"C:\tools\cua-driver.exe"),
                sha256: "c".repeat(64),
            }));
            match state {
                TrustState::Unapproved { sha256, path, .. } => {
                    assert_eq!(sha256, "c".repeat(64));
                    assert_eq!(path, PathBuf::from(r"C:\tools\cua-driver.exe"));
                }
                other => panic!("expected Unapproved, got {other:?}"),
            }
        }

        #[test]
        fn a_superseded_driver_keeps_both_digests() {
            // The pane shows the found hash *and* the one that was approved —
            // without both, a user cannot tell which file they are looking at.
            let state = TrustState::from_prepare(Err(Error::TrustSuperseded {
                path: PathBuf::from(r"C:\tools\cua-driver.exe"),
                approved: "a".repeat(64),
                found: "b".repeat(64),
            }));
            match state {
                TrustState::Superseded {
                    approved, found, ..
                } => {
                    assert_eq!(approved, "a".repeat(64));
                    assert_eq!(found, "b".repeat(64));
                }
                other => panic!("expected Superseded, got {other:?}"),
            }
        }

        #[test]
        fn a_user_pinned_driver_resolves_to_approved() {
            let state = TrustState::from_prepare(Ok(VerifiedDriver {
                path: PathBuf::from(r"C:\tools\cua-driver.exe"),
                version: Version::new(0, 14, 2),
                sha256: "d".repeat(64),
                basis: TrustBasis::UserPinned {
                    approved_at: SystemTime::UNIX_EPOCH,
                },
            }));
            match state {
                TrustState::Approved { version, .. } => assert_eq!(version, "0.14.2"),
                other => panic!("expected Approved, got {other:?}"),
            }
        }

        #[test]
        fn an_unmapped_failure_still_reaches_the_user_verbatim() {
            // The catch-all must not swallow the reason. A driver below the
            // version floor is approved *and* unusable, and a pane that only
            // said "problem" would send the user hunting for a trust issue that
            // is not there.
            let state = TrustState::from_prepare(Err(Error::DriverTooOld {
                found: Version::new(0, 11, 0),
                minimum: Version::new(0, 12, 6),
            }));
            match state {
                TrustState::Problem { detail } => {
                    assert!(detail.contains("0.11.0"), "got {detail:?}");
                    assert!(detail.contains("0.12.6"), "got {detail:?}");
                }
                other => panic!("expected Problem, got {other:?}"),
            }
        }
    }
}
