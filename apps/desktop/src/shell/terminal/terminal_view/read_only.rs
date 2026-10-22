//! A terminal's read-only lock as this view sees it (the lock itself is
//! per terminal, in `terminal_locks`): blocking input, telling the user once,
//! and keeping the lock attached to whichever daemon PTY the terminal runs on.

use super::*;
use crate::shell::terminal::terminal_locks;

impl TerminalView {
    /// Whether this terminal is read-only.
    pub fn is_read_only(&self) -> bool {
        terminal_locks::is_locked(self.tab_id())
    }

    /// Lock or unlock this terminal, for every view of it.
    pub fn toggle_read_only(&mut self, cx: &mut Context<Self>) {
        let locked = !self.is_read_only();
        let tab_id = self.tab_id().to_owned();
        let pty = self.external_id();
        terminal_locks::set(&tab_id, pty.as_deref(), locked);
        self.read_only_notice_at = None;
        cx.notify();
    }

    /// `true` (and the input is to be dropped) when the terminal is locked.
    /// A blocked input says why, at most once a minute per view — timed rather
    /// than once per lock, since the lock can be re-set from another view.
    pub(super) fn input_blocked_by_lock(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.is_read_only() {
            return false;
        }
        let due = self.read_only_notice_at.is_none_or(|at| at.elapsed().as_secs() >= 60);
        if due {
            self.read_only_notice_at = Some(std::time::Instant::now());
            crate::shell::toast::toast(
                cx,
                crate::shell::toast::ToastKind::Info,
                "Pane is read-only — unlock it from the terminal's menu or the command palette \
                 (Toggle Read-Only Terminal).",
            );
        }
        true
    }

    /// The "Read-only" pill shown at the pane's top right while locked.
    pub(super) fn read_only_pill(&self) -> Option<gpui::Div> {
        self.is_read_only().then(|| {
            div()
                .absolute()
                .top(px(6.0))
                .right(px(10.0))
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(self.density.r_xs))
                .bg(self.theme.bg_overlay)
                .text_color(self.theme.fg_muted)
                .text_size(px(self.typography.t_body_sm))
                .border_1()
                .border_color(self.theme.border_inactive)
                .child("🔒 Read-only")
        })
    }

    /// The terminal now runs on a (possibly new) daemon PTY: a locked terminal
    /// keeps that PTY locked for remote input too.
    pub(super) fn bind_lock_to_session(&self) {
        if let Some(pty) = self.external_id() {
            let tab_id = self.tab_id().to_owned();
            terminal_locks::bind_pty(&tab_id, &pty);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn a_lock_blocks_input_for_every_view_of_the_terminal(cx: &mut TestAppContext) {
        let mount = |cx: &mut TestAppContext| {
            let (backend, sid) = spawn_local_pty_dormant(80, 24).expect("dormant spawn");
            cx.add_window(|win, cx| {
                TerminalView::mount_pending(
                    backend,
                    sid,
                    SurfaceIds::restored("/proj".to_string(), "surface-ro".to_string(), "tab-read-only-test".to_string()),
                    None,
                    Theme::default(),
                    Density::default(),
                    Typography::default(),
                    win,
                    cx,
                )
            })
        };
        let first = mount(cx);
        let second = mount(cx);
        first.update(cx, |v, _w, cx| v.toggle_read_only(cx)).unwrap();
        second
            .update(cx, |v, _w, cx| {
                assert!(v.is_read_only(), "the lock is the terminal's, not one view's");
                assert!(v.input_blocked_by_lock(cx));
                assert!(v.read_only_notice_at.is_some(), "the first blocked input says why");
                assert!(v.read_only_pill().is_some());
            })
            .unwrap();
        second.update(cx, |v, _w, cx| v.toggle_read_only(cx)).unwrap();
        first
            .update(cx, |v, _w, cx| {
                assert!(!v.is_read_only());
                assert!(!v.input_blocked_by_lock(cx));
            })
            .unwrap();
    }
}
