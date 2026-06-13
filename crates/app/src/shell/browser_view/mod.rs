//! `BrowserView` — a pane-group leaf that hosts a native webview behind a
//! minimal browser chrome (back / forward / reload + address bar).
//!
//! The webview itself is a native sibling view layered over the window's
//! GPU canvas (see [`native::NativeWebview`]); this entity owns it, pins
//! its frame from a laid-out anchor each paint, drives navigation, and
//! hides it when the tab is inactive or covered by a modal.
//!
//! Only the URL is persistent state — restore re-navigates. No relay, no
//! PTY: a pure in-process leaf like the editor / diff views.

mod native;
mod render;

use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AppContext, Context, FocusHandle, Focusable, SharedString, Task, Window};
use gpui_component::input::InputState;
use oximux_settings::{Density, Theme, Typography};
use wry::PageLoadEvent;

use native::{NativeWebview, WebviewCallbacks};

/// Document-start script — installed on every navigation. Kept minimal for
/// P1 (a marker only); P2 layers the console/error ring buffers here.
const INIT_SCRIPT: &str = "/* oximux browser */";

/// Privacy-preserving default search endpoint used when the address bar
/// content is not a URL. A query string, not a product reference.
const SEARCH_ENDPOINT: &str = "https://duckduckgo.com/?q=";

/// Default landing page for a fresh browser tab.
pub const DEFAULT_URL: &str = "https://duckduckgo.com/";

/// Set by the workspace each render: `true` when a modal/overlay covers the
/// pane area. Browser webviews are native views layered ABOVE the GPU canvas,
/// so they must hide while a modal is up (else they float over it). Read by
/// `PaneGroup` in its per-render visibility sweep.
#[derive(Default)]
pub struct WebviewSuppressed(pub bool);

impl gpui::Global for WebviewSuppressed {}

/// Convenience reader — `false` when the global was never set.
pub fn webview_suppressed(cx: &gpui::App) -> bool {
    cx.try_global::<WebviewSuppressed>().is_some_and(|g| g.0)
}

/// Page-chrome change forwarded from a webview callback (main thread) into
/// the entity over a channel, so updates fold into one `cx.notify()`.
enum ChromeEvent {
    Title(String),
    Navigated(String),
    LoadStarted,
    LoadFinished(String),
}

pub struct BrowserView {
    /// `None` if the native webview failed to build — render shows an error.
    native: Option<Rc<NativeWebview>>,
    /// Live URL (seeded from the initial url; updated on navigation). The
    /// persistence source of truth.
    url: String,
    title: String,
    loading: bool,
    /// `true` while this tab is the active, uncovered tab — drives webview
    /// visibility. Set by the host each render (`set_active`).
    active: bool,
    address: gpui::Entity<InputState>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    _events: Task<()>,
}

impl BrowserView {
    pub fn new(
        url: impl Into<String>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url = url.into();
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter address"));
        address.update(cx, |s, cx| {
            s.set_value(SharedString::from(url.clone()), window, cx)
        });

        // Channel: webview callbacks (main run loop) → entity event loop.
        let (tx, mut rx) = unbounded::<ChromeEvent>();
        let native = build_native(window, &url, tx).map(Rc::new);
        if native.is_none() {
            tracing::warn!("BrowserView: native webview build failed; tab will show an error state");
        }

        let events = cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |view, cx| view.on_chrome_event(ev, cx)).is_err() {
                    return;
                }
            }
        });

        Self {
            native,
            url,
            title: String::new(),
            loading: false,
            // Starts hidden (webview built with `with_visible(false)`); the
            // host's first render sweep flips it on for the active, uncovered
            // tab. `set_active` only drives the native view when this changes,
            // so the initial `false` must match the webview's built state.
            active: false,
            address,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _events: events,
        }
    }

    fn on_chrome_event(&mut self, ev: ChromeEvent, cx: &mut Context<Self>) {
        match ev {
            ChromeEvent::Title(t) => self.title = t,
            ChromeEvent::Navigated(u) => self.url = u,
            ChromeEvent::LoadStarted => self.loading = true,
            ChromeEvent::LoadFinished(u) => {
                self.loading = false;
                if !u.is_empty() {
                    self.url = u;
                }
            }
        }
        cx.notify();
    }

    /// Show/hide the webview. The host computes `active && !covered` each
    /// render; an inactive or covered tab hides the native view immediately
    /// (its `render` won't run to do it lazily).
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if let Some(native) = &self.native {
            native.set_visible(active);
        }
    }

    /// Navigate to whatever is in the address bar: a URL as-is, a bare
    /// domain promoted to `https://`, otherwise a search query.
    fn submit_address(&mut self, cx: &mut Context<Self>) {
        let raw = self.address.read(cx).value().to_string();
        let target = normalize_address(&raw);
        if let Some(native) = &self.native {
            native.load_url(&target);
        }
        self.url = target;
        cx.notify();
    }

    fn go_back(&self) {
        if let Some(n) = &self.native {
            n.go_back();
        }
    }

    fn go_forward(&self) {
        if let Some(n) = &self.native {
            n.go_forward();
        }
    }

    fn reload(&self) {
        if let Some(n) = &self.native {
            n.reload();
        }
    }

    /// Live URL — persistence reads this so a restored tab reopens where the
    /// user left off (including link-click navigations).
    pub fn current_url(&self) -> String {
        self.url.clone()
    }

    /// Tab-chip label: the page title when known, else the URL host.
    pub fn chrome_label(&self) -> SharedString {
        if !self.title.is_empty() {
            return SharedString::from(self.title.clone());
        }
        SharedString::from(host_of(&self.url).unwrap_or_else(|| "Browser".to_string()))
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Build the native webview, wiring its callbacks to forward chrome events
/// over `tx`. Split out so the closures' `'static` capture of `tx` clones
/// stays readable.
fn build_native(
    window: &Window,
    url: &str,
    tx: UnboundedSender<ChromeEvent>,
) -> Option<NativeWebview> {
    let nav_tx = tx.clone();
    let title_tx = tx.clone();
    let load_tx = tx;
    let callbacks = WebviewCallbacks {
        on_navigation: move |u: String| {
            let _ = nav_tx.unbounded_send(ChromeEvent::Navigated(u));
            true
        },
        on_title: move |t: String| {
            let _ = title_tx.unbounded_send(ChromeEvent::Title(t));
        },
        on_load: move |event: PageLoadEvent, u: String| match event {
            PageLoadEvent::Started => {
                let _ = load_tx.unbounded_send(ChromeEvent::LoadStarted);
            }
            PageLoadEvent::Finished => {
                let _ = load_tx.unbounded_send(ChromeEvent::LoadFinished(u));
            }
        },
    };
    NativeWebview::build(window, url, INIT_SCRIPT, callbacks).ok()
}

/// Normalize address-bar text into a navigable URL.
fn normalize_address(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return DEFAULT_URL.to_string();
    }
    if t.starts_with("http://")
        || t.starts_with("https://")
        || t.starts_with("about:")
        // `file://` is passed through by design — this is a local dev cockpit,
        // so navigating to local files from the address bar is intentional.
        || t.starts_with("file://")
    {
        return t.to_string();
    }
    // A bare domain (a dot, no spaces) gets promoted to https; everything
    // else is treated as a search query.
    if !t.contains(' ') && t.contains('.') {
        return format!("https://{t}");
    }
    format!("{SEARCH_ENDPOINT}{}", percent_encode_query(t))
}

/// Minimal percent-encoding for the search-query path: enough to keep
/// spaces and the handful of chars that would break the query string safe.
fn percent_encode_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Tab-chip label for a URL before its `BrowserView` exists (the initial
/// open + restore paths): the host, or a generic fallback.
pub fn host_label(url: &str) -> String {
    host_of(url).unwrap_or_else(|| "Browser".to_string())
}

/// Host portion of a URL for the tab label (`https://a.b/c` → `a.b`).
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split(['/', '?', '#']).next()?;
    let host = host.split('@').next_back()?; // strip userinfo
    let host = host.split(':').next()?; // strip port
    // A real host has no whitespace — reject malformed input so the label
    // falls back to a generic name rather than echoing garbage.
    (!host.is_empty() && !host.contains(char::is_whitespace)).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_full_urls() {
        assert_eq!(normalize_address("https://a.dev/x"), "https://a.dev/x");
        assert_eq!(normalize_address("http://a.dev"), "http://a.dev");
        assert_eq!(normalize_address("about:blank"), "about:blank");
    }

    #[test]
    fn normalize_promotes_bare_domain() {
        assert_eq!(normalize_address("example.com"), "https://example.com");
        assert_eq!(normalize_address("  sub.example.com/p  "), "https://sub.example.com/p");
    }

    #[test]
    fn normalize_searches_non_urls() {
        assert_eq!(
            normalize_address("hello world"),
            format!("{SEARCH_ENDPOINT}hello%20world")
        );
        // A single bare word (no dot) is a search, not a domain.
        assert_eq!(
            normalize_address("rustlang"),
            format!("{SEARCH_ENDPOINT}rustlang")
        );
    }

    #[test]
    fn normalize_empty_is_default() {
        assert_eq!(normalize_address("   "), DEFAULT_URL);
    }

    #[test]
    fn percent_encode_covers_reserved() {
        assert_eq!(percent_encode_query("a b&c"), "a%20b%26c");
        assert_eq!(percent_encode_query("plain-Text_1.0~"), "plain-Text_1.0~");
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://a.b.dev/x?y#z").as_deref(), Some("a.b.dev"));
        assert_eq!(host_of("http://user@h.dev:8080/").as_deref(), Some("h.dev"));
        assert_eq!(host_label("https://x.dev/p"), "x.dev");
        assert_eq!(host_label("not a url"), "Browser");
    }
}
