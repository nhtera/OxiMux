//! A spawned child agent's tab: opened right after its parent's tab (after the
//! parent's earlier children), labelled `↳ …`, and never activated — an agent
//! starting helpers must not pull the user's eye or focus off what they are
//! doing. The spawn tree (P5) is the navigation; the tab strip only keeps the
//! family together.

use super::*;
use oximux_agents::thread::launch_posture::SpawnPosture;

impl PaneGroup {
    /// Open a spawned child's chat beside its parent's tab without activating
    /// it. Returns the child's session id; `None` when this group does not hold
    /// the parent.
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
        let parent_ix = self.chat_tab_index_by_remote_id(parent, cx)?;
        let (theme, density, typography) = (self.theme, self.density, self.typography.clone());
        let cwd_for_view = cwd.clone();
        let model_for_view = model.clone();
        let view = cx.new(|cx| {
            crate::shell::agent_chat::AgentChatView::new_child(
                cwd_for_view,
                model_for_view,
                backend,
                posture,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        let ix = self.push_agent_chat_view(view, cwd, model, false, window, cx);
        let label = format!("↳ {}", self.tabs[ix].label);
        self.tabs[ix].label = SharedString::from(label);
        let parent_tab = &self.tabs[parent_ix];
        let parent_name = parent_tab.custom_title.clone().unwrap_or_else(|| parent_tab.label.clone());
        self.tabs[ix].tooltip = Some(SharedString::from(format!("Spawned by {parent_name}")));
        self.place_after_family(ix, parent_ix, parent, cx);
        self.sync_remote_tab_title(ix, cx);
        cx.notify();
        self.chat_session_id_at(ix, cx)
    }

    /// Every chat view in this group.
    pub fn agent_chat_views(&self) -> Vec<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.content {
                PaneContent::AgentChat(view) => Some(view.clone()),
                _ => None,
            })
            .collect()
    }

    /// The tab index of the chat bound to `remote_session_id`, if this group
    /// holds it.
    pub fn chat_tab_index_by_remote_id(&self, remote_session_id: &str, cx: &App) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            matches!(&tab.content, PaneContent::AgentChat(view)
                if view.read(cx).remote_session_id() == remote_session_id)
        })
    }

    /// Move tab `ix` in the strip to just after `parent_ix` and any of
    /// `parent`'s children already sitting there (read from the spawn tree).
    fn place_after_family(&mut self, ix: usize, parent_ix: usize, parent: &str, cx: &App) {
        let lineage = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
            .map(|core| core.lineage().clone());
        self.tab_order.retain(|&t| t != ix);
        let Some(mut at) = self.tab_order.iter().position(|&t| t == parent_ix) else {
            self.tab_order.push(ix);
            return;
        };
        at += 1;
        while let Some(&next) = self.tab_order.get(at) {
            let is_sibling = self.chat_session_id_at(next, cx).is_some_and(|sid| {
                lineage
                    .as_ref()
                    .and_then(|l| l.row(&sid).ok().flatten())
                    .is_some_and(|row| row.parent_id == parent)
            });
            if !is_sibling {
                break;
            }
            at += 1;
        }
        self.tab_order.insert(at, ix);
    }
}
