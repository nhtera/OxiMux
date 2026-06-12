//! macOS desktop-notification dispatch via `UNUserNotificationCenter`.
//!
//! One process-global delegate handles click routing and foreground
//! presentation; per-window `MacNotifier` instances share it through a
//! registered-sender list. Each banner gets a namespaced identifier
//! (`agent:{tab}:{seq}` / `test:{seq}`); posting a new banner for a tab
//! removes that tab's previous delivered banner first, so a pane never
//! stacks stale state in Notification Center.
//!
//! `UNUserNotificationCenter` only exists for bundled apps — touching it
//! from a bare `cargo run` binary raises an Objective-C exception. The
//! constructor probes `NSBundle` first and downgrades to a no-op (with a
//! one-time WARN) when unbundled, so the non-notification dev loop keeps
//! working. Authorization denial likewise flips a process-wide flag and
//! short-circuits every later dispatch.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Instant;

use block2::Block;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AllocAnyThread, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSString,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use tokio::sync::mpsc::UnboundedSender;

use super::{
    AgentNotifySettings, BurstGate, ClickTarget, NotificationRequest, NotificationSource,
    Notifier, NotifierAvailability, TabId,
};
use std::sync::Arc;

/// Process-wide "notifications cannot work" latch: unbundled binary or
/// authorization denied. Checked before every dispatch.
static UNAVAILABLE: AtomicBool = AtomicBool::new(false);
/// Why dispatch is unavailable — drives the settings-pane hint copy.
static UNBUNDLED: AtomicBool = AtomicBool::new(false);
/// Identifier sequence shared by every notifier instance, so two windows
/// can never mint the same banner id.
static ID_SEQ: AtomicU64 = AtomicU64::new(1);
/// One-time center setup (delegate + authorization request).
static CENTER_SETUP: Once = Once::new();

/// Click sinks, one per live window. The delegate broadcasts the decoded
/// click target to all of them; each window's router ignores targets it
/// does not own. Closed-window senders fail to send and are pruned in place.
static CLICK_SENDERS: Mutex<Vec<UnboundedSender<ClickTarget>>> = Mutex::new(Vec::new());

/// Process-wide 5s per-workspace burst collapse. Static (not per
/// notifier instance) so the same worktree open in two windows still
/// produces one banner for a same-instant burst.
static BURST: Mutex<Option<BurstGate>> = Mutex::new(None);

/// True when a banner for `workspace_key` may fire now.
fn burst_allows(workspace_key: &str) -> bool {
    let mut slot = match BURST.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.get_or_insert_with(BurstGate::new)
        .should_fire(workspace_key, Instant::now())
}

/// The delegate must outlive the process (`UNUserNotificationCenter`
/// holds it weakly). Stateless: callbacks only touch the statics above,
/// which is what makes the `Send + Sync` assertion below sound.
struct DelegateHolder(#[allow(dead_code)] Retained<NotificationDelegate>);
unsafe impl Send for DelegateHolder {}
unsafe impl Sync for DelegateHolder {}
static DELEGATE: OnceLock<DelegateHolder> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OxiMuxNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive_response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &Block<dyn Fn()>,
        ) {
            let identifier = response.notification().request().identifier().to_string();
            if let Some(target) = parse_click_identifier(&identifier) {
                broadcast_click(target);
            }
            completion_handler.call(());
        }

        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &Block<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // Foreground delivery: the dispatcher already gated on focus /
            // pane visibility, so anything that reaches the OS while the
            // app is frontmost was deliberately allowed — show it.
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::Sound
                | UNNotificationPresentationOptions::List,));
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Decode a banner identifier into its click target: `agent:{tab}:{seq}`
/// → agent tab, `bell:{session}:{seq}` → terminal session. Test banners
/// (`test:{seq}`) and foreign identifiers yield `None` (click just
/// dismisses).
fn parse_click_identifier(identifier: &str) -> Option<ClickTarget> {
    let parse_id = |rest: &str| -> Option<u64> {
        let (id, _seq) = rest.split_once(':')?;
        id.parse().ok()
    };
    if let Some(rest) = identifier.strip_prefix("agent:") {
        return parse_id(rest).map(|id| ClickTarget::AgentTab(TabId(id)));
    }
    if let Some(rest) = identifier.strip_prefix("bell:") {
        return parse_id(rest).map(ClickTarget::TerminalSession);
    }
    None
}

/// Send a decoded click target to every registered window router,
/// pruning senders whose receiver is gone (window closed).
fn broadcast_click(target: ClickTarget) {
    let mut senders = match CLICK_SENDERS.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    senders.retain(|tx| tx.send(target).is_ok());
}

/// True when the process runs from a real `.app` bundle. A bare binary
/// has no bundle identifier, and `UNUserNotificationCenter` raises an
/// Objective-C exception when touched from one.
fn running_in_bundle() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// One-time center wiring: install the click/foreground delegate and ask
/// for banner + sound authorization. Denial latches [`UNAVAILABLE`].
fn setup_center_once() {
    CENTER_SETUP.call_once(|| {
        let delegate = NotificationDelegate::new();
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let _ = DELEGATE.set(DelegateHolder(delegate));

        let handler = block2::StackBlock::new(|granted: Bool, _err: *mut NSError| {
            if !granted.as_bool() {
                UNAVAILABLE.store(true, Ordering::Relaxed);
                tracing::warn!(
                    "notification authorization denied; suppressing dispatch \
                     (System Settings → Notifications → OxiMux)"
                );
            }
        })
        .copy();
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &handler,
        );
    });
}

/// Post a system-infrastructure banner (relay restart, daemon version
/// mismatch). Bypasses the agent settings gates — these are rare,
/// load-bearing messages about the app itself — but still honors the
/// process availability latch. No click routing; non-blocking.
///
/// Callable from any thread: the notification center is documented
/// thread-safe, and the bindings encode that (no main-thread marker is
/// required to obtain or use it) — the relay-respawn path calls this
/// from an async runtime worker.
pub fn post_system_banner(title: &str, body: &str) {
    if !running_in_bundle() {
        tracing::warn!(title, body, "system banner skipped (unbundled dev binary)");
        return;
    }
    setup_center_once();
    if UNAVAILABLE.load(Ordering::Relaxed) {
        return;
    }
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&format!("system:{seq}")),
        &content,
        None,
    );
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.addNotificationRequest_withCompletionHandler(&request, None);
}

/// macOS-backed implementation. Gating order per dispatch: process
/// availability → settings (master / source / focus / kind) → per-
/// workspace burst gate → post (replacing the tab's previous banner).
pub struct MacNotifier {
    /// Live per-kind enable + sound + focus-gate prefs, shared with the
    /// settings pane (which flips the atomics at runtime).
    settings: Arc<AgentNotifySettings>,
    /// Last posted banner id per namespaced source key (`agent:{tab}` /
    /// `bell:{session}`), so a fresh edge replaces the stale banner
    /// instead of stacking under it in Notification Center.
    last_posted: Mutex<HashMap<String, String>>,
}

impl MacNotifier {
    /// `click_tx` is the producer half of an unbounded channel whose
    /// consumer (this window's router) activates the matching workspace
    /// tab when a banner is clicked. `settings` is the shared preference
    /// cell read on every dispatch.
    pub fn new(click_tx: UnboundedSender<ClickTarget>, settings: Arc<AgentNotifySettings>) -> Self {
        if !running_in_bundle() {
            // Set the "why" flag before the availability latch so a
            // concurrent `availability()` read never sees unavailable-
            // without-reason (which would render the wrong hint copy).
            UNBUNDLED.store(true, Ordering::Relaxed);
            if !UNAVAILABLE.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "not running from an .app bundle; desktop notifications disabled \
                     (use scripts/bundle-macos.sh for the notification dev loop)"
                );
            }
        } else {
            setup_center_once();
            match CLICK_SENDERS.lock() {
                Ok(mut senders) => senders.push(click_tx),
                Err(poisoned) => poisoned.into_inner().push(click_tx),
            }
        }
        Self {
            settings,
            last_posted: Mutex::new(HashMap::new()),
        }
    }

    /// Namespaced banner identifier for a request. The namespace doubles
    /// as the click-routing discriminator (see `parse_click_identifier`),
    /// so bell banners — whose `tab_id` carries a raw terminal session id —
    /// never collide with agent tab ids.
    fn mint_identifier(req: &NotificationRequest) -> String {
        let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
        match req.source {
            NotificationSource::Test => format!("test:{seq}"),
            NotificationSource::TerminalBell => format!("bell:{}:{seq}", req.tab_id.0),
            NotificationSource::AgentState => format!("agent:{}:{seq}", req.tab_id.0),
        }
    }

    /// Replacement key for `last_posted`: namespace + id, so an agent tab
    /// and a terminal session that happen to share a numeric id never
    /// evict each other's banner.
    fn replace_key(req: &NotificationRequest) -> String {
        match req.source {
            NotificationSource::Test => "test".to_string(),
            NotificationSource::TerminalBell => format!("bell:{}", req.tab_id.0),
            NotificationSource::AgentState => format!("agent:{}", req.tab_id.0),
        }
    }
}

impl Notifier for MacNotifier {
    fn notify(&self, req: NotificationRequest) {
        if UNAVAILABLE.load(Ordering::Relaxed) {
            return;
        }
        if !self.settings.should_fire(&req) {
            return;
        }
        if req.source != NotificationSource::Test && !burst_allows(&req.workspace_key) {
            return;
        }

        let title = req.kind.title(&req.label);
        let message = if req.body.is_empty() {
            "Click to jump to the workspace.".to_string()
        } else {
            req.body.clone()
        };
        let identifier = Self::mint_identifier(&req);
        let stale = {
            let mut last = match self.last_posted.lock() {
                Ok(l) => l,
                Err(poisoned) => poisoned.into_inner(),
            };
            if req.source == NotificationSource::Test {
                None
            } else {
                last.insert(Self::replace_key(&req), identifier.clone())
            }
        };

        let center = UNUserNotificationCenter::currentNotificationCenter();
        if let Some(stale_id) = stale {
            let ids = NSArray::from_retained_slice(&[NSString::from_str(&stale_id)]);
            center.removeDeliveredNotificationsWithIdentifiers(&ids);
        }

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&title));
        content.setBody(&NSString::from_str(&message));
        if self.settings.sound_enabled() {
            content.setSound(Some(&UNNotificationSound::defaultSound()));
        }
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&identifier),
            &content,
            None,
        );
        let completion = block2::StackBlock::new(|err: *mut NSError| {
            if !err.is_null() {
                let desc = unsafe { (*err).localizedDescription().to_string() };
                tracing::warn!(error = %desc, "notification dispatch failed");
            }
        })
        .copy();
        center.addNotificationRequest_withCompletionHandler(&request, Some(&completion));
    }

    fn availability(&self) -> NotifierAvailability {
        if !UNAVAILABLE.load(Ordering::Relaxed) {
            NotifierAvailability::Available
        } else if UNBUNDLED.load(Ordering::Relaxed) {
            NotifierAvailability::Unbundled
        } else {
            NotifierAvailability::PermissionDenied
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_identifier_roundtrip() {
        assert_eq!(
            parse_click_identifier("agent:42:7"),
            Some(ClickTarget::AgentTab(TabId(42)))
        );
        assert_eq!(
            parse_click_identifier("bell:9:3"),
            Some(ClickTarget::TerminalSession(9))
        );
        assert_eq!(parse_click_identifier("test:7"), None);
        assert_eq!(parse_click_identifier("agent:notanum:7"), None);
        assert_eq!(parse_click_identifier("agent:42"), None);
        assert_eq!(parse_click_identifier("bell:42"), None);
        assert_eq!(parse_click_identifier(""), None);
    }
}
