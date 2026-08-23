//! Turn off GPUI's DirectComposition path on Windows so the browser pane can
//! actually be seen.
//!
//! # The bug this exists for
//!
//! `Ctrl+Shift+B` opened a browser tab that loaded correctly and rendered
//! nothing. The tab title resolved, the URL bar filled in, the page fired its
//! load event and the agent-context probes returned its full DOM — and the
//! content area stayed black. Everything worked except being visible.
//!
//! GPUI creates its window with `WS_EX_NOREDIRECTIONBITMAP` and presents
//! through a DirectComposition swapchain. That style is what makes a window
//! composited rather than redirected, and a window without a redirection
//! surface has nowhere for a *child HWND* to draw. WebView2 as wry builds it is
//! exactly that: `CreateCoreWebView2Controller` parents a child window into
//! ours. The child exists, receives input and runs the page; it simply never
//! composites.
//!
//! GPUI already carries the switch — `GPUI_DISABLE_DIRECT_COMPOSITION`, read
//! once when the platform initialises — because the same path breaks on some
//! drivers. This sets it for us rather than asking every user to.
//!
//! # Why this is global, and what it costs here
//!
//! GPUI reads the flag onto the *platform*, not the window, so it applies to
//! every window the process opens. There is no per-window form of it without
//! forking GPUI, and this repo takes GPUI as a plain git dependency — a
//! `[patch]` onto a vendored zed is a permanent maintenance cost.
//!
//! The thing a global switch would be expected to cost is transparency:
//! `WindowBackgroundAppearance::Transparent` needs the composited path. The app
//! asks for it in exactly one place, the floating usage popover — and that
//! window is opened only by the `#[cfg(target_os = "macos")]` arm of
//! `WorkspaceRoot::toggle_usage_popover`. Windows renders the same popover
//! inside the main window behind a flag, with no second window and nothing
//! transparent about it.
//!
//! So on the one platform this function touches, nothing in the app currently
//! wants what it gives up. That is a fact about today's code rather than a
//! guarantee: the check to make before adding a transparent window on Windows
//! is this one.
//!
//! # Why not the other repairs
//!
//! *Visual hosting.* WebView2 can draw into a composition visual instead of a
//! child window (`CreateCoreWebView2CompositionController`), which would keep
//! DirectComposition and fix the pane properly. wry does not expose it — there
//! is no mention of a composition controller anywhere in 0.55 — so this route
//! starts with a wry fork and ends with hand-managing input routing, which
//! visual hosting makes the embedder's job.
//!
//! *A separate top-level window over the pane.* Removes the child-HWND problem
//! by not having a child, and replaces it with keeping a second window pinned
//! to a rectangle through every move, resize, z-order change and workspace
//! switch — and with a webview that floats above GPUI's own overlays instead of
//! under them.
//!
//! # Ordering
//!
//! Must run before `gpui_platform::application()`, which is where GPUI reads
//! it, and before any thread starts, because writing the environment is only
//! sound while this process is single-threaded. It sits with the other two
//! environment writes at the top of `main` for that reason.

/// The variable GPUI reads, once, in `WindowsPlatform::new`.
///
/// It treats exactly `"true"` and `"1"` as on and anything else as off, which
/// is why [`WANTED`] is `"1"` rather than something more descriptive.
const VAR: &str = "GPUI_DISABLE_DIRECT_COMPOSITION";

/// What we set when nobody has said otherwise.
const WANTED: &str = "1";

/// Ask GPUI for the redirected present path unless the environment already
/// says otherwise.
///
/// A no-op off Windows: no other platform hosts the webview as a child window,
/// and no other platform reads this variable.
pub fn prefer_child_window_compositing() {
    #[cfg(windows)]
    {
        if !wants_write(std::env::var_os(VAR).as_deref()) {
            tracing::debug!(
                "{VAR} is already set; leaving it alone rather than forcing the browser pane's value"
            );
            return;
        }

        // SAFETY: called from the top of `main`, before the tokio runtime and
        // the GPUI executor exist, so no other thread can be reading the
        // environment. `set_var` is only unsound when it races a concurrent
        // read.
        unsafe { std::env::set_var(VAR, WANTED) };
        tracing::debug!(
            "disabled GPUI direct composition so the browser pane's child window can composite"
        );
    }
}

/// Whether to write [`VAR`], given whatever it holds now.
///
/// Split out from the write so the rule can be tested without touching the
/// process environment — an env-mutating test races every other test in the
/// binary, and the rule is the only part with a decision in it.
fn wants_write(existing: Option<&std::ffi::OsStr>) -> bool {
    existing.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// The default case, and the one that fixes the pane.
    #[test]
    fn an_unset_variable_is_written() {
        assert!(wants_write(None));
    }

    /// An explicit setting wins, including one that turns composition back
    /// *on*. Someone who never opens a browser tab and would rather keep the
    /// popover's corners can say so, and someone debugging a driver problem
    /// should not find their override quietly replaced by ours.
    #[test]
    fn any_explicit_setting_is_left_alone() {
        for value in ["1", "true", "0", "false", ""] {
            assert!(
                !wants_write(Some(OsStr::new(value))),
                "{value:?} is an explicit answer and should stand"
            );
        }
    }

    /// The whole fix is this one string matching what GPUI reads. A typo would
    /// be invisible: the app would start, the tab would open, and the page
    /// would be blank exactly as before.
    #[test]
    fn the_variable_is_spelled_the_way_gpui_spells_it() {
        assert_eq!(VAR, "GPUI_DISABLE_DIRECT_COMPOSITION");
        assert_eq!(WANTED, "1", "gpui accepts only \"1\" and \"true\"");
    }
}
