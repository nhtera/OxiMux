//! Turns what a delegated child does into events its parent is owed.
//!
//! One watcher per host (inside [`HostCore`]), keyed off the registry's change
//! signal rather than a session closing: a child that unregisters (tab closed,
//! process gone, rekey) and registers again is simply watched again, and an
//! unregister alone emits nothing and never ends its lineage row.
//!
//! Each registered child is watched through its own event stream. On attach the
//! watcher first reads what the session already retained, so a child that
//! finished its whole first turn before anyone looked still produces its event.
//! Every event is written to the outbox under its **edge identity** (the turn's
//! end, by this session run and sequence number, or the permit's request id), so
//! reading the same turn twice writes one row.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use oximux_agents::envelope::{self, ChildEvent, ChildEventKind, PermitInfo};
use oximux_agents::lineage::{LineageRow, LineageState, USER_ROOT};
use oximux_agents::session_registry::SessionHandle;
use oximux_agents::thread::ThreadEvent;
use tokio::sync::broadcast::error::RecvError;

use crate::host_core::HostCore;

/// How much of a permit's arguments the envelope shows.
const ARGS_SUMMARY_LIMIT: usize = 200;

/// The children being watched: child id → the session run watched.
#[derive(Default)]
pub(crate) struct ChildWatcher {
    attached: Mutex<HashMap<String, Attached>>,
}

struct Attached {
    incarnation: String,
    task: tokio::task::AbortHandle,
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ChildWatcher {
    /// Watch every registered child that is not ended; stop watching the rest.
    pub(crate) fn sync(&self, core: &Arc<HostCore>) {
        let tracked = match core.lineage().tracked() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "child watcher: lineage read failed");
                return;
            }
        };
        let mut attached = self.attached.lock().unwrap();
        let mut keep = std::collections::HashSet::new();
        for (child, state) in tracked {
            if state == LineageState::Ended {
                continue;
            }
            let Some(handle) = core.registry().get(&child) else { continue };
            keep.insert(child.clone());
            if attached.get(&child).is_some_and(|a| a.incarnation == handle.incarnation()) {
                continue;
            }
            handle.set_watched(true);
            let task = core.runtime().spawn(watch_child(Arc::downgrade(core), child.clone(), handle.clone()));
            attached.insert(child, Attached { incarnation: handle.incarnation().to_string(), task: task.abort_handle() });
        }
        attached.retain(|child, _| keep.contains(child));
    }
}

/// Follow one child session run: what it retained, then what it streams.
async fn watch_child(core: Weak<HostCore>, child: String, handle: Arc<SessionHandle>) {
    // Subscribe before reading the backlog so nothing falls in between; an
    // event seen twice is one outbox row (its edge identity).
    let mut live = handle.subscribe();
    let mut turn = TurnScan::default();
    for (seq, event) in handle.events_since(0) {
        if let Some(edge) = turn.observe(seq, &event, handle.incarnation()) {
            let Some(core) = core.upgrade() else { return };
            emit(&core, &child, &handle, edge);
        }
    }
    loop {
        match live.recv().await {
            Ok((seq, event)) => {
                if let Some(edge) = turn.observe(seq, &event, handle.incarnation()) {
                    let Some(core) = core.upgrade() else { return };
                    emit(&core, &child, &handle, edge);
                }
            }
            // A slow watcher skipped events: re-read what is retained (rows
            // already written are no-ops).
            Err(RecvError::Lagged(_)) => {
                for (seq, event) in handle.events_since(0) {
                    if let Some(edge) = turn.observe(seq, &event, handle.incarnation()) {
                        let Some(core) = core.upgrade() else { return };
                        emit(&core, &child, &handle, edge);
                    }
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// One event a child owes its parent, before it is addressed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Edge {
    pub kind: ChildEventKind,
    pub edge_id: String,
    pub status: Option<String>,
    pub result: Option<String>,
    pub permit: Option<(String, String, String)>,
}

/// What the current turn has said so far, to report with its end.
#[derive(Default)]
pub(crate) struct TurnScan {
    /// The agent's latest message since the turn's last tool call.
    text: String,
}

impl TurnScan {
    pub(crate) fn observe(&mut self, seq: u64, event: &ThreadEvent, incarnation: &str) -> Option<Edge> {
        match event {
            ThreadEvent::UserMessage { .. } | ThreadEvent::ToolCallStarted { .. } => self.text.clear(),
            ThreadEvent::AssistantTextDelta(delta) => self.text.push_str(delta),
            ThreadEvent::AssistantText(text) => self.text = text.clone(),
            ThreadEvent::TurnEnded { is_error, result, .. } => {
                let said = std::mem::take(&mut self.text);
                let said = Some(said.trim().to_string()).filter(|t| !t.is_empty());
                return Some(Edge {
                    kind: if *is_error { ChildEventKind::Failed } else { ChildEventKind::Finished },
                    edge_id: format!("{incarnation}:{seq}"),
                    status: if *is_error { result.clone().or(Some("error".into())) } else { None },
                    result: said.or_else(|| result.clone().filter(|_| !is_error)),
                    permit: None,
                });
            }
            ThreadEvent::PermissionRequested { request_id, tool_name, input, .. } => {
                return Some(Edge {
                    kind: ChildEventKind::NeedsPermission,
                    edge_id: request_id.clone(),
                    status: None,
                    result: None,
                    permit: Some((request_id.clone(), tool_name.clone(), summarize(&input.to_string()))),
                });
            }
            ThreadEvent::QuestionAsked { request_id, .. } => {
                return Some(Edge {
                    kind: ChildEventKind::NeedsInput,
                    edge_id: request_id.clone(),
                    status: None,
                    result: None,
                    permit: None,
                });
            }
            _ => {}
        }
        None
    }
}

/// The parent a child's events go to, if it has one that is a session (not
/// the operator, not a team or plan run).
pub(crate) fn session_parent(row: &LineageRow) -> Option<&str> {
    let parent = row.parent_id.as_str();
    (parent != USER_ROOT && !parent.starts_with("team:") && !parent.starts_with("plan:")).then_some(parent)
}

/// Address an edge to the child's parent and write it, if the child's notify
/// setting admits it.
fn emit(core: &Arc<HostCore>, child: &str, handle: &SessionHandle, edge: Edge) {
    if !core.is_owner() {
        return;
    }
    let Ok(Some(row)) = core.lineage().row(child) else { return };
    let Some(parent) = session_parent(&row) else { return };
    if !envelope::admitted(row.notify_parent, edge.kind) {
        return;
    }
    let event = ChildEvent {
        kind: edge.kind,
        child: child.to_string(),
        depth: row.depth,
        title: handle.meta_snapshot().title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| child.to_string()),
        status: edge.status,
        result: edge.result,
        permit: edge.permit.map(|(request_id, tool, args_summary)| PermitInfo {
            request_id,
            answerable: row.allow_parent_permits,
            tool,
            args_summary,
        }),
    };
    core.record_child_event(parent, &event, &edge.edge_id);
}

fn summarize(args: &str) -> String {
    match args.char_indices().nth(ARGS_SUMMARY_LIMIT) {
        Some((at, _)) => format!("{}…", &args[..at]),
        None => args.to_string(),
    }
}
