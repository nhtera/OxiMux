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
        // Every project is searched for the *label* — a session's tab can be
        // in a project that is not on screen, and the banner should still say
        // what it is called. Visibility is another matter: a group's active
        // tab in a project the person is not looking at is not visible, and
        // reporting it as such suppressed the banner entirely (a visible pane
        // in an active window never banners, whatever the focus preference).
        let active = self.active_project.as_ref().map(|project| project.id.clone());
        let found = self.project_panes_by_project.iter().find_map(|(project_id, panes)| {
            let panes = panes.read(cx);
            let (label, active_tab) = panes
                .group_entities()
                .into_iter()
                .find_map(|group| group.read(cx).attention_context(chat, agent, cx))?;
            let on_screen = active.as_deref() == Some(project_id.as_str());
            Some(((label, active_tab && on_screen), panes.window_active()))
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
        self.navigate_to_attention_target(&target, window, cx);
    }

    /// Open the session a namespaced target names.
    ///
    /// Shared by a banner's click and by ⌘⇧U, so the two cannot disagree about
    /// where a given target lives. The session may have no tab at all — a
    /// child another agent spawned, or a row whose tab has since closed. The
    /// Agents page is then the answer rather than nothing: it is where that
    /// row is, and where the person can deal with it.
    pub(crate) fn navigate_to_attention_target(
        &mut self,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = if let Some(session) = oximux_agents::session_prefs::chat_id(target) {
            self.focus_chat_by_session_id(session, window, cx)
        } else if let Some(db_id) = oximux_agents::session_prefs::cockpit_id(target) {
            match db_id.strip_prefix("ambient:") {
                Some(pty_id) => self.focus_ambient_agent_terminal(pty_id, window, cx),
                None => self.focus_agent_by_db_id(db_id, window, cx),
            }
        } else {
            false
        };
        if !focused {
            self.show_agents_page(cx);
        }
    }

    /// The attention target of the row the rail has focused, if any.
    ///
    /// What "this session" means for `MarkUnread`: the one the person is
    /// looking at. The rail already tracks it, so this reads that rather than
    /// keeping a selection of its own to drift.
    pub(crate) fn focused_attention_target(&self, cx: &gpui::App) -> Option<String> {
        let focused = self.left_rail.read(cx).focused_agent_target()?;
        let db_id = match &focused {
            crate::shell::left_rail::rail_agent_row::RailAgentTarget::AgentSession { db_id } => {
                db_id.clone()
            }
            crate::shell::left_rail::rail_agent_row::RailAgentTarget::AmbientTerminal {
                pty_id,
            } => format!("ambient:{pty_id}"),
            crate::shell::left_rail::rail_agent_row::RailAgentTarget::Chat { session_id } => {
                session_id.clone()
            }
        };
        Some(crate::shell::agents_dashboard::row_attention::attention_target(&db_id, &focused))
    }

    /// Open a dashboard row's context menu at the pointer.
    ///
    /// Closes the other overlays first: two full-window dismiss regions
    /// competing is the bug the `close_modal_overlays` contract exists for.
    pub(crate) fn open_agent_row_menu(
        &mut self,
        db_id: String,
        target: crate::shell::left_rail::rail_agent_row::RailAgentTarget,
        attention: Option<&oximux_agents::attention::Attention>,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.close_modal_overlays(cx);
        let menu = self.agent_row_menu.clone();
        menu.update(cx, |menu, cx| menu.open(db_id, target, attention, x, y, cx));
        cx.notify();
    }

    /// Do what a row's context menu asked.
    ///
    /// Both write through the host, because both are facts about what the
    /// person has seen and the host owns that: `mark_viewed` is the one call
    /// behind "viewed" and "Dismiss", and the unread mark is what keeps a row
    /// standing out after it has been read.
    pub(crate) fn act_on_agent_row(
        &mut self,
        db_id: &str,
        target: &crate::shell::left_rail::rail_agent_row::RailAgentTarget,
        action: crate::shell::agents_dashboard::agent_row_menu::AgentRowAction,
        cx: &mut Context<Self>,
    ) {
        use crate::shell::agents_dashboard::agent_row_menu::AgentRowAction;
        let Some(host) = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
        else {
            return;
        };
        let key = crate::shell::agents_dashboard::row_attention::attention_target(db_id, target);
        match action {
            // A dismissal is a look: the person has seen this and dealt with
            // it. The evaluator decides what that clears — a failure and a
            // stall go, a permission stays, because the agent is still
            // waiting on an answer.
            AgentRowAction::Dismiss => host.mark_viewed(&key),
            AgentRowAction::MarkUnread => host.mark_unread(&key),
        }
        cx.notify();
    }

    /// Open the Agents page, for a banner whose session has no tab to focus.
    pub(crate) fn show_agents_page(&mut self, cx: &mut Context<Self>) {
        let rail = self.left_rail.clone();
        rail.update(cx, |rail, cx| {
            rail.select_nav(crate::shell::left_rail::nav_section::NavItem::Agents, cx);
        });
        cx.notify();
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
