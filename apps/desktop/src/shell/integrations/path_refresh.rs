//! Making a just-installed tool visible to the process that installed it.
//!
//! `winget` installs a CLI and appends its directory to the **persisted** PATH
//! — the one in the registry, which every process started afterwards inherits.
//! This process was started before, so its own PATH still has the old value,
//! and `which` keeps reporting the tool as missing however many times the pane
//! re-probes. Nothing is wrong with the install; the app is simply looking at
//! a stale copy of the answer.
//!
//! Without this, "Install" would work perfectly and then appear not to have,
//! and the fix a user would eventually find is "restart the app" — which is
//! precisely what an inline remediation exists to avoid.
//!
//! Only Windows needs it. Homebrew installs into a prefix that is already on
//! PATH, and so does every Linux package manager: on those platforms a new
//! binary lands somewhere the process is already looking.

/// Re-read the OS's persisted PATH into this process.
///
/// Returns whether the process PATH actually changed — the caller uses that to
/// decide whether a re-probe is worth doing, not to decide whether the install
/// worked.
#[cfg(windows)]
pub(crate) fn refresh() -> bool {
    let Some(merged) = persisted_path() else {
        return false;
    };
    let current = std::env::var("PATH").unwrap_or_default();
    if merged.is_empty() || merged == current {
        return false;
    }
    // SAFETY: `set_var` is unsafe because on Unix a concurrent `getenv` can
    // observe a freed pointer. This function is Windows-only, where Rust's
    // implementation is `SetEnvironmentVariableW` and reads go through
    // `GetEnvironmentVariableW` — both thread-safe by contract, with the
    // process environment block owned and locked by the OS. There is no
    // equivalent hazard here, which is exactly why the Unix arm below does
    // nothing rather than doing this.
    unsafe { std::env::set_var("PATH", &merged) };
    tracing::info!(
        target: "oximux_app::integrations",
        "refreshed PATH from the registry after an install"
    );
    true
}

/// No-op. See the module doc: every other platform's package managers install
/// into a directory the process is already searching.
#[cfg(not(windows))]
pub(crate) fn refresh() -> bool {
    false
}

/// The machine PATH and the user PATH, joined the way Windows joins them.
///
/// Read through PowerShell rather than the registry directly, for one reason
/// that is not convenience: both values are usually `REG_EXPAND_SZ` and carry
/// literal `%SystemRoot%`-style references. `GetEnvironmentVariable` expands
/// them; a raw registry read would hand back placeholders and quietly break
/// every path that used one.
///
/// Spawned with an absolute path because the whole point of this function is
/// that PATH cannot currently be trusted.
#[cfg(windows)]
fn persisted_path() -> Option<String> {
    use oximux_no_window::NoWindow as _;

    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let shell = format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    let output = std::process::Command::new(shell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Environment]::GetEnvironmentVariable('Path','Machine') + ';' + \
             [Environment]::GetEnvironmentVariable('Path','User')",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .no_window()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(normalize(&String::from_utf8_lossy(&output.stdout)))
}

/// Tidy the joined value: trim, and drop the empty segments that a machine or
/// user PATH ending in `;` produces when the two are concatenated.
///
/// An empty PATH segment means "the current directory" on Windows, so leaving
/// them in would silently put the working directory on the search path of
/// every process this app spawns from then on.
#[cfg(windows)]
fn normalize(raw: &str) -> String {
    raw.trim()
        .split(';')
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn empty_segments_are_dropped_because_they_mean_the_cwd() {
        assert_eq!(normalize("C:\\a;;C:\\b"), "C:\\a;C:\\b");
        assert_eq!(normalize("  C:\\a;C:\\b;  "), "C:\\a;C:\\b");
        // The join of a machine PATH ending in `;` with a user PATH.
        assert_eq!(normalize("C:\\machine;;C:\\user"), "C:\\machine;C:\\user");
    }

    #[test]
    fn a_single_entry_survives_intact() {
        assert_eq!(normalize("C:\\Windows\\System32"), "C:\\Windows\\System32");
    }

    #[test]
    fn nothing_at_all_stays_nothing() {
        assert_eq!(normalize("   "), "");
        assert_eq!(normalize(";;;"), "");
    }

    /// Against the real registry. Not an assertion about this machine's PATH —
    /// it asserts the read works and comes back looking like a PATH, which is
    /// the half that breaks silently.
    #[test]
    fn the_persisted_path_is_readable_and_expanded() {
        let path = persisted_path().expect("PowerShell is present on Windows");
        assert!(!path.is_empty());
        assert!(
            !path.contains('%'),
            "unexpanded placeholders survived: {path}"
        );
        assert!(
            path.to_lowercase().contains("system32"),
            "a PATH without System32 is not a PATH: {path}"
        );
    }
}
