//! macOS desktop-notification dispatch via `mac-notification-sys`.
//!
//! `notify_needs_approval` schedules one `tokio::task::spawn_blocking` per
//! dispatch. The blocking task posts a banner with `wait_for_click: true`
//! so the upstream call blocks the worker thread until the user clicks or
//! the banner is dismissed. On `NotificationResponse::Click` the agent's
//! session id is pushed through an mpsc sender; a GPUI-side receiver
//! activates the originating workspace tab. Other response variants do
//! nothing (the badge stays amber until the agent itself transitions out
//! of `NeedsApproval`).
//!
//! macOS does not expose a synchronous "is authorized?" query through this
//! crate. The first dispatch returning `Err` is treated as permission
//! denial: we set an `AtomicBool`, log once at WARN, and short-circuit
//! every subsequent call for the process lifetime.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mac_notification_sys::{Notification, NotificationResponse};
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentNotifySettings, NotificationKind, Notifier, TabId};

/// macOS-backed implementation. The mpsc sender bridges from the blocking
/// worker thread back into the GPUI runtime; an `AsyncApp` handle cannot
/// cross that boundary because it holds an `Rc`-based weak reference and
/// is therefore `!Send`.
pub struct MacNotifier {
    click_tx: UnboundedSender<u64>,
    permission_denied: Arc<AtomicBool>,
    /// Live per-kind enable + sound + focus-gate prefs, shared with the
    /// settings pane (which flips the atomics at runtime).
    settings: Arc<AgentNotifySettings>,
}

impl MacNotifier {
    /// `click_tx` is the producer half of an unbounded channel whose
    /// consumer activates the matching workspace tab when a notification
    /// is clicked. Unbounded keeps the OS callback path lock-free; the
    /// receiver is a single GPUI-side task that drains promptly. `settings`
    /// is the shared preference cell read on every dispatch.
    pub fn new(click_tx: UnboundedSender<u64>, settings: Arc<AgentNotifySettings>) -> Self {
        Self {
            click_tx,
            permission_denied: Arc::new(AtomicBool::new(false)),
            settings,
        }
    }
}

impl Notifier for MacNotifier {
    fn notify(
        &self,
        tab_id: TabId,
        kind: NotificationKind,
        agent_label: &str,
        body: &str,
        window_active: bool,
    ) {
        if self.permission_denied.load(Ordering::Relaxed) {
            return;
        }
        // Per-kind enable + focus gate live in the shared settings cell.
        if !self.settings.should_fire(kind, window_active) {
            return;
        }
        let title = kind.title(agent_label);
        let message = if body.is_empty() {
            "Click to switch to the tab.".to_string()
        } else {
            body.to_string()
        };
        let sound = self.settings.sound_enabled().then(|| kind.sound_name());
        let click_tx = self.click_tx.clone();
        let permission_denied = Arc::clone(&self.permission_denied);
        let tab_id_raw = tab_id.0;

        // JoinHandle dropped intentionally: the closure body is the single
        // FFI call plus a non-blocking mpsc send, so a panic here cannot
        // leave the calling task in a corrupted state. spawn_blocking
        // honors its own panic policy (logged + thread retired).
        tokio::task::spawn_blocking(move || {
            let mut builder = Notification::new();
            builder.title(&title).message(&message).wait_for_click(true);
            if let Some(name) = sound {
                builder.sound(name);
            }
            let result = builder.send();
            match result {
                Ok(NotificationResponse::Click) => {
                    // Receiver may be dropped at app shutdown; ignore.
                    let _ = click_tx.send(tab_id_raw);
                }
                Ok(_) => {
                    // None / CloseButton / ActionButton / Reply — user
                    // dismissed without engaging. Badge stays amber until
                    // the agent transitions out of NeedsApproval.
                }
                Err(err) => {
                    if !permission_denied.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            ?err,
                            "macOS notification dispatch failed; suppressing further attempts \
                             this session (check System Settings → Notifications)"
                        );
                    }
                }
            }
        });
    }
}
