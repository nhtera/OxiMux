//! Open a URL in the user's default browser.
//!
//! Delegates to [`gpui::App::open_url`] — the platform layer already knows how
//! to launch a browser everywhere (NSWorkspace on macOS, ShellExecute on
//! Windows), and never pops a console window doing it. This wrapper exists for
//! the scheme guard, not the launching.

/// Launch `url` in the default browser. No-op on an empty string.
/// Only `https://` URLs are forwarded — `file://`, `applescript://`, and
/// other schemes are silently ignored to prevent a crafted issue URL from
/// executing arbitrary local actions through the system opener.
pub(crate) fn open_url(url: &str, cx: &gpui::App) {
    if url.is_empty() {
        return;
    }
    if !url.starts_with("https://") {
        tracing::warn!(target: "oximux_app", url, "open_url: blocked non-https scheme");
        return;
    }
    cx.open_url(url);
}
