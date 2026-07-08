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

use tokio::process::Command;

/// Returns `true` if `bin` resolves to something on `$PATH`.
///
/// Shells out to `which <bin>` and treats a 0 exit as "installed". A
/// non-zero exit, missing `which`, or any I/O error all fold to `false`
/// — the only meaningful answer this helper owes its caller is
/// "available, yes or no".
///
/// macOS-only target (per the v1 plan); `which` ships at `/usr/bin/which`
/// on every macOS install so the bare-name lookup is safe.
pub async fn which_on_path(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn which_on_path_finds_known_binary() {
        // /bin/sh's leaf name is on PATH on every macOS / Linux box. If
        // this assertion fails, the test runner has a broken shell env —
        // not a code regression.
        assert!(which_on_path("sh").await, "sh must be on PATH");
    }

    #[tokio::test]
    async fn which_on_path_returns_false_for_garbage_name() {
        // Random-looking name that no sane installer would ship under.
        // Confirms the false branch never panics on a clean miss.
        assert!(!which_on_path("oximux-this-binary-should-not-exist-xyz").await);
    }
}
