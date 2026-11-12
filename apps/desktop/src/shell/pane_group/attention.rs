//! What a pane group knows about the sessions in it, for the attention model.
//!
//! Two facts the host cannot have: what a session's tab is called and whether
//! it is the one in front. Its own module because `tabs.rs` is near its size
//! cap, and because this is the whole of what is left of the chat
//! notification path — the deciding moved to the host, and only the context
//! and the look stayed behind.

use gpui::{App, Context, SharedString};
use oximux_core::AgentSessionId;

use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::{PaneGroup, PaneGroupTabKind};

impl PaneGroup {
    /// Dispatch a chat-attention banner (turn finished / errored / needs
    /// approval / question / auth) for the chat tab backing `view`, through the
     /// The context one of this group's tabs can supply about a session the
    /// attention bridge is about to banner: what to call it, and whether the
    /// person is looking at it right now.
    ///
    /// Read-only, and it replaces `notify_chat_attention`, which used to post
    /// the chat banner from here. A tab knows its label and whether it is the
    /// visible one; it does not know whether anyone has dealt with the thing
    /// the agent is asking about. That is the host's to say, so the decision
    /// moved there and only the context stayed.
    ///
    /// `chat` matches the registry id a chat view is published under; `agent`
    /// matches a cockpit tab's runtime session id.
    pub fn attention_context(
        &self,
        chat: Option<&str>,
        agent: Option<AgentSessionId>,
        cx: &App,
    ) -> Option<(SharedString, bool)> {
        let idx = self.tabs.iter().position(|tab| match (&tab.content, &tab.kind) {
            (PaneContent::AgentChat(view), _) => {
                chat.is_some_and(|id| view.read(cx).remote_session_id() == id)
            }
            (_, PaneGroupTabKind::Agent { session_id, .. }) => agent == Some(*session_id),
            _ => false,
        })?;
        let tab = &self.tabs[idx];
        let label = tab.custom_title.clone().unwrap_or_else(|| tab.label.clone());
        // Visible = the group's active tab. The window-active half is the
        // bridge's to add; a group cannot see which window is in front.
        Some((label, idx == self.active))
    }

    /// Opening a session's tab is reading it.
    ///
    /// What ends an alert nobody has to answer: a failure stays in NEEDS YOU
    /// until it is viewed or dismissed, and a finished turn stays unread until
    /// it is read. This is the chat half — a cockpit row's read is recorded by
    /// the rail and dashboard rows, which is where that row is clicked.
    ///
    /// Pays for itself: it is what is left of `notify_chat_attention`, which
    /// posted the chat's banner from here and relied on a window refocus to
    /// reopen its gate. Refocusing a window is not reading a session.
    pub(crate) fn note_read(&self, idx: usize, cx: &mut Context<Self>) {
        let Some(PaneContent::AgentChat(view)) = self.tabs.get(idx).map(|tab| &tab.content) else {
            return;
        };
        let target = oximux_agents::session_prefs::chat_key(view.read(cx).remote_session_id());
        if let Some(host) = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
        {
            host.mark_viewed(&target);
        }
    }}
