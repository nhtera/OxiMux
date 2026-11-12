//! Read the host's registered chat sessions into rail rows.
//!
//! The rail's other rows come from `agent_sessions` and from agents detected
//! inside plain terminals. Chat sessions are in neither: they keep no DB row,
//! and a session another agent spawned has no tab of its own either. The
//! registry is the only place that knows about all of them, which is why the
//! rows are sourced from it rather than from the open tabs.
//!
//! Its own module so `workspace_ops.rs` gains a call rather than a loop.

use std::collections::HashMap;

use gpui::App;
use oximux_core::Workspace;

use crate::shell::session_merge::ChatRow;

/// Every registered chat session that resolves to one of these workspaces.
///
/// `resolve` maps a session's own working directory to a worktree root. It is
/// passed in because a cwd is not necessarily a worktree root — it can sit
/// anywhere beneath one, and nested worktrees need the longest match — and the
/// caller already owns that resolver for the ambient rows.
///
/// A session with no published cwd is skipped: nothing can say which workspace
/// it belongs under, and guessing would file it against the wrong project.
pub fn collect_chat_rows(
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    resolve: impl Fn(&str, &HashMap<String, Vec<Workspace>>) -> Option<String>,
    cx: &App,
) -> Vec<ChatRow> {
    // The registry, not the host core: the rows only need who is registered
    // and what each one is doing. Their attention is resolved separately, by
    // the one evaluator.
    let Some(registry) = cx
        .try_global::<crate::remote_control::RemoteControl>()
        .map(|rc| rc.registry())
    else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for id in registry.ids() {
        let Some(handle) = registry.get(&id) else {
            continue;
        };
        let meta = handle.meta_snapshot();
        let Some(cwd) = meta.cwd else {
            continue;
        };
        let Some(worktree_path) = resolve(&cwd.to_string_lossy(), workspaces_by_project) else {
            continue;
        };
        let status = handle.status_snapshot();
        rows.push(ChatRow {
            session_id: id.to_string(),
            worktree_path,
            title: meta.title,
            // The registry carries no adapter: `SessionMeta` publishes the
            // title, model, mode and cwd, and adding a field to it is a wire
            // change. The title is what the row actually shows, so the label
            // is only the fallback for a session that has not titled itself
            // yet, and the icon falls back like any unknown adapter. PR D
            // rewrites this row anyway, from the attention's own verb.
            label: "Chat".to_string(),
            adapter_id: String::new(),
            working: status.working,
            awaiting_permission: status.awaiting_permission,
        });
    }
    rows
}
