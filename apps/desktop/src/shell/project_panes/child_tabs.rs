//! Routing a spawned child to the group that holds its parent.

use super::*;
use oximux_agents::thread::launch_posture::SpawnPosture;

impl ProjectPanes {
    /// Every chat view in every group of this project.
    pub fn agent_chat_views(&self, cx: &gpui::App) -> Vec<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.groups.values().flat_map(|g| g.read(cx).agent_chat_views()).collect()
    }

    /// Whether any group here holds the chat `remote_session_id`.
    pub fn holds_chat(&self, remote_session_id: &str, cx: &gpui::App) -> bool {
        self.agent_chat_view_by_remote_id(remote_session_id, cx).is_some()
    }

    /// Open a spawned child beside its parent, in whichever group holds the
    /// parent. Does not switch the active group. `None` when no group does.
    #[allow(clippy::too_many_arguments)]
    pub fn open_child_chat_tab(
        &mut self,
        parent: &str,
        cwd: PathBuf,
        model: Option<String>,
        backend: oximux_agents::thread::ChatBackend,
        posture: SpawnPosture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let group = self
            .groups
            .values()
            .find(|g| g.read(cx).agent_chat_view_by_remote_id(parent, cx).is_some())
            .cloned()?;
        group.update(cx, |g, cx| g.open_child_chat_tab(parent, cwd, model, backend, posture, window, cx))
    }
}
