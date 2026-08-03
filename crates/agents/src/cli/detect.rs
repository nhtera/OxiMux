//! Shared detection helper for CLI adapters.
//!
//! Every concrete `CliAgentAdapter` implementation answers `detect()` by
//! asking "is this binary on PATH?" The first adapter (`claude_code.rs`,
//! step 5) inlined the helper; the second (`codex.rs`, step 6) is its
//! second caller — promoting now keeps the third (`aider.rs`, step 7)
//! from sprouting a fresh copy.
//!
//! Contract: never `Err`. A missing tool is `Ok(false)`, not a panic —
//! the startup registry calls this for every adapter at cold boot and a
//! failure here would prevent any adapter from showing up in the dialog.

use std::path::PathBuf;

/// Returns `true` if `bin` resolves to something on `$PATH`.
///
/// A miss, an unset `PATH`, or any I/O error all fold to `false` — the only
/// meaningful answer this helper owes its caller is "available, yes or no".
pub async fn which_on_path(bin: &str) -> bool {
    resolve_on_path(bin).await.is_some()
}

/// The absolute path `bin` resolves to on `$PATH`, or `None`.
///
/// Uses the `which` *crate* rather than shelling out to the `which` *binary*,
/// which this did until the Windows port. Two reasons, in order of severity:
/// that binary does not exist on Windows, so every first run would have
/// reported zero agents installed; and the crate applies `PATHEXT` there, which
/// is what makes a bare `claude` resolve to the `claude.cmd` shim npm actually
/// installs — a plain PATH walk would miss it. Not spawning a process per
/// lookup is a bonus, not the point.
///
/// `spawn_blocking` because resolution stats each PATH entry, and a
/// network-mounted directory on PATH makes that slow; callers run on the boot
/// path and on the UI thread's executor.
pub async fn resolve_on_path(bin: &str) -> Option<PathBuf> {
    let bin = bin.to_owned();
    tokio::task::spawn_blocking(move || resolve_on_path_blocking(&bin))
        .await
        .ok()
        .flatten()
}

/// [`resolve_on_path`] without a runtime.
///
/// Spawn sites are synchronous — they are called from the UI thread, which has
/// nothing to await on — and resolving is what makes a bare name work on
/// Windows at all, so they need an answer without one. The cost is a handful of
/// `stat`s; `which` does not spawn a process.
pub fn resolve_on_path_blocking(bin: &str) -> Option<PathBuf> {
    which::which(bin).ok()
}

/// The program to hand `Command::new` for a bare agent binary name.
///
/// Resolving first is not an optimisation. Rust's Windows spawn appends `.exe`
/// and does not consult `PATHEXT`, so a bare name never finds the `claude.cmd`
/// / `codex.cmd` shim npm installs — while detection, which goes through
/// `which`, does find it. Left unresolved the two disagree, and an agent is
/// advertised as installed and then fails to launch.
///
/// Falls back to the bare name on a miss so the failure stays a plain
/// "not found" from the spawn rather than a different error shape here.
pub fn program_for_spawn(bin: &str) -> PathBuf {
    resolve_on_path_blocking(bin).unwrap_or_else(|| PathBuf::from(bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell's leaf name is on PATH on every macOS / Linux box, and `cmd` is
    /// on every Windows one. If an assertion using this fails, the test runner
    /// has a broken environment — not a code regression.
    fn ambient_binary() -> &'static str {
        if cfg!(windows) { "cmd" } else { "sh" }
    }

    #[tokio::test]
    async fn which_on_path_finds_known_binary() {
        let bin = ambient_binary();
        assert!(which_on_path(bin).await, "{bin} must be on PATH");
    }

    #[tokio::test]
    async fn which_on_path_returns_false_for_garbage_name() {
        // Random-looking name that no sane installer would ship under.
        // Confirms the false branch never panics on a clean miss.
        assert!(!which_on_path("oximux-this-binary-should-not-exist-xyz").await);
    }

    #[tokio::test]
    async fn resolve_on_path_returns_an_absolute_path() {
        let resolved = resolve_on_path(ambient_binary()).await.expect("must resolve");
        assert!(
            resolved.is_absolute(),
            "callers hand this to a detached daemon to exec, so a relative path \
             would resolve against the wrong cwd: {}",
            resolved.display()
        );
    }

    #[tokio::test]
    async fn resolve_on_path_is_none_for_garbage_name() {
        assert!(
            resolve_on_path("oximux-this-binary-should-not-exist-xyz")
                .await
                .is_none()
        );
    }
}
