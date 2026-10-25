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
        self.tabs[ix].tooltip = Some(spawned_by(&self.tabs[parent_ix]));
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

    /// A restored helper's "Spawned by …" names its parent as the parent's tab
    /// reads now: a restored chat is renumbered, so the saved text could name
    /// another tab. A helper whose parent is not open here keeps the saved text.
    pub(super) fn refresh_spawned_by(&mut self, cx: &App) {
        let lineage = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
            .map(|core| core.lineage().clone());
        let Some(lineage) = lineage else { return };
        self.refresh_spawned_by_with(|sid| lineage.row(sid).ok().flatten().map(|row| row.parent_id), cx);
    }

    /// [`Self::refresh_spawned_by`] over any spawn tree (`parent_of`).
    pub(super) fn refresh_spawned_by_with(&mut self, parent_of: impl Fn(&str) -> Option<String>, cx: &App) {
        for ix in 0..self.tabs.len() {
            if self.tabs[ix].tooltip.is_none() {
                continue;
            }
            let Some(parent) = self.chat_session_id_at(ix, cx).and_then(|sid| parent_of(&sid)) else {
                continue;
            };
            if let Some(parent_ix) = self.chat_tab_index_by_remote_id(&parent, cx) {
                self.tabs[ix].tooltip = Some(spawned_by(&self.tabs[parent_ix]));
            }
        }
    }
}

/// A child tab's hover text, naming its parent as the parent's tab reads.
fn spawned_by(parent: &PaneGroupTab) -> SharedString {
    let name = parent.custom_title.clone().unwrap_or_else(|| parent.label.clone());
    SharedString::from(format!("Spawned by {name}"))
}
