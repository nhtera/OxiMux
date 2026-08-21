//! Asking the machine what it actually has.
//!
//! Blocking on purpose, and called only from a background thread. Each probe
//! is a PATH resolution plus at most two short process spawns, which is fast
//! on a warm machine and emphatically not fast on a cold one with a network
//! share on PATH — so it never touches the UI thread.
//!
//! Every "can't tell" answer resolves to the pessimistic one. A tool that
//! cannot be run is reported missing rather than assumed fine: the whole point
//! of the pane is to explain a surface that is already not working, and an
//! optimistic probe would say everything is healthy on exactly the machine
//! where something is not.

use std::process::{Command, Stdio};
use std::time::Duration;

use oximux_no_window::NoWindow as _;

use super::catalog::Tool;

/// How long a version or auth probe may take before it is treated as a
/// failure.
///
/// Generous, because `gh auth status` talks to the network on a cold cache and
/// a 1-second cap would report a working install as broken. Bounded, because
/// this runs on a refresh the user can trigger repeatedly.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// What the pane knows about one tool right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Health {
    /// No probe has finished yet. Distinct from `Missing` so a slow first
    /// paint never accuses the machine of lacking something.
    Checking,
    Missing,
    /// On PATH, but the sign-in step has not been done. Only the forge CLIs
    /// can report this.
    SignedOut { version: Option<String> },
    Ready { version: Option<String> },
}

impl Health {
    /// Whether this tool is doing its job.
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Health::Ready { .. })
    }

    /// Whether an install would help. A signed-out CLI is installed already —
    /// offering to install it again is the single most useless button the pane
    /// could grow.
    pub(crate) fn wants_install(&self) -> bool {
        matches!(self, Health::Missing)
    }
}

/// Probe one tool. **Blocking — background thread only.**
pub(crate) fn probe(tool: Tool) -> Health {
    let Some(program) = locate(tool) else {
        return Health::Missing;
    };
    let version = run_version(&program);
    if !tool.has_sign_in() {
        return Health::Ready { version };
    }
    if signed_in(&program, tool) {
        Health::Ready { version }
    } else {
        Health::SignedOut { version }
    }
}

/// Whether a package manager is usable here, so the pane can offer its recipe
/// rather than a command that will not run.
pub(crate) fn manager_available(manager: &str) -> bool {
    oximux_agents::cli::resolve_on_path_blocking(manager).is_some()
}

/// Where `tool`'s binary is, if anywhere.
///
/// ripgrep goes through the same resolution the app itself uses, not a bare
/// PATH lookup: the packaged build ships `rg` beside the main binary, and a
/// pane that reported "Not installed" for a bundled copy the Search panel is
/// happily using would be reporting on the wrong machine.
fn locate(tool: Tool) -> Option<std::path::PathBuf> {
    if tool.is_bundled() {
        let bundled = crate::shell::tool_paths::rg_program();
        if bundled.is_absolute() {
            return Some(bundled.to_path_buf());
        }
    }
    oximux_agents::cli::resolve_on_path_blocking(tool.binary())
}

/// The tool's own version string, or `None` when it will not say.
fn run_version(program: &std::path::Path) -> Option<String> {
    let out = run(Command::new(program).arg("--version"))?;
    parse_version(&out)
}

/// Whether the forge CLI has credentials.
///
/// Both CLIs spell it the same way and both exit non-zero when signed out,
/// which is the whole classification — there is no output to parse and no
/// reason to invent one.
fn signed_in(program: &std::path::Path, tool: Tool) -> bool {
    debug_assert!(tool.has_sign_in());
    let mut cmd = Command::new(program);
    cmd.args(["auth", "status"]);
    // Here the exit code *is* the answer, so unlike `run_version` a non-zero
    // exit is a result rather than a failure — hence the separate call shape.
    match spawn_bounded(&mut cmd) {
        Some(output) => output.status.success(),
        None => false,
    }
}

/// Run a command and return its stdout, or `None` if it failed in any way.
fn run(cmd: &mut Command) -> Option<String> {
    let output = spawn_bounded(cmd)?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Spawn with no console window, no inherited stdin, and a wall-clock bound.
///
/// The bound is why this is not just `Command::output()`: a forge CLI waiting
/// on an unreachable host would otherwise hold the probe thread open
/// indefinitely, and the pane's Re-check button would appear to do nothing
/// forever.
fn spawn_bounded(cmd: &mut Command) -> Option<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .no_window()
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// Pull a version number out of whatever a tool prints for `--version`.
///
/// The four tools print four different shapes — `git version 2.35.0`,
/// `gh version 2.98.0 (2026-01-01)`, `glab 1.114.0`, `ripgrep 15.2.0` — so the
/// rule is structural rather than per-tool: the first whitespace-separated
/// token that starts with a digit and contains a dot. A tool that changes its
/// banner keeps working; a tool this cannot parse loses its version line and
/// nothing else.
fn parse_version(raw: &str) -> Option<String> {
    raw.lines().next()?.split_whitespace().find_map(|token| {
        let trimmed = token.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
        let starts_numeric = trimmed.chars().next().is_some_and(|c| c.is_ascii_digit());
        (starts_numeric && trimmed.contains('.')).then(|| trimmed.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_found_in_every_banner_shape_we_ship_against() {
        assert_eq!(
            parse_version("git version 2.35.0.windows.1"),
            Some("2.35.0.windows.1".to_string())
        );
        assert_eq!(
            parse_version("gh version 2.98.0 (2026-01-14)\nhttps://github.com/cli/cli"),
            Some("2.98.0".to_string())
        );
        assert_eq!(parse_version("glab 1.114.0\n"), Some("1.114.0".to_string()));
        assert_eq!(parse_version("ripgrep 15.2.0"), Some("15.2.0".to_string()));
    }

    #[test]
    fn only_the_first_line_is_considered() {
        // gh prints a second line with a URL that contains dots and digits.
        assert_eq!(
            parse_version("gh version 2.98.0\nhttps://cli.github.com/1.2.3"),
            Some("2.98.0".to_string())
        );
    }

    #[test]
    fn an_unparseable_banner_costs_the_version_and_nothing_else() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("some tool, version unknown"), None);
        // A bare integer is not a version — "gh 3" would be a banner we do not
        // understand, and guessing is worse than staying quiet.
        assert_eq!(parse_version("tool 3"), None);
    }

    #[test]
    fn surrounding_punctuation_is_not_part_of_the_version() {
        assert_eq!(parse_version("tool (1.2.3)"), Some("1.2.3".to_string()));
    }

    #[test]
    fn checking_is_not_missing() {
        // A slow first paint must never render as an accusation.
        assert!(!Health::Checking.wants_install());
        assert!(Health::Missing.wants_install());
    }

    #[test]
    fn a_signed_out_cli_is_not_offered_an_install() {
        let signed_out = Health::SignedOut {
            version: Some("2.98.0".into()),
        };
        assert!(!signed_out.is_ready());
        assert!(
            !signed_out.wants_install(),
            "it is already installed; the missing step is a sign-in"
        );
    }

    /// Against the real machine. Not an assertion about what is installed on
    /// it — it asserts the probe answers at all, for every tool, without
    /// hanging or panicking.
    #[test]
    fn every_tool_probes_to_a_definite_answer() {
        for tool in Tool::ALL {
            let health = probe(tool);
            assert_ne!(
                health,
                Health::Checking,
                "{:?} must resolve; Checking is the pane's state, not a probe result",
                tool
            );
        }
    }

    #[test]
    fn a_binary_that_cannot_exist_is_not_a_manager() {
        assert!(!manager_available("definitely-not-a-real-package-manager"));
    }
}
