//! A POSIX `sh` for test fixtures, found even where PATH has none.
//!
//! The fake agents in this crate's tests are little `sh -c` scripts — a JSON-RPC
//! loop is comfortably within `sh`'s language and comfortably outside `cmd`'s.
//! On unix `Command::new("sh")` is the end of the story. On Windows there is no
//! `sh` on a stock PATH, but there IS one on every machine that can build this
//! repo at all: Git for Windows ships a full MSYS userland (`sh`, `sed`, `yes`,
//! `head` — everything the fixtures use), and its runtime resolves `/usr/bin`
//! itself, so the scripts run unmodified no matter what PATH the test runner
//! inherited. GitHub's `windows-latest` runners carry the same layout.
//!
//! Resolution: `sh` on PATH wins (an MSYS2 or Cygwin user's own choice), then
//! the `bin\sh.exe` sibling of the `git` on PATH (`<root>\cmd\git.exe` →
//! `<root>\bin\sh.exe`). A machine with neither panics with instructions
//! rather than failing 26 tests with a bare "program not found".

use std::process::Command;

/// A `Command` on the resolved `sh`, no args yet — call sites chain their own
/// `.arg("-c").arg(script)`, keeping each script beside the test that owns it.
pub(crate) fn sh_command() -> Command {
    Command::new(sh_program())
}

/// `sh -c <script>`, for the one-expression call sites.
pub(crate) fn sh_script(script: &str) -> Command {
    let mut cmd = sh_command();
    cmd.arg("-c").arg(script);
    cmd
}

#[cfg(not(windows))]
fn sh_program() -> std::path::PathBuf {
    std::path::PathBuf::from("sh")
}

#[cfg(windows)]
fn sh_program() -> std::path::PathBuf {
    static SH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    SH.get_or_init(|| {
        if let Ok(p) = which::which("sh") {
            return p;
        }
        if let Ok(git) = which::which("git") {
            // <root>\cmd\git.exe → <root>\bin\sh.exe
            if let Some(root) = git.parent().and_then(|p| p.parent()) {
                let candidate = root.join("bin").join("sh.exe");
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        panic!(
            "these fixtures need a POSIX sh and none was found: put `sh` on PATH \
             or install Git for Windows (its bin\\sh.exe is picked up via `git`)"
        );
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resolver's whole contract: the returned program spawns and the
    /// script's stdout comes back. Covers the PATH-less-PowerShell case on
    /// Windows because `cargo test` there IS that environment.
    #[test]
    fn resolved_sh_runs_a_script() {
        let out = sh_script("printf ok").output().expect("sh spawns");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"ok");
    }
}
