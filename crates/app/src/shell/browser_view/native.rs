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

use super::agent_context::PickRect;

/// Owns the live `wry::WebView`. `!Send` — only ever touched on the GPUI
/// main thread (construction, frame updates, navigation, teardown-on-drop).
pub struct NativeWebview {
    webview: WebView,
}

/// Page-chrome callbacks wired into the webview at build time. Each fires
/// on the main run loop; implementations forward into the owning entity
/// (typically over a channel) — keep them cheap and non-reentrant.
pub struct WebviewCallbacks<N, T, L, I>
where
    N: Fn(String) -> bool + 'static,
    T: Fn(String) + 'static,
    L: Fn(PageLoadEvent, String) + 'static,
    I: Fn(String) + 'static,
{
    /// Top-level navigation starting (the new URL). Return `true` to allow.
    pub on_navigation: N,
    /// Document title changed.
    pub on_title: T,
    /// Page load started / finished (event + URL).
    pub on_load: L,
    /// Raw message body posted from the page via `window.ipc.postMessage`
    /// (the agent-context probes). Parsed by the owning entity.
    pub on_ipc: I,
}

impl NativeWebview {
    /// Build a webview as a child of `window`'s native surface, loading
    /// `url`. `init_script` runs at document-start on every navigation.
    ///
    /// `data_store_id` selects an isolated cookie/cache store (a browser
    /// profile); `None` uses the shared default store. The id is the profile
    /// UUID's bytes — distinct ids never see each other's cookies.
    pub fn build<N, T, L, I>(
        window: &Window,
        url: &str,
        init_script: &str,
        data_store_id: Option<[u8; 16]>,
        callbacks: WebviewCallbacks<N, T, L, I>,
    ) -> wry::Result<Self>
    where
        N: Fn(String) -> bool + 'static,
        T: Fn(String) + 'static,
        L: Fn(PageLoadEvent, String) + 'static,
        I: Fn(String) + 'static,
    {
        let on_ipc = callbacks.on_ipc;
        let mut builder = WebViewBuilder::new()
            .with_url(url)
            .with_initialization_script(init_script)
            // Start hidden — the owning view's first render sweep shows it only
            // if its tab is active+uncovered. Avoids a 1-frame flash of every
            // restored browser tab's webview before the sweep corrects them.
            .with_visible(false)
            // Don't grab first responder on creation — keyboard stays with
            // the GPUI views until the user clicks into the page.
            .with_focused(false)
            // Inspector available on demand (toggled from the toolbar wrench).
            .with_devtools(true)
            .with_back_forward_navigation_gestures(true)
            .with_navigation_handler(callbacks.on_navigation)
            .with_document_title_changed_handler(callbacks.on_title)
            .with_on_page_load_handler(callbacks.on_load)
            .with_ipc_handler(move |req: wry::http::Request<String>| {
                on_ipc(req.into_body());
            });
        // Per-profile cookie/cache isolation is a macOS WKWebsiteDataStore
        // feature; off-platform the id is ignored (single shared store).
        #[cfg(target_os = "macos")]
        if let Some(id) = data_store_id {
            use wry::WebViewBuilderExtDarwin;
            builder = builder.with_data_store_identifier(id);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = data_store_id;
        let webview = builder.build_as_child(window)?;
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

    /// Run an arbitrary script (agent-context probes: snapshot / console /
    /// picker injection). Results come back over the IPC handler, not here.
    pub fn eval_script(&self, js: &str) {
        self.eval(js);
    }

    /// Give the webview keyboard focus — the element picker reads keystrokes
    /// (`C` / `S` / `Esc`) in the page, so it needs first responder after the
    /// crosshair button (a GPUI element) was clicked.
    pub fn focus(&self) {
        let _ = self.webview.focus();
    }

    /// Hand keyboard first-responder back to the GPUI render surface (the
    /// webview's parent view). The webview is a native sibling NSView: once it
    /// takes first-responder (a click into the page, or our own `focus()` for
    /// the picker) it KEEPS it even when hidden — `isHidden` doesn't resign
    /// first-responder — which silently swallows every keystroke app-wide.
    /// Call this whenever the webview should stop owning the keyboard: when its
    /// tab is hidden, when the picker ends, and when the address bar is focused.
    pub fn focus_parent(&self) {
        let _ = self.webview.focus_parent();
    }

    /// Open / close / query the WebKit inspector for this page.
    pub fn open_devtools(&self) {
        self.webview.open_devtools();
    }

    pub fn close_devtools(&self) {
        self.webview.close_devtools();
    }

    pub fn is_devtools_open(&self) -> bool {
        self.webview.is_devtools_open()
    }

    /// Override the page color-scheme (drives `prefers-color-scheme`) by
    /// setting the webview's native appearance. `System` clears the override.
    /// macOS-only; a no-op elsewhere (wry exposes no portable theme setter).
    #[cfg(target_os = "macos")]
    pub fn set_appearance(&self, appearance: super::PageAppearance) {
        use objc2_app_kit::{
            NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        };
        use wry::WebViewExtMacOS;

        let named = match appearance {
            super::PageAppearance::System => None,
            super::PageAppearance::Light => unsafe { NSAppearance::appearanceNamed(NSAppearanceNameAqua) },
            super::PageAppearance::Dark => unsafe {
                NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua)
            },
        };
        let webview = self.webview.webview();
        webview.setAppearance(named.as_deref());
    }

    #[cfg(not(target_os = "macos"))]
    pub fn set_appearance(&self, _appearance: super::PageAppearance) {}

    /// Capture the webview to PNG bytes and deliver them to `on_png` (fired on
    /// the main thread once the async snapshot completes). `rect` snapshots a
    /// sub-region in view coordinates; `None` captures the visible viewport.
    #[cfg(target_os = "macos")]
    pub fn screenshot(&self, rect: Option<PickRect>, on_png: impl Fn(Vec<u8>) + 'static) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use block2::RcBlock;
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
        use objc2_core_foundation::{CGPoint, CGRect, CGSize};
        use objc2_foundation::{NSDictionary, NSError};
        use objc2_web_kit::WKSnapshotConfiguration;
        use wry::WebViewExtMacOS;

        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let webview = self.webview.webview();

        let config = rect.map(|r| {
            let c = unsafe { WKSnapshotConfiguration::new(mtm) };
            unsafe {
                c.setRect(CGRect::new(
                    CGPoint::new(r.x, r.y),
                    CGSize::new(r.w, r.h),
                ));
            }
            c
        });

        // The block bound is `Fn` (ObjC blocks may be retained); a once-guard
        // keeps a stray double-invocation from writing the clipboard twice.
        let fired = Arc::new(AtomicBool::new(false));
        let handler = RcBlock::new(move |image: *mut NSImage, _err: *mut NSError| {
            if fired.swap(true, Ordering::AcqRel) {
                return;
            }
            if image.is_null() {
                return;
            }
            let image: &NSImage = unsafe { &*image };
            let Some(png) = png_bytes(image) else {
                return;
            };
            on_png(png);
        });

        unsafe {
            webview
                .takeSnapshotWithConfiguration_completionHandler(config.as_deref(), &handler);
        }

        /// NSImage → PNG via a bitmap re-encode (TIFF is NSImage's lossless
        /// interchange rep; `NSBitmapImageRep` re-encodes it to PNG).
        fn png_bytes(image: &NSImage) -> Option<Vec<u8>> {
            let tiff = image.TIFFRepresentation()?;
            let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
            let props = NSDictionary::new();
            let png = unsafe {
                rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props)
            }?;
            Some(png.to_vec())
        }
    }

    /// No native snapshot off macOS — agent-context screenshot is the one
    /// platform-specific probe; the rest ride the cross-platform JS bridge.
    #[cfg(not(target_os = "macos"))]
    pub fn screenshot(&self, _rect: Option<PickRect>, _on_png: impl Fn(Vec<u8>) + 'static) {}
}
