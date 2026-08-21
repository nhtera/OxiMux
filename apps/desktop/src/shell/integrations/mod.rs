//! Integrations — the external CLIs OxiMux calls out to, and their health.
//!
//! **Why this exists at all.** Half of OxiMux's surfaces are wrappers around a
//! command-line tool: Source Control is `git`, the Tasks page is `gh` or
//! `glab`, Search is `rg`. When one of those is absent the surface does not
//! break loudly — it goes quiet. An empty Tasks page looks exactly like a repo
//! with no issues, and a Search panel that finds nothing looks exactly like a
//! search with no matches. The user's conclusion is "this feature is bad",
//! not "this feature is unplugged".
//!
//! So the pane's job is not to list dependencies. It is to convert a silent
//! degradation into a sentence and a button, in the one place a person goes
//! looking when something seems broken.
//!
//! **The remediation is inline for the same reason.** A status page that
//! diagnoses a problem and then tells you to go elsewhere has moved the work
//! without reducing it. The card that says `gh` is missing is the card that
//! installs it, and the card that says it is signed out is the card that tells
//! you the one command that signs it in.
//!
//! Split three ways so the parts can be reasoned about separately:
//!
//! * [`catalog`] — what could be here. Pure data and pure wording.
//! * [`probe`] — what is here. Blocking syscalls, background thread only.
//! * [`install`] — making it be here. A package-manager child process and a
//!   small state machine, in the same shape as the driver install.
//!
//! The view is [`crate::shell::settings_modal::pane_integrations`].

pub(crate) mod catalog;
pub(crate) mod install;
pub(crate) mod path_refresh;
pub(crate) mod probe;

use catalog::Tool;
use probe::Health;

/// One tool's row in the pane: what it is, what the machine says about it, and
/// whether a package manager is standing by.
#[derive(Clone, Debug)]
pub(crate) struct IntegrationRow {
    pub tool: Tool,
    pub health: Health,
    /// Whether this platform has a package manager that is actually installed.
    /// Recorded per row rather than globally because the answer is per
    /// platform *and* per tool — a future recipe could name a different
    /// manager for one tool.
    pub manager_ready: bool,
}

impl IntegrationRow {
    /// The state a row starts in, before any probe has run.
    pub(crate) fn checking(tool: Tool) -> Self {
        Self {
            tool,
            health: Health::Checking,
            manager_ready: false,
        }
    }

    /// Whether the pane should offer to install this tool: something is
    /// missing, and there is a manager here that can fix it.
    pub(crate) fn can_install(&self) -> bool {
        self.health.wants_install() && self.manager_ready
    }
}

/// Re-read the OS's PATH, then probe every tool. **Blocking — background
/// thread only.**
///
/// The sweep to run *after* an install, and only then. A tool that winget just
/// wrote is on the persisted PATH and not on this process's copy of it, so a
/// plain [`probe_all`] would report the successful install as a failed one.
/// The refresh is not free (it spawns a shell) and would be pure waste on the
/// every-open sweep, which is why it is a second entry point rather than a
/// step inside the first.
pub(crate) fn refresh_path_and_probe_all() -> Vec<IntegrationRow> {
    path_refresh::refresh();
    probe_all()
}

/// Probe every tool. **Blocking — background thread only.**
///
/// One pass over the whole catalog rather than a probe per card, because the
/// pane refreshes as a unit and a half-updated list is a list nobody can
/// trust: the point of the pane is the shape of the whole answer.
pub(crate) fn probe_all() -> Vec<IntegrationRow> {
    Tool::ALL
        .into_iter()
        .map(|tool| {
            let health = probe::probe(tool);
            // Only asked when it could change what the card offers. Resolving
            // a package manager is cheap, but it is a `stat` per PATH entry
            // and there is no reason to pay it for a healthy tool.
            let manager_ready = health.wants_install()
                && tool
                    .install_recipe()
                    .is_some_and(|r| probe::manager_available(r.manager));
            IntegrationRow {
                tool,
                health,
                manager_ready,
            }
        })
        .collect()
}

/// How many tools are not doing their job — the count the nav badge and the
/// pane header both read.
pub(crate) fn unhealthy_count(rows: &[IntegrationRow]) -> usize {
    rows.iter()
        .filter(|row| !matches!(row.health, Health::Ready { .. } | Health::Checking))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tool: Tool, health: Health, manager_ready: bool) -> IntegrationRow {
        IntegrationRow {
            tool,
            health,
            manager_ready,
        }
    }

    #[test]
    fn a_row_starts_checking_and_offers_nothing() {
        let row = IntegrationRow::checking(Tool::GithubCli);
        assert_eq!(row.health, Health::Checking);
        assert!(
            !row.can_install(),
            "offering an install before the probe lands would flash a button at a healthy machine"
        );
    }

    #[test]
    fn an_install_needs_both_a_gap_and_a_manager() {
        assert!(row(Tool::GithubCli, Health::Missing, true).can_install());
        assert!(
            !row(Tool::GithubCli, Health::Missing, false).can_install(),
            "no manager here — the card must offer the manual route instead"
        );
        assert!(
            !row(Tool::GithubCli, Health::Ready { version: None }, true).can_install(),
            "nothing to fix"
        );
    }

    #[test]
    fn checking_is_not_counted_as_unhealthy() {
        // Otherwise the badge flashes a number on every open and then clears.
        let rows = vec![
            row(Tool::Git, Health::Checking, false),
            row(Tool::Ripgrep, Health::Checking, false),
        ];
        assert_eq!(unhealthy_count(&rows), 0);
    }

    #[test]
    fn a_signed_out_cli_counts_as_unhealthy() {
        // It is installed, so it is not "missing" — but it fails every call it
        // is asked to make, which is the thing the badge is about.
        let rows = vec![
            row(Tool::Git, Health::Ready { version: None }, false),
            row(Tool::GithubCli, Health::SignedOut { version: None }, false),
            row(Tool::GitlabCli, Health::Missing, true),
        ];
        assert_eq!(unhealthy_count(&rows), 2);
    }

    #[test]
    fn a_fully_healthy_machine_counts_zero() {
        let rows: Vec<_> = Tool::ALL
            .into_iter()
            .map(|t| row(t, Health::Ready { version: None }, false))
            .collect();
        assert_eq!(unhealthy_count(&rows), 0);
    }

    /// Against the real machine. Asserts the sweep answers for every tool and
    /// never offers an install it cannot perform.
    #[test]
    fn the_sweep_covers_the_catalog_and_stays_consistent() {
        let rows = probe_all();
        assert_eq!(rows.len(), Tool::ALL.len());
        for row in &rows {
            assert_ne!(row.health, Health::Checking);
            if row.can_install() {
                assert!(
                    row.tool.install_recipe().is_some(),
                    "{:?} offers an install with no recipe behind it",
                    row.tool
                );
            }
        }
    }
}
