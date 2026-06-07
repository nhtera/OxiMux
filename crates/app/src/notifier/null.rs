//! No-op `Notifier` — non-mac builds and as a permission-denied fallback.

use super::{NotificationKind, Notifier, TabId};

/// Discards every dispatch. Used on non-macOS targets (where no OS
/// notification path is wired) and as the universal mock for tests that
/// only care about whether higher layers attempt a call.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn notify(
        &self,
        _tab_id: TabId,
        _kind: NotificationKind,
        _agent_label: &str,
        _body: &str,
        _window_active: bool,
    ) {
    }
}
