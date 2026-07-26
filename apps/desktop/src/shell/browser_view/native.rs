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

/// User-Agent for the inline browser. WKWebView's *default* UA omits the
/// trailing `Version/<n> Safari/<build>` token, and major sites (Google,
/// among others) read that absence as an unknown/legacy browser and fall back
/// to a stripped-down page that also ignores `prefers-color-scheme`. We send a
/// current desktop **Safari** UA — honest to the underlying WebKit engine (a
/// Chrome UA would invite Blink-only code paths and client-hint mismatches the
/// engine can't satisfy) — so sites serve their modern layout and respect the
/// page's light/dark preference. The `Intel Mac OS X 10_15_7` platform token
/// is what real Safari reports on every Mac (including Apple Silicon), by
/// design. Bump the `Version/` number as Safari advances.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.3 Safari/605.1.15";

/// Owns the live `wry::WebView`. `!Send` — only ever touched on the GPUI
/// main thread (construction, frame updates, navigation, teardown-on-drop).
pub struct NativeWebview {
    webview: WebView,
    /// macOS: a pane-sized container the webview is re-parented into so the
    /// WebKit inspector can dock *inside the pane* (split page/inspector) rather
    /// than spilling over the whole app. `None` if the wrap could not be set up
    /// (then DevTools falls back to a standalone inspector window).
    #[cfg(target_os = "macos")]
    inspector_dock: Option<InspectorDock>,
}

/// The two retained AppKit views backing an in-pane docked inspector.
///
/// `container` is pinned to the pane's body bounds each paint and holds the
/// webview as its sole child; when the inspector attaches, WebKit splits this
/// container between page and inspector. `host` is the container's superview
/// (GPUI's window root) — kept for the bounds Y-flip, matching how `wry`
/// positions a child webview against its parent's height.
#[cfg(target_os = "macos")]
struct InspectorDock {
    host: objc2::rc::Retained<objc2_app_kit::NSView>,
    container: objc2::rc::Retained<objc2_app_kit::NSView>,
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
            .with_user_agent(BROWSER_USER_AGENT)
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
        #[cfg(target_os = "macos")]
        let inspector_dock = wrap_for_docked_inspector(&webview);
        Ok(Self {
            webview,
            #[cfg(target_os = "macos")]
            inspector_dock,
        })
    }

    /// Pin the webview frame to a GPUI element's window-relative bounds.
    ///
    /// When a docked-inspector container exists we size *that* (the webview
    /// fills it via autoresizing, or WebKit splits it with the inspector); the
    /// container's superview is the GPUI window root, so we replicate `wry`'s
    /// own top-left→AppKit Y-flip against it. Otherwise we defer to `wry`.
    pub fn set_bounds_px(&self, bounds: Bounds<Pixels>) {
        #[cfg(target_os = "macos")]
        if let Some(dock) = &self.inspector_dock {
            use objc2_core_foundation::{CGPoint, CGRect, CGSize};
            let x = f32::from(bounds.origin.x) as f64;
            let y = f32::from(bounds.origin.y) as f64;
            let w = f32::from(bounds.size.width) as f64;
            let h = f32::from(bounds.size.height) as f64;
            let origin_y = if dock.host.isFlipped() {
                y
            } else {
                dock.host.frame().size.height - y - h
            };
            dock.container
                .setFrame(CGRect::new(CGPoint::new(x, origin_y), CGSize::new(w, h)));
            return;
        }
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
        // Toggle the container (not the webview) so a docked inspector — a
        // sibling of the webview inside the container — hides with the page
        // when the tab is inactive or covered.
        #[cfg(target_os = "macos")]
        if let Some(dock) = &self.inspector_dock {
            dock.container.setHidden(!visible);
            return;
        }
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

    /// Halt the in-flight navigation/resource loads (the toolbar's stop button,
    /// shown in place of reload while a page is loading).
    pub fn stop(&self) {
        self.eval("window.stop()");
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

    /// Pop a native toolbar dropdown, anchored under `anchor_bounds` (a button's
    /// window-relative bounds) when given, else at the mouse. Returns the chosen
    /// row index. Blocks on the menu's modal loop — call it off the GPUI `App`
    /// borrow (a pumped task would otherwise re-enter and double-borrow).
    ///
    /// The window root the webview was parented into is the same view `wry`
    /// positions the child webview against, so we reuse its top-left→AppKit
    /// Y-flip to map the button's bounds into that view's space for the anchor.
    #[cfg(target_os = "macos")]
    pub fn popup_menu(
        &self,
        anchor_bounds: Option<Bounds<Pixels>>,
        header: Option<&str>,
        items: &[super::native_menu::MenuItem],
    ) -> Option<usize> {
        super::native_menu::popup(self.menu_anchor(anchor_bounds), header, items)
    }

    /// Pop a nested native dropdown (submenus) and return the chosen leaf's
    /// caller id. Same anchoring as [`Self::popup_menu`]; drives the profile
    /// menu's cascading "Import Cookies" tree.
    #[cfg(target_os = "macos")]
    pub fn popup_menu_tree(
        &self,
        anchor_bounds: Option<Bounds<Pixels>>,
        header: Option<&str>,
        entries: &[super::native_menu::MenuEntry],
    ) -> Option<u32> {
        super::native_menu::popup_tree(self.menu_anchor(anchor_bounds), header, entries)
    }

    /// Map a button's window-relative bounds into the AppKit view `wry`
    /// positions the child webview against, producing a drop-down anchor.
    /// `None` when there are no bounds (open at the mouse) or no host view.
    #[cfg(target_os = "macos")]
    fn menu_anchor(
        &self,
        anchor_bounds: Option<Bounds<Pixels>>,
    ) -> Option<super::native_menu::Anchor> {
        use objc2::rc::Retained;
        use objc2_app_kit::NSView;
        use objc2_core_foundation::{CGPoint, CGRect, CGSize};
        use wry::WebViewExtMacOS;

        let host: Option<Retained<NSView>> = self
            .inspector_dock
            .as_ref()
            .map(|d| d.host.clone())
            .or_else(|| unsafe { self.webview.webview().superview() });

        anchor_bounds.zip(host).map(|(b, host)| {
            let bx = f32::from(b.origin.x) as f64;
            let by = f32::from(b.origin.y) as f64;
            let bw = f32::from(b.size.width) as f64;
            let bh = f32::from(b.size.height) as f64;
            let flipped = host.isFlipped();
            let view_y = if flipped {
                by
            } else {
                host.frame().size.height - by - bh
            };
            super::native_menu::Anchor {
                view: host,
                rect: CGRect::new(CGPoint::new(bx, view_y), CGSize::new(bw, bh)),
                flipped,
            }
        })
    }

    /// Inject imported cookies into this webview's active-profile cookie store.
    /// The data store is the one `wry` bound to this profile at build time, so
    /// the cookies land in the right isolated store and survive reloads. WebKit
    /// cookie APIs are main-thread; callers invoke this on the main thread.
    /// Google integrity cookies (bound to the source browser's TLS fingerprint)
    /// are dropped — they'd reject the transplanted session. Returns the number
    /// of cookies submitted. Values are never logged.
    #[cfg(target_os = "macos")]
    pub fn import_cookies(&self, cookies: &[super::cookie_import::ParsedCookie]) -> usize {
        use objc2::runtime::{AnyObject, ProtocolObject};
        use objc2_foundation::{
            NSDate, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieExpires, NSHTTPCookieName,
            NSHTTPCookiePath, NSHTTPCookiePropertyKey, NSHTTPCookieSameSiteLax,
            NSHTTPCookieSameSitePolicy, NSHTTPCookieSameSiteStrict, NSHTTPCookieSecure,
            NSHTTPCookieValue, NSMutableDictionary, NSString,
        };
        use wry::WebViewExtMacOS;

        use super::cookie_import::{SameSite, inject};

        let webview = self.webview.webview();
        let store = unsafe { webview.configuration().websiteDataStore().httpCookieStore() };

        let mut count = 0usize;
        for c in cookies {
            if !inject::should_import(&c.host, &c.name) {
                continue;
            }
            let dict = NSMutableDictionary::<NSHTTPCookiePropertyKey, AnyObject>::new();
            let domain = NSString::from_str(&c.host);
            let name = NSString::from_str(&c.name);
            let value = NSString::from_str(&c.value);
            let path = NSString::from_str(if c.path.is_empty() { "/" } else { &c.path });
            unsafe {
                dict.setObject_forKey(&domain, ProtocolObject::from_ref(NSHTTPCookieDomain));
                dict.setObject_forKey(&name, ProtocolObject::from_ref(NSHTTPCookieName));
                dict.setObject_forKey(&value, ProtocolObject::from_ref(NSHTTPCookieValue));
                dict.setObject_forKey(&path, ProtocolObject::from_ref(NSHTTPCookiePath));
                if c.secure {
                    let yes = NSString::from_str("TRUE");
                    dict.setObject_forKey(&yes, ProtocolObject::from_ref(NSHTTPCookieSecure));
                }
                match c.same_site {
                    SameSite::Lax => dict
                        .setObject_forKey(NSHTTPCookieSameSiteLax, ProtocolObject::from_ref(NSHTTPCookieSameSitePolicy)),
                    SameSite::Strict => dict
                        .setObject_forKey(NSHTTPCookieSameSiteStrict, ProtocolObject::from_ref(NSHTTPCookieSameSitePolicy)),
                    SameSite::None | SameSite::Unspecified => {}
                }
                if let Some(secs) = c.expires_unix_s {
                    let when = NSDate::dateWithTimeIntervalSince1970(secs as f64);
                    dict.setObject_forKey(&when, ProtocolObject::from_ref(NSHTTPCookieExpires));
                }
                if let Some(cookie) = NSHTTPCookie::cookieWithProperties(&dict) {
                    store.setCookie_completionHandler(&cookie, None);
                    count += 1;
                }
            }
        }
        count
    }

    /// Open the WebKit inspector docked *inside the browser pane* (page on top,
    /// inspector below) — the layout a normal browser shows.
    ///
    /// WebKit's attached inspector docks into the inspected webview's superview
    /// and splits it. We re-parented the webview into a pane-sized container at
    /// build time (`wrap_for_docked_inspector`), so the split stays within the
    /// pane. We steer the initial presentation to *attached* (the opposite of a
    /// standalone window) and force-attach in case the page's inspector was
    /// previously materialized detached.
    ///
    /// If the wrap could not be set up (`inspector_dock == None`), an attached
    /// inspector would spill over the whole app, so we keep the old behavior:
    /// open it as a standalone floating window instead.
    #[cfg(target_os = "macos")]
    pub fn open_devtools(&self) {
        use objc2::rc::Retained;
        use objc2::runtime::AnyObject;
        use objc2_foundation::{NSUserDefaults, ns_string};
        use wry::WebViewExtMacOS;

        let docked = self.inspector_dock.is_some();
        // WebKit reads this default when it first materializes an inspector for
        // a page group: `true` docks it attached to the page (inside our
        // container), `false` opens a standalone window. Steer it per `docked`.
        let defaults = NSUserDefaults::standardUserDefaults();
        defaults.setBool_forKey(
            docked,
            ns_string!("__WebInspectorPageGroupLevel1__.WebKit2InspectorStartsAttached"),
        );
        let webview = self.webview.webview();
        unsafe {
            let inspector: Retained<AnyObject> = objc2::msg_send![&*webview, _inspector];
            let _: () = objc2::msg_send![&inspector, show];
            // Re-attach if a prior `show` had materialized it detached. WebKit's
            // bias toward re-docking (the reason detaching is unreliable) works
            // in our favor here.
            if docked {
                let _: () = objc2::msg_send![&inspector, attach];
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
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

/// Re-parent the freshly built child webview into a pane-sized container so the
/// WebKit inspector can dock inside the pane (see [`NativeWebview::open_devtools`]).
///
/// `wry` parents a child webview directly into the GPUI window root. WebKit's
/// attached inspector splits the inspected view's *superview*, so left as-is it
/// would overrun the whole app. Inserting an intermediate container — sized to
/// the pane each paint — confines that split. The webview fills the container
/// via autoresizing until the inspector claims its share. Returns `None` if any
/// AppKit handle is unavailable, leaving DevTools to fall back to a window.
#[cfg(target_os = "macos")]
fn wrap_for_docked_inspector(webview: &WebView) -> Option<InspectorDock> {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};
    use wry::WebViewExtMacOS;

    let mtm = MainThreadMarker::new()?;
    let wk = webview.webview(); // Retained<WKWebView> — an NSView subclass
    // SAFETY: all on the GPUI main thread; the webview is a live child view.
    unsafe {
        let host = wk.superview()?; // GPUI window root the webview was parented into
        let frame = wk.frame();

        let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
        // Slot the container in where the webview sat, then move the webview in.
        host.addSubview(&container);
        container.addSubview(&wk);
        wk.setFrame(container.bounds());
        wk.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        // Build starts the page hidden via `with_visible(false)` (the webview's
        // own `setHidden`). Move that gate to the container so the page + a
        // docked inspector hide together; the first render sweep reveals the
        // active tab.
        wk.setHidden(false);
        container.setHidden(true);

        Some(InspectorDock { host, container })
    }
}
