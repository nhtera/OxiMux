//! `CREATE_NO_WINDOW` for every child the app spawns.
//!
//! # The failure this exists to stop
//!
//! The packaged Windows build is a GUI-subsystem process — it has no console.
//! When a process without a console spawns a console-subsystem child (`git`,
//! `rg`, the `node` behind every npm-installed agent CLI, an LSP server),
//! Windows conjures a brand-new console window for that child. Its stdio is
//! piped, so the window is empty — the user just sees bare terminal windows
//! appear: one per LSP server, one per restored agent session, and a fresh
//! flash every 500 ms courtesy of the git status poller.
//!
//! Nothing in the app ever wants that window. Every child either talks over
//! pipes or over a ConPTY (which renders into OxiMux itself), so the console
//! Windows offers to create has nothing to show — `CREATE_NO_WINDOW` declines
//! it. On every other platform there is no such window and this is a no-op,
//! which is why call sites apply it unconditionally instead of behind a `cfg`.
//!
//! # Why a crate
//!
//! The same reason as `owner-only` and `job-object`: the spawn sites live in
//! crates that share no other dependency (`git`, `agents`, `editor`,
//! `dictation`, the desktop app…), and a copy-pasted flag constant is exactly
//! the kind of thing the next spawn site forgets. One trait, imported where a
//! `Command` is built, keeps the fix greppable and the omission visible.
//!
//! Note for future spawn sites: `creation_flags` *replaces* previously set
//! flags rather than OR-ing into them, so a site that needs another flag
//! (e.g. `CREATE_NEW_PROCESS_GROUP`) must pass the union itself instead of
//! stacking a manual `creation_flags` call on top of `no_window()`.

/// `dwCreationFlags` bit: run the child without a console at all.
///
/// Chosen over `DETACHED_PROCESS` deliberately — `CREATE_NO_WINDOW` still
/// gives the child a (hidden) console, so children that probe their console
/// handles or take `CTRL_SHUTDOWN_EVENT` keep working; detaching outright
/// breaks console-API users in ways that only surface at runtime.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Suppress the console window a GUI-subsystem parent would otherwise get
/// for a console-subsystem child. No-op on non-Windows platforms.
pub trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl NoWindow for tokio::process::Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both impls must chain — every call site uses builder style.
    #[test]
    fn std_command_chains() {
        let mut cmd = std::process::Command::new("git");
        let _: &mut std::process::Command = cmd.no_window().arg("--version");
    }

    #[test]
    fn tokio_command_chains() {
        let mut cmd = tokio::process::Command::new("git");
        let _: &mut tokio::process::Command = cmd.no_window().arg("--version");
    }

    /// On Windows the child must really run without a console. `cmd /c ver`
    /// exits 0 whether or not a console exists, so the assertion here is only
    /// that the flag doesn't break spawning; the "no window appears" half is
    /// inherently visual and is covered by the flag being the documented
    /// `CREATE_NO_WINDOW` bit.
    #[cfg(windows)]
    #[test]
    fn flagged_child_still_spawns_and_exits_zero() {
        let status = std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .no_window()
            .status()
            .expect("cmd /c ver spawns");
        assert!(status.success());
    }
}
