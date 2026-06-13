//! Thin wrapper over a `wry::WebView` attached as a child of the GPUI
//! window's native render surface.
//!
//! The webview is a sibling native view layered ABOVE the window's GPU
//! canvas — it does not clip to the GPUI element tree, so the owning
//! `BrowserView` is responsible for pinning its frame each paint (from a
//! laid-out anchor) and hiding it when its tab is inactive or covered.
//!
//! Coordinates: `wry` takes top-left logical points, which is the same
//! space GPUI lays out in (`Pixels` == logical points). So a GPUI
//! element's window-relative bounds map straight onto the webview frame —
//! no scale-factor or Y-flip math (wry applies both internally).

use gpui::{Bounds, Pixels, Window};
use wry::{
    PageLoadEvent, Rect, WebView, WebViewBuilder,
    dpi::{LogicalPosition, LogicalSize},
};

/// Owns the live `wry::WebView`. `!Send` — only ever touched on the GPUI
/// main thread (construction, frame updates, navigation, teardown-on-drop).
pub struct NativeWebview {
    webview: WebView,
}

/// Page-chrome callbacks wired into the webview at build time. Each fires
/// on the main run loop; implementations forward into the owning entity
/// (typically over a channel) — keep them cheap and non-reentrant.
pub struct WebviewCallbacks<N, T, L>
where
    N: Fn(String) -> bool + 'static,
    T: Fn(String) + 'static,
    L: Fn(PageLoadEvent, String) + 'static,
{
    /// Top-level navigation starting (the new URL). Return `true` to allow.
    pub on_navigation: N,
    /// Document title changed.
    pub on_title: T,
    /// Page load started / finished (event + URL).
    pub on_load: L,
}

impl NativeWebview {
    /// Build a webview as a child of `window`'s native surface, loading
    /// `url`. `init_script` runs at document-start on every navigation.
    pub fn build<N, T, L>(
        window: &Window,
        url: &str,
        init_script: &str,
        callbacks: WebviewCallbacks<N, T, L>,
    ) -> wry::Result<Self>
    where
        N: Fn(String) -> bool + 'static,
        T: Fn(String) + 'static,
        L: Fn(PageLoadEvent, String) + 'static,
    {
        let webview = WebViewBuilder::new()
            .with_url(url)
            .with_initialization_script(init_script)
            // Start hidden — the owning view's first render sweep shows it only
            // if its tab is active+uncovered. Avoids a 1-frame flash of every
            // restored browser tab's webview before the sweep corrects them.
            .with_visible(false)
            // Don't grab first responder on creation — keyboard stays with
            // the GPUI views until the user clicks into the page.
            .with_focused(false)
            .with_back_forward_navigation_gestures(true)
            .with_navigation_handler(callbacks.on_navigation)
            .with_document_title_changed_handler(callbacks.on_title)
            .with_on_page_load_handler(callbacks.on_load)
            .build_as_child(window)?;
        Ok(Self { webview })
    }

    /// Pin the webview frame to a GPUI element's window-relative bounds.
    pub fn set_bounds_px(&self, bounds: Bounds<Pixels>) {
        let _ = self.webview.set_bounds(Rect {
            position: LogicalPosition::new(
                f32::from(bounds.origin.x) as f64,
                f32::from(bounds.origin.y) as f64,
            )
            .into(),
            size: LogicalSize::new(
                f32::from(bounds.size.width) as f64,
                f32::from(bounds.size.height) as f64,
            )
            .into(),
        });
    }

    pub fn set_visible(&self, visible: bool) {
        let _ = self.webview.set_visible(visible);
    }

    pub fn load_url(&self, url: &str) {
        let _ = self.webview.load_url(url);
    }

    fn eval(&self, js: &str) {
        let _ = self.webview.evaluate_script(js);
    }

    /// History/reload via injected JS — `wry` exposes no native back/forward
    /// methods, and `history.*` covers the SPA + multi-page cases alike.
    pub fn go_back(&self) {
        self.eval("history.back()");
    }

    pub fn go_forward(&self) {
        self.eval("history.forward()");
    }

    pub fn reload(&self) {
        self.eval("location.reload()");
    }
}
