//! Terminal notifications: a program's OSC 9 / OSC 777 request, or the user's
//! own `oximux notify`, turned into pane attention plus a desktop banner.
//!
//! Program requests are opt-in (`terminal.osc_notifications`): any program can
//! print those bytes. The banner's title is always OxiMux's — the tab label —
//! with the program's title quoted in the body, and each pane raises at most
//! one banner per [`NOTIFY_BANNER_DEBOUNCE`].

use super::*;

/// Shortest gap between two terminal-notification banners from one pane.
const NOTIFY_BANNER_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

impl TerminalView {
    /// Whether a notification request is shown at all: the user's own always,
    /// a program's only when the user opted in.
    pub(super) fn accepts_notification(explicit: bool, settings: &TerminalSettings) -> bool {
        explicit || settings.osc_notifications
    }

    /// An agent here already raises its own banners (program status or hook
    /// sideband), so its OSC 9 / 777 would only be a second one.
    pub(super) fn agent_reports_itself(&self) -> bool {
        self.agent_scan.program_status_in_force() || self.agent_scan.current(std::time::Instant::now()).is_some()
    }

    /// Hand a notification to the pane group's one terminal-banner path. The
    /// text is sanitized there; this end only rate-limits the pane.
    pub(super) fn maybe_notify_terminal(&mut self, title: Option<String>, body: String, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        if self
            .last_notify_banner
            .is_some_and(|t| now.duration_since(t) < NOTIFY_BANNER_DEBOUNCE)
        {
            return;
        }
        let Some(body) = crate::notifier::terminal_notification_body(title.as_deref(), &body) else {
            return;
        };
        self.last_notify_banner = Some(now);
        let session = self.session_id;
        let pane_visible = self.is_visible();
        if let Some(opener) = self.opener.clone() {
            // Deferred for the same reason as the bell: this runs inside this
            // view's own update, and the group's tab walk reads every view.
            cx.defer(move |cx| {
                let _ = opener.update(cx, |pg, cx| {
                    pg.notify_terminal(
                        session,
                        crate::notifier::NotificationKind::TerminalNotify,
                        body,
                        pane_visible,
                        cx,
                    );
                });
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_notifications_are_opt_in_and_the_users_are_not() {
        let mut settings = TerminalSettings::default();
        assert!(!TerminalView::accepts_notification(false, &settings), "off by default");
        assert!(TerminalView::accepts_notification(true, &settings));
        settings.osc_notifications = true;
        assert!(TerminalView::accepts_notification(false, &settings));
    }
}
