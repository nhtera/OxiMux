//! What a window contributes to the attention model, and what it does when
//! one of its banners is clicked.
//!
//! Its own module because `workspace_ops.rs` is at its size cap and because
//! these three are one subject: the host decides what needs a person, and the
//! window supplies the facts only a window has — what a session's tab is
//! called, whether anyone is looking at it, and which rows this window raised
//! alerts for.

use gpui::{Context, Window};

use crate::workspace_root::WorkspaceRoot;

impl WorkspaceRoot {
    /// What this window can say about the session a banner is about: what to
    /// call it, which workspace it sits in, and whether the person is looking
    /// at it right now.
    ///
    /// `None` when no tab here holds it — the caller then asks the next
    /// window, and a session no window holds reads as not visible.
    ///
    /// The window-active half is this window's own flag, so a tab that *is*
    /// the visible one in a background window does not read as "the person is
    /// looking at it".
    pub(crate) fn attention_context_for(
        &self,
        chat: Option<&str>,
        target: &str,
        cx: &gpui::App,
    ) -> Option<crate::notifier::attention_bridge::TargetContext> {
        // A cockpit row's key carries its database id; the tab is keyed by the
        // runtime session the live-agent map pairs with it.
        let agent = oximux_agents::session_prefs::cockpit_id(target)
            .and_then(|db_id| self.live_agents.get(db_id))
            .map(|entry| entry.session_id);
        if chat.is_none() && agent.is_none() {
            return None;
        }
        let found = self.project_panes_by_project.values().find_map(|panes| {
            let panes = panes.read(cx);
            let context = panes
                .group_entities()
                .into_iter()
                .find_map(|group| group.read(cx).attention_context(chat, agent, cx))?;
            Some((context, panes.window_active()))
        });
        let ((label, pane_visible), window_active) = found?;
        Some(crate::notifier::attention_bridge::TargetContext {
            label: label.to_string(),
            // The target is the session's identity here. The burst gate no
            // longer sees agent-state traffic — the bridge dedupes by edge —
            // so this field is only a grouping key, and the target is a
            // better one than a worktree path shared by every tab in it.
            workspace_key: target.to_string(),
            pane_visible,
            window_active,
        })
    }

    /// Open whatever one of the attention bridge's banners was about.
    ///
    /// The banner's identifier carries a handle, not a session key: the click
    /// target is `Copy` because one click is re-sent to every window. The
    /// bridge holds the mapping, and for a root's banner it answers with the
    /// child that has been waiting longest — the same place the jump-to-next
    /// hotkey would go.
    pub(crate) fn navigate_to_attention(
        &mut self,
        clicked: crate::notifier::ClickTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = crate::notifier::attention_bridge::AttentionBridge::global(cx)
            .and_then(|bridge| bridge.clicked(clicked))
        else {
            return;
        };
        // A banner can outlive the row it was about: the person may click one
        // from Notification Center long after. Focusing nothing is the right
        // answer then — popping a window with no destination is worse.
        if let Some(session) = oximux_agents::session_prefs::chat_id(&target) {
            self.focus_chat_by_session_id(session, window, cx);
            return;
        }
        if let Some(db_id) = oximux_agents::session_prefs::cockpit_id(&target) {
            match db_id.strip_prefix("ambient:") {
                Some(pty_id) => self.focus_ambient_agent_terminal(pty_id, window, cx),
                None => self.focus_agent_by_db_id(db_id, window, cx),
            };
        }
    }

    /// End the cockpit alerts this window raised, because it is going away.
    ///
    /// The host is shared process-wide and only this window's pass retires its
    /// own keys (`cockpit_attention`), so a window that closes with a blocked
    /// terminal would leave that alert standing for the rest of the run.
    pub(crate) fn retire_cockpit_attention(&mut self, cx: &gpui::App) {
        if let Some(host) =
            cx.try_global::<crate::remote_control::RemoteControl>().and_then(|rc| rc.host_core())
        {
            crate::shell::workspace::cockpit_attention::retire_cockpit_attention(
                &host,
                &mut self.cockpit_attention_raised,
            );
        }
    }
}
