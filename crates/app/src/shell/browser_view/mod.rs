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

mod agent_context;
#[cfg(target_os = "macos")]
mod cookie_import;
mod native;
#[cfg(target_os = "macos")]
mod native_menu;
mod render;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    App, AppContext, BorrowAppContext, Bounds, ClipboardItem, Context, FocusHandle, Focusable,
    Image, ImageFormat, Pixels, SharedString, Task, Window,
};
use gpui_component::input::InputState;
use oximux_settings::{Density, Theme, Typography};
use wry::PageLoadEvent;

use agent_context::{IpcMessage, PickRect};
use native::{NativeWebview, WebviewCallbacks};

use crate::browser_profiles::{BrowserProfile, BrowserProfiles};

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
    /// Raw `window.ipc.postMessage` body from an agent-context probe.
    Ipc(String),
    /// PNG bytes from an async screenshot, bound for the clipboard.
    Screenshot(Vec<u8>),
}

/// Which agent-context probe most recently produced a result — drives the
/// transient confirmation (a green check on the firing toolbar button plus a
/// short "copied" toast floated over the page), so a copy is no longer silent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyKind {
    Screenshot,
    Dom,
    Console,
    /// Picked element copied to the clipboard.
    Pick,
    /// Picked element piped into the active agent terminal.
    PickToAgent,
}

impl CopyKind {
    /// Text for the confirmation toast.
    fn pill_label(self) -> &'static str {
        match self {
            CopyKind::Screenshot => "Screenshot copied",
            CopyKind::Dom => "DOM copied",
            CopyKind::Console => "Console copied",
            CopyKind::Pick => "Element copied",
            CopyKind::PickToAgent => "Sent to agent",
        }
    }

    /// The toolbar button this result lights up (so the check lands on the
    /// control the user pressed). Pick-to-agent shares the crosshair.
    fn lights_button(self, other: CopyKind) -> bool {
        self == other
            || matches!(
                (self, other),
                (CopyKind::Pick, CopyKind::PickToAgent)
                    | (CopyKind::PickToAgent, CopyKind::Pick)
            )
    }
}

/// Page color-scheme override applied to the embedded webview (drives the
/// page's `prefers-color-scheme`); independent of the app chrome's theme.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum PageAppearance {
    /// Follow the system/app appearance (no override).
    #[default]
    System,
    Light,
    Dark,
}

impl PageAppearance {
    /// Lowercase slug shared with the in-page theme menu (marks the active row).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn slug(self) -> &'static str {
        match self {
            PageAppearance::System => "system",
            PageAppearance::Light => "light",
            PageAppearance::Dark => "dark",
        }
    }

    /// Display label for the native page-theme menu rows.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn menu_label(self) -> &'static str {
        match self {
            PageAppearance::System => "System",
            PageAppearance::Light => "Light",
            PageAppearance::Dark => "Dark",
        }
    }
}

impl From<agent_context::AppearanceValue> for PageAppearance {
    fn from(v: agent_context::AppearanceValue) -> Self {
        match v {
            agent_context::AppearanceValue::System => PageAppearance::System,
            agent_context::AppearanceValue::Light => PageAppearance::Light,
            agent_context::AppearanceValue::Dark => PageAppearance::Dark,
        }
    }
}

/// A profiles-menu choice deferred to the next render. Switching or creating a
/// profile rebuilds the webview, which needs a `&Window`; the IPC callback has
/// none, so the choice is stashed and applied in `render` (mirrors
/// `pending_agent_text`).
enum ProfileRequest {
    /// Switch to this store (`None` = the shared default).
    Switch(Option<uuid::Uuid>),
    /// Create a fresh isolated profile and switch to it.
    New,
}

/// A profiles-menu pick, indexed by the cascading menu's leaf id. Profile
/// switches defer to the next render (they rebuild the webview); cookie imports
/// run inline (no `&Window` needed — they only write the live data store).
#[cfg(target_os = "macos")]
enum MenuAction {
    Profile(ProfileRequest),
    /// Import cookies from `(browser, source profile)` into the active profile.
    ImportSource { browser_id: &'static str, profile_dir: String },
    /// Import cookies from a browser-extension JSON export.
    ImportFile,
}

/// Append `action` to the routing table and return its leaf id (the table
/// index). Keeps the menu tree and its action list in lockstep.
#[cfg(target_os = "macos")]
fn push_action(actions: &mut Vec<MenuAction>, action: MenuAction) -> u32 {
    let id = actions.len() as u32;
    actions.push(action);
    id
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
    /// Tracks the address bar's focus state across renders so we only reclaim
    /// keyboard first-responder from the webview on the rising edge (the click
    /// into the field), not every frame.
    address_focused: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Sender into the entity event loop — agent-context probes route async
    /// results (screenshots) back through here onto the main thread.
    events_tx: UnboundedSender<ChromeEvent>,
    _events: Task<()>,
    /// The most recent probe result; lights the firing toolbar button for a
    /// short window (the textual confirmation floats in-page). Cleared by
    /// `_confirm` after a short delay.
    confirmed: Option<CopyKind>,
    /// Holds the timer that clears `confirmed`; a fresh result replaces it so
    /// rapid clicks restart the window instead of stacking.
    _confirm: Option<Task<()>>,
    /// Page color-scheme override; applied to the webview and re-applied after
    /// a profile-driven rebuild.
    appearance: PageAppearance,
    /// `true` while the WebKit inspector is open for this tab.
    devtools_open: bool,
    /// Picked-element text awaiting delivery to the active agent terminal.
    /// Stashed from the IPC callback (no `Window` there) and dispatched on the
    /// next render via `SendTextToActiveAgent`.
    pending_agent_text: Option<String>,
    /// Active browser profile (cookie/cache store). `None` = the shared default
    /// store. Persisted per tab; switching rebuilds the webview.
    profile_id: Option<uuid::Uuid>,
    /// A profiles-menu choice awaiting the next render (where a `Window` exists
    /// to rebuild the webview).
    pending_profile: Option<ProfileRequest>,
    /// Window-relative bounds of the appearance + profile toolbar buttons,
    /// recorded each paint by a measuring canvas (see `render`). The native
    /// dropdowns anchor under these so they drop straight from the button (a
    /// `Cell` so the paint closure can write without borrowing the entity).
    appearance_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    profile_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl BrowserView {
    pub fn new(
        url: impl Into<String>,
        profile_id: Option<uuid::Uuid>,
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
        let events_tx = tx.clone();
        let native = build_native(window, &url, profile_id.map(|u| *u.as_bytes()), tx).map(Rc::new);
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
            address_focused: false,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            events_tx,
            _events: events,
            confirmed: None,
            _confirm: None,
            appearance: PageAppearance::default(),
            devtools_open: false,
            pending_agent_text: None,
            profile_id,
            pending_profile: None,
            appearance_anchor: Rc::new(Cell::new(None)),
            profile_anchor: Rc::new(Cell::new(None)),
        }
    }

    /// Flash a transient confirmation for a probe result: light the firing
    /// toolbar button and float a "copied" toast over the page for a short
    /// window. A new result restarts the window.
    fn flash_confirm(&mut self, kind: CopyKind, cx: &mut Context<Self>) {
        self.confirmed = Some(kind);
        // The textual confirmation floats in the page (top-right), not the
        // toolbar — an inline pill in the toolbar's flex row shoved every icon
        // left as it appeared. Picker results already draw their own
        // near-element chip, so they skip the page toast and only light the
        // toolbar button.
        if let Some(native) = &self.native
            && !matches!(kind, CopyKind::Pick | CopyKind::PickToAgent)
        {
            native.eval_script(&agent_context::confirm_toast_js(kind.pill_label()));
        }
        cx.notify();
        self._confirm = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |view, cx| {
                view.confirmed = None;
                cx.notify();
            });
        }));
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
            ChromeEvent::Ipc(body) => {
                self.on_ipc(&body, cx);
                return;
            }
            ChromeEvent::Screenshot(png) => {
                let image = Image::from_bytes(ImageFormat::Png, png);
                cx.write_to_clipboard(ClipboardItem::new_image(&image));
                // Flash the page white once the capture has landed (so the
                // flash is never in the shot) and confirm on the toolbar.
                if let Some(native) = &self.native {
                    native.eval_script(agent_context::SCREENSHOT_FLASH_JS);
                }
                self.flash_confirm(CopyKind::Screenshot, cx);
                return;
            }
        }
        cx.notify();
    }

    /// Dispatch a probe payload posted from the page: format it to the
    /// clipboard, or (for a picked rect) kick off the element screenshot.
    /// Foreign IPC bodies are ignored.
    fn on_ipc(&mut self, body: &str, cx: &mut Context<Self>) {
        let Some(msg) = IpcMessage::parse(body) else {
            return;
        };
        match msg {
            IpcMessage::Console { items, errors } => {
                self.to_clipboard(agent_context::format_console(&items, &errors), cx);
                self.flash_confirm(CopyKind::Console, cx);
            }
            IpcMessage::Dom(snap) => {
                self.to_clipboard(agent_context::format_dom_snapshot(&snap), cx);
                self.flash_confirm(CopyKind::Dom, cx);
            }
            // The picker focuses the webview to read its keys; once it ends
            // (copy / shot / agent / cancel) hand the keyboard back so the next
            // address bar or pane keystroke doesn't leak into the page.
            IpcMessage::Pick(payload) => {
                self.to_clipboard(agent_context::format_pick_part(&payload), cx);
                self.flash_confirm(CopyKind::Pick, cx);
                self.release_webview_focus();
            }
            IpcMessage::PickToAgent(payload) => {
                self.pending_agent_text = Some(agent_context::format_pick(&payload));
                self.flash_confirm(CopyKind::PickToAgent, cx);
                self.release_webview_focus();
            }
            IpcMessage::PickShot { rect } => {
                self.capture_screenshot(Some(rect));
                self.release_webview_focus();
            }
            IpcMessage::PickCancel => self.release_webview_focus(),
            IpcMessage::SetAppearance { value } => self.set_appearance(value.into(), cx),
            // Profile changes rebuild the webview (needs a `Window`); defer to
            // the next render. `"default"` = the shared store; otherwise a UUID.
            IpcMessage::SwitchProfile { id } => {
                let target = if id == "default" {
                    Some(None)
                } else {
                    uuid::Uuid::parse_str(&id).ok().map(Some)
                };
                if let Some(target) = target {
                    self.pending_profile = Some(ProfileRequest::Switch(target));
                    cx.notify();
                }
            }
            IpcMessage::NewProfile => {
                self.pending_profile = Some(ProfileRequest::New);
                cx.notify();
            }
        }
    }

    /// Hand keyboard first-responder from the webview back to the GPUI surface.
    fn release_webview_focus(&self) {
        if let Some(native) = &self.native {
            native.focus_parent();
        }
    }

    fn to_clipboard(&self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Copy the captured console + error buffers to the clipboard.
    pub fn copy_console(&self) {
        if let Some(n) = &self.native {
            n.eval_script(agent_context::READ_CONSOLE_JS);
        }
    }

    /// Copy a compact DOM snapshot (interactive + landmark elements) to the
    /// clipboard.
    pub fn copy_dom_snapshot(&self) {
        if let Some(n) = &self.native {
            n.eval_script(agent_context::SNAPSHOT_JS);
        }
    }

    /// Start the in-page element picker. Focus the webview first so it reads
    /// the picker's `C` / `S` / `Esc` keys (the crosshair button that invoked
    /// this is a GPUI element, which otherwise keeps first responder).
    pub fn start_element_picker(&self) {
        if let Some(n) = &self.native {
            n.focus();
            n.eval_script(agent_context::PICKER_JS);
        }
    }

    /// Capture the webview (or a sub-`rect`) to a PNG on the clipboard. The
    /// snapshot is async; the result returns over the event channel.
    pub fn capture_screenshot(&self, rect: Option<PickRect>) {
        let Some(native) = &self.native else {
            return;
        };
        let tx = self.events_tx.clone();
        native.screenshot(rect, move |png| {
            let _ = tx.unbounded_send(ChromeEvent::Screenshot(png));
        });
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
            // A hidden webview must not keep keyboard first-responder, or it
            // swallows input for whatever the user switches to (another tab, a
            // terminal). Hand the keyboard back to the GPUI surface on hide.
            if !active {
                native.focus_parent();
            }
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

    fn stop_loading(&mut self, cx: &mut Context<Self>) {
        if let Some(n) = &self.native {
            n.stop();
        }
        // `window.stop()` doesn't fire a page-load-finished callback on
        // WKWebView, so clear the loading flag here or the toolbar would stay
        // stuck on the stop icon.
        self.loading = false;
        cx.notify();
    }

    /// Live URL — persistence reads this so a restored tab reopens where the
    /// user left off (including link-click navigations).
    pub fn current_url(&self) -> String {
        self.url.clone()
    }

    /// Active profile (cookie/cache store) for persistence; `None` = default.
    pub fn profile_id(&self) -> Option<uuid::Uuid> {
        self.profile_id
    }

    /// Toggle the WebKit inspector for this tab.
    pub fn toggle_devtools(&mut self, cx: &mut Context<Self>) {
        let Some(n) = &self.native else { return };
        if n.is_devtools_open() {
            n.close_devtools();
            self.devtools_open = false;
        } else {
            n.open_devtools();
            self.devtools_open = true;
        }
        cx.notify();
    }

    pub fn appearance(&self) -> PageAppearance {
        self.appearance
    }

    /// Open the page-theme menu (System / Light / Dark, active ✓) and apply the
    /// chosen scheme.
    ///
    /// macOS pops a native `NSMenu` over the webview (it composites above the
    /// native view; a GPUI menu would draw under it). The pop-up runs modally —
    /// a nested run loop that pumps the app's main-thread tasks — so it must
    /// open *outside* the current `App` borrow this click handler holds, or a
    /// pumped task re-entering `App::update` double-borrows and aborts. Deferring
    /// onto a foreground task lets the handler return (releasing the borrow)
    /// first; the choice is applied once the menu closes.
    #[cfg(target_os = "macos")]
    pub fn open_appearance_menu(&mut self, cx: &mut Context<Self>) {
        let Some(native) = self.native.clone() else {
            return;
        };
        let anchor = self.appearance_anchor.get();
        const ORDER: [PageAppearance; 3] =
            [PageAppearance::System, PageAppearance::Light, PageAppearance::Dark];
        let items: Vec<native_menu::MenuItem> = ORDER
            .iter()
            .map(|a| native_menu::MenuItem::new(a.menu_label(), *a == self.appearance))
            .collect();
        cx.spawn(async move |this, cx| {
            let Some(idx) = native.popup_menu(anchor, Some("Page theme"), &items) else {
                return;
            };
            let _ = this.update(cx, |view, cx| view.set_appearance(ORDER[idx], cx));
        })
        .detach();
    }

    /// Off macOS the native view has no AppKit menu to pop over it, so the
    /// theme picker is injected into the page (its own shadow root) and routes
    /// the choice back via `set_appearance`.
    #[cfg(not(target_os = "macos"))]
    pub fn open_appearance_menu(&mut self, _cx: &mut Context<Self>) {
        if let Some(n) = &self.native {
            n.eval_script(&agent_context::appearance_menu_js(self.appearance.slug()));
        }
    }

    /// Apply a page color-scheme override (from the in-page theme menu).
    fn set_appearance(&mut self, next: PageAppearance, cx: &mut Context<Self>) {
        self.appearance = next;
        if let Some(n) = &self.native {
            n.set_appearance(next);
        }
        cx.notify();
    }

    /// Display name of the active profile (for the toolbar tooltip).
    pub fn profile_name(&self, cx: &App) -> String {
        cx.try_global::<BrowserProfiles>()
            .map(|g| g.name_of(self.profile_id))
            .unwrap_or_else(|| "Default".to_string())
    }

    /// Open the profiles menu: every cookie-isolated profile with a ✓ on the
    /// active one, then a "New Profile…" action below a divider.
    ///
    /// macOS pops a native `NSMenu` over the webview, deferred onto a foreground
    /// task so its modal run loop opens outside this handler's `App` borrow (see
    /// [`Self::open_appearance_menu`]). The pick is routed through
    /// `pending_profile` and applied on the next render — switching/creating a
    /// profile rebuilds the webview, which needs a `Window` the async task lacks.
    #[cfg(target_os = "macos")]
    pub fn open_profile_menu(&mut self, cx: &mut Context<Self>) {
        let Some(native) = self.native.clone() else {
            return;
        };
        let anchor = self.profile_anchor.get();
        let active = self.profile_id;

        // `actions[id]` is the routing for the leaf with that id. Rows: shared
        // default, one per profile, "New Profile…", then the "Import Cookies"
        // cascade (one submenu per installed browser → its source profiles).
        let mut actions: Vec<MenuAction> = Vec::new();
        let mut entries: Vec<native_menu::MenuEntry> = Vec::new();

        let id = push_action(&mut actions, MenuAction::Profile(ProfileRequest::Switch(None)));
        entries.push(native_menu::MenuEntry::item("Default", active.is_none(), id));
        if let Some(g) = cx.try_global::<BrowserProfiles>() {
            for p in &g.profiles {
                let id =
                    push_action(&mut actions, MenuAction::Profile(ProfileRequest::Switch(Some(p.id))));
                entries.push(native_menu::MenuEntry::item(p.name.clone(), Some(p.id) == active, id));
            }
        }
        let id = push_action(&mut actions, MenuAction::Profile(ProfileRequest::New));
        entries.push(native_menu::MenuEntry::item("New Profile…", false, id).with_separator());

        // Cookie-import cascade — only when at least one browser is installed.
        let detected = cookie_import::detect_installed_browsers();
        if !detected.is_empty() {
            let mut import_children: Vec<native_menu::MenuEntry> = Vec::new();
            for b in &detected {
                let label = format!("From {}", b.descriptor.label);
                if b.profiles.len() > 1 {
                    let mut rows = Vec::new();
                    for p in &b.profiles {
                        let id = push_action(
                            &mut actions,
                            MenuAction::ImportSource {
                                browser_id: b.descriptor.id,
                                profile_dir: p.directory.clone(),
                            },
                        );
                        rows.push(native_menu::MenuEntry::item(p.name.clone(), false, id));
                    }
                    import_children.push(native_menu::MenuEntry::submenu(label, rows));
                } else {
                    let id = push_action(
                        &mut actions,
                        MenuAction::ImportSource {
                            browser_id: b.descriptor.id,
                            profile_dir: b.profiles[0].directory.clone(),
                        },
                    );
                    import_children.push(native_menu::MenuEntry::item(label, false, id));
                }
            }
            let id = push_action(&mut actions, MenuAction::ImportFile);
            import_children.push(native_menu::MenuEntry::item("From File…", false, id).with_separator());
            entries
                .push(native_menu::MenuEntry::submenu("Import Cookies", import_children).with_separator());
        }

        cx.spawn(async move |this, cx| {
            let Some(id) = native.popup_menu_tree(anchor, Some("Profiles"), &entries) else {
                return;
            };
            let mut actions = actions;
            match actions.swap_remove(id as usize) {
                MenuAction::Profile(req) => {
                    let _ = this.update(cx, |view, cx| {
                        view.pending_profile = Some(req);
                        cx.notify();
                    });
                }
                MenuAction::ImportSource { browser_id, profile_dir } => {
                    let label =
                        cookie_import::catalog::by_id(browser_id).map(|d| d.label).unwrap_or("browser");
                    let collected = cx
                        .background_executor()
                        .spawn(async move {
                            cookie_import::collect_from_source(browser_id, &profile_dir)
                        })
                        .await;
                    let _ =
                        this.update(cx, move |view, _cx| view.apply_import_result(collected, label));
                }
                MenuAction::ImportFile => {
                    let picked = rfd::AsyncFileDialog::new()
                        .add_filter("Cookie export (JSON)", &["json"])
                        .pick_file()
                        .await;
                    let Some(file) = picked else { return };
                    let path = file.path().to_path_buf();
                    let collected = cx
                        .background_executor()
                        .spawn(async move { cookie_import::collect_from_file(&path) })
                        .await;
                    let _ =
                        this.update(cx, move |view, _cx| view.apply_import_result(collected, "file"));
                }
            }
        })
        .detach();
    }

    /// Inject a collected cookie set into the active profile's data store and
    /// float a confirmation toast over the page. Runs on the main thread (where
    /// WebKit's cookie store must be touched). Errors surface as a toast too.
    #[cfg(target_os = "macos")]
    fn apply_import_result(
        &mut self,
        result: Result<cookie_import::Collected, cookie_import::ImportError>,
        label: &str,
    ) {
        let Some(native) = self.native.clone() else { return };
        match result {
            Ok(collected) => {
                let imported = native.import_cookies(&collected.cookies);
                let plural = if imported == 1 { "" } else { "s" };
                let mut msg = format!("Imported {imported} cookie{plural} from {label}");
                if collected.skipped > 0 {
                    msg.push_str(&format!(" · {} skipped", collected.skipped));
                }
                msg.push_str(" — reload to apply");
                native.eval_script(&agent_context::confirm_toast_js(&msg));
            }
            Err(e) => {
                native.eval_script(&agent_context::confirm_toast_js(&format!(
                    "Cookie import failed: {e}"
                )));
            }
        }
    }

    /// Off macOS the profiles menu is injected into the page (a GPUI menu would
    /// draw under the native webview); selections route back over IPC and apply
    /// on the next render (where a `Window` exists to rebuild the webview).
    #[cfg(not(target_os = "macos"))]
    pub fn open_profile_menu(&mut self, cx: &mut Context<Self>) {
        let Some(n) = &self.native else { return };
        let active = self.profile_id;
        let mut items: Vec<serde_json::Value> = vec![serde_json::json!({
            "id": "default",
            "name": "Default",
            "active": active.is_none(),
        })];
        if let Some(g) = cx.try_global::<BrowserProfiles>() {
            for p in &g.profiles {
                items.push(serde_json::json!({
                    "id": p.id.to_string(),
                    "name": p.name,
                    "active": Some(p.id) == active,
                }));
            }
        }
        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        n.eval_script(&agent_context::profile_menu_js(&json));
    }

    /// Apply a deferred profiles-menu choice (called from `render`, where a
    /// `Window` exists to rebuild the webview).
    fn apply_profile_request(
        &mut self,
        req: ProfileRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match req {
            ProfileRequest::Switch(id) => self.switch_profile(id, window, cx),
            ProfileRequest::New => self.new_profile(window, cx),
        }
    }

    /// Create a new isolated profile and switch this tab to it.
    pub fn new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = uuid::Uuid::new_v4();
        // Name from the list length INSIDE the mutation so two quick creates
        // can't both read the same count and mint duplicate "Profile N" names.
        cx.update_global::<BrowserProfiles, _>(|g, _| {
            let name = format!("Profile {}", g.profiles.len() + 1);
            g.profiles.push(BrowserProfile { id, name });
        });
        crate::browser_profiles::save(cx.global::<BrowserProfiles>());
        self.switch_profile(Some(id), window, cx);
    }

    /// Point this tab at a different profile's data store, rebuilding the
    /// webview (cookies/cache are bound at build time). No-op if unchanged.
    pub fn switch_profile(
        &mut self,
        profile_id: Option<uuid::Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.profile_id == profile_id {
            return;
        }
        self.profile_id = profile_id;
        let tx = self.events_tx.clone();
        let rebuilt =
            build_native(window, &self.url, profile_id.map(|u| *u.as_bytes()), tx).map(Rc::new);
        if rebuilt.is_none() {
            tracing::warn!("BrowserView: webview rebuild for profile switch failed");
            return;
        }
        // Swap in the fresh webview (dropping the old one tears it down) and
        // restore the live presentation state the rebuild reset. The new page
        // reloads `self.url`, but its title/load state belong to the old
        // profile's page — clear them so the chip doesn't show a stale title
        // or a spinner that never resolves.
        self.native = rebuilt;
        self.devtools_open = false;
        self.title = String::new();
        self.loading = false;
        if let Some(n) = &self.native {
            n.set_appearance(self.appearance);
            n.set_visible(self.active);
        }
        cx.notify();
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
    data_store_id: Option<[u8; 16]>,
    tx: UnboundedSender<ChromeEvent>,
) -> Option<NativeWebview> {
    let nav_tx = tx.clone();
    let title_tx = tx.clone();
    let ipc_tx = tx.clone();
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
        on_ipc: move |body: String| {
            let _ = ipc_tx.unbounded_send(ChromeEvent::Ipc(body));
        },
    };
    NativeWebview::build(
        window,
        url,
        agent_context::INIT_SCRIPT,
        data_store_id,
        callbacks,
    )
    .ok()
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

    #[test]
    fn pick_and_pick_to_agent_share_one_button() {
        // The crosshair button lights up for both clipboard-pick and
        // pick-to-agent, but never for an unrelated probe.
        assert!(CopyKind::Pick.lights_button(CopyKind::PickToAgent));
        assert!(CopyKind::PickToAgent.lights_button(CopyKind::Pick));
        assert!(CopyKind::Pick.lights_button(CopyKind::Pick));
        assert!(!CopyKind::Pick.lights_button(CopyKind::Dom));
        assert!(!CopyKind::Screenshot.lights_button(CopyKind::Console));
    }

    #[test]
    fn pill_labels_distinguish_image_from_text_copies() {
        // "Screenshot" vs "DOM" must read differently — both used to feel like
        // a generic "snapshot".
        assert_eq!(CopyKind::Screenshot.pill_label(), "Screenshot copied");
        assert_eq!(CopyKind::Dom.pill_label(), "DOM copied");
        assert_eq!(CopyKind::PickToAgent.pill_label(), "Sent to agent");
    }
}
