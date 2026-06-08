//! Open a URL in the user's default browser.
//!
//! macOS-only app, so `open <url>` is the portable launcher. Best-effort: a
//! spawn failure is logged, not surfaced — the URL is also in the tracing log,
//! and the caller has no better recovery than retry.

/// Launch `url` in the default browser. No-op on an empty string.
pub(crate) fn open_url(url: &str) {
    if url.is_empty() {
        return;
    }
    if let Err(err) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!(target: "oximux_app", ?err, url, "failed to open url in browser");
    }
}
