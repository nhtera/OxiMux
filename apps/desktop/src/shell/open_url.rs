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

/// Launch `http://localhost:<port>` in the default browser.
///
/// A separate entrance rather than a relaxation of [`open_url`]'s scheme
/// guard, and the distinction is the whole point. That guard exists because
/// `open_url` forwards a string that came from somewhere else — an issue body,
/// a PR description — where a crafted scheme is an arbitrary local action.
/// Here there is no string to craft: the caller supplies a `u16` and the URL
/// is built from it, so the only thing this can reach is a port on this
/// machine.
///
/// Deliberately **not** the embedded browser pane, even though the app ships
/// one. Building a WebView2 child window on Windows pumps the message loop
/// mid-construction, which re-enters GPUI while the `App` is already borrowed;
/// any foreground task pending at that moment — a terminal's poll task, which
/// is to say almost always — then panics with "RefCell already borrowed" and
/// takes the process with it. That fault is pre-existing on the
/// `NewBrowserTab` path and is not this module's to fix, but a ports row must
/// not become a second, easier way to hit it.
pub(crate) fn open_loopback_port(port: u16, cx: &gpui::App) {
    cx.open_url(&crate::shell::ports_panel::labels::url_for(port));
}
