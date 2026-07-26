//! Per-agent-session persistence watcher.
//!
//! The `agent_sessions` table and its boot-time interrupted-marking have
//! existed since the storage phase, but nothing ever inserted rows or wrote
//! live transitions — the rail/dashboard status map (which reads SQLite)
//! stayed empty, so terminal states (Done / Failed / Stopped) never showed
//! on workspace rows. This module closes that gap.
//!
//! One watcher per agent launch, spawned by `WorkspaceRoot` right after the
//! status channel exists (fresh spawns AND relay re-adopts). It:
//!
//! 1. resolves the workspace key for the launch cwd (a `workspaces.id`
//!    UUID, or `primary:<project_id>` for repo-root launches — V013
//!    dropped the FK so the synthesized key is storable),
//! 2. inserts the session row (status `idle`),
//! 3. mirrors every status transition into the row (+ `ended_at` on
//!    terminal states) and nudges the rail so rows repaint event-driven.
//!
//! All SQLite work runs on the background executor; the watch loop itself
//! is async and idles between transitions. The task self-terminates after
//! the terminal status is written (or when the channel closes), so
//! `.detach()` cannot leak — the sender drops when the runtime poll task
//! exits.

use gpui::Context;
use oximux_agents::AgentStatusStream;
use oximux_core::{AgentSessionId, AgentStatus};

use crate::shell::agent_presentation::adapter_display_name;
use crate::shell::session_live_store::LiveAgentEntry;
use crate::workspace_root::WorkspaceRoot;

/// Spawn the persistence watcher for one agent session. `worktree_path` is
/// the launch cwd used to resolve the owning workspace; `adapter_id` /
/// `model` / `effort` are stored verbatim on the row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_for_session(
    root: &WorkspaceRoot,
    worktree_path: String,
    adapter_id: &'static str,
    model: Option<String>,
    effort: Option<String>,
    runtime_session_id: AgentSessionId,
    mut status_rx: AgentStatusStream,
    // `true` when this is a restore re-adopt: try to reuse the prior
    // interrupted DB row (keeping its title) before inserting a fresh one.
    restoring: bool,
    cx: &mut Context<WorkspaceRoot>,
) {
    let workspace_repo = root.app_state.workspace_repo.clone();
    let project_repo = root.app_state.project_repo.clone();
    let agent_repo = root.app_state.agent_session_repo.clone();

    cx.spawn(async move |weak, cx| {
        // Resolve the workspace key + insert the row off the main thread.
        let path = worktree_path.clone();
        let workspace_key = cx
            .background_executor()
            .spawn(async move {
                match workspace_repo.get_by_worktree_path(&path) {
                    Ok(Some(w)) => Some(w.id),
                    Ok(None) => match project_repo.get_by_root_path(&path) {
                        Ok(Some(p)) => Some(format!("primary:{}", p.id)),
                        Ok(None) => None,
                        Err(err) => {
                            tracing::warn!(?err, "agent persistence: project lookup failed");
                            None
                        }
                    },
                    Err(err) => {
                        tracing::warn!(?err, "agent persistence: workspace lookup failed");
                        None
                    }
                }
            })
            .await;
        let Some(workspace_key) = workspace_key else {
            // A cwd outside every known project/workspace (e.g. a custom
            // launch dir). Status still flows to the tab badge via the
            // watch channel; only row history is skipped.
            tracing::info!(
                %worktree_path,
                "agent persistence: no workspace/project row for cwd; skipping"
            );
            return;
        };

        // Keep the resolved key for the live sideband map; `workspace_key`
        // itself is moved into the insert task below.
        let ws_key = workspace_key.clone();

        let repo = agent_repo.clone();
        let (m, e) = (model.clone(), effort.clone());
        // On restore, re-adopt the prior interrupted session for this
        // (workspace, adapter) so the tab keeps its DB row + persisted title
        // instead of orphaning it and inserting a duplicate. A fresh spawn — or
        // a restore with nothing claimable — inserts a new row.
        let ws_claim = workspace_key;
        let resolved = cx
            .background_executor()
            .spawn(async move {
                if restoring {
                    match repo.claim_interrupted_for_restore(&ws_claim, adapter_id) {
                        Ok(Some((id, started))) => {
                            return Ok((id, started.unwrap_or_default()));
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(?err, "agent persistence: restore claim failed");
                        }
                    }
                }
                repo.insert(&ws_claim, adapter_id, m.as_deref(), e.as_deref())
                    .map(|s| (s.id, s.started_at.unwrap_or_default()))
            })
            .await;
        let (row_id, started_at) = match resolved {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(?err, "agent persistence: session insert failed");
                return;
            }
        };

        // Register the live entry now that the stable DB UUID exists. The rail
        // lists every open agent by this key (and dedups it against the DB
        // history row, which shares the same id). `register_live_agent` marks
        // the rail dirty, so no separate repaint nudge is needed here.
        let _ = weak.update(cx, |this, cx| {
            this.register_live_agent(
                row_id.clone(),
                LiveAgentEntry {
                    workspace_key: ws_key.clone(),
                    adapter_id,
                    label: adapter_display_name(adapter_id).into(),
                    status_rx: status_rx.clone(),
                    started_at,
                    session_id: runtime_session_id,
                },
                cx,
            );
        });

        // Transitions may have raced ahead of the insert (subscribe happens
        // before this task runs) — sync whatever the channel holds now.
        let mut last = AgentStatus::Idle;
        // The agent's last persisted title (its most recent prompt). Tracked so
        // we only write the DB when the prompt actually changes, not every tick.
        let mut last_title: Option<String> = None;
        // Likewise the last persisted assistant reply, so a restored session
        // keeps its finished-turn message across a restart.
        let mut last_msg: Option<String> = None;
        let (current, detail) = {
            let snap = status_rx.borrow_and_update();
            (snap.status.clone(), snap.detail.clone())
        };
        let title_changed =
            persist_title_if_changed(&agent_repo, &row_id, &detail, &mut last_title, cx).await;
        persist_message_if_changed(&agent_repo, &row_id, &detail, &mut last_msg, cx).await;
        let _ = weak.update(cx, |this, cx| {
            this.note_agent_sideband(&ws_key, &current, detail, cx);
            if title_changed {
                this.mark_rail_dirty(cx);
            }
        });
        if current != last {
            last = current.clone();
            write_status(&agent_repo, &row_id, &current, cx).await;
            let _ = weak.update(cx, |this, cx| this.mark_rail_dirty(cx));
        }
        while !last.is_terminal() {
            if status_rx.changed().await.is_err() {
                // Sender dropped without a terminal transition — the
                // runtime tore the session down mid-flight (e.g. tab close
                // aborting the poll task before its Exit event drains). By
                // definition the agent stopped without finishing, so write
                // the terminal truth here instead of leaving the row at
                // whatever non-terminal status it last had (`Running`
                // decays to `Idle` at the prompt, which the boot sweep
                // used to miss entirely).
                write_status(&agent_repo, &row_id, &AgentStatus::Interrupted, cx).await;
                let _ = weak.update(cx, |this, cx| {
                    this.note_agent_sideband(&ws_key, &AgentStatus::Interrupted, None, cx);
                    // Tab closed / session torn down — drop the live entry so
                    // the rail falls back to the (now Interrupted) history row.
                    this.remove_live_agent(&row_id, cx);
                });
                return;
            }
            let (status, detail) = {
                let snap = status_rx.borrow_and_update();
                (snap.status.clone(), snap.detail.clone())
            };
            let title_changed =
                persist_title_if_changed(&agent_repo, &row_id, &detail, &mut last_title, cx).await;
            persist_message_if_changed(&agent_repo, &row_id, &detail, &mut last_msg, cx).await;
            // The live tool subline can change WITHOUT a status edge (Bash →
            // Edit while still Running), so refresh it every tick — not only
            // when the status itself changes.
            let _ = weak.update(cx, |this, cx| {
                this.note_agent_sideband(&ws_key, &status, detail, cx);
                // A new prompt mid-Running has no status edge; nudge the rail so
                // the title updates even when the status itself stays put.
                if title_changed {
                    this.mark_rail_dirty(cx);
                }
            });
            if status == last {
                continue;
            }
            last = status.clone();
            write_status(&agent_repo, &row_id, &status, cx).await;
            // Event-driven rail repaint. Ignore failure: the root may be
            // gone (window closed, tab torn off) while the row write above
            // must still happen.
            let _ = weak.update(cx, |this, cx| this.mark_rail_dirty(cx));
        }

        // Terminal status reached. The session is now history (a persisted
        // row); drop the live entry so the rail shows the single history row
        // rather than a duplicate live + history pair.
        let _ = weak.update(cx, |this, cx| this.remove_live_agent(&row_id, cx));
    })
    .detach();
}

/// Persist the agent's title (its most recent prompt from the sideband) to the
/// session row, but only when it differs from the last write — the prompt rides
/// every snapshot once captured, so this would otherwise write every tick. A
/// blank/absent prompt is ignored (keeps the prior title). Best-effort: a write
/// failure is logged, never propagated.
///
/// Returns `true` when a new title was written, so the caller can repaint the
/// rail — a fresh prompt with no status edge would otherwise not refresh it.
async fn persist_title_if_changed(
    repo: &oximux_storage::AgentSessionRepo,
    row_id: &str,
    detail: &Option<oximux_core::SidebandDetail>,
    last_title: &mut Option<String>,
    cx: &gpui::AsyncApp,
) -> bool {
    let Some(title) = detail
        .as_ref()
        .and_then(|d| d.prompt.as_ref())
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
    else {
        return false;
    };
    if last_title.as_deref() == Some(title.as_str()) {
        return false;
    }
    *last_title = Some(title.clone());
    let repo = repo.clone();
    let id = row_id.to_string();
    cx.background_executor()
        .spawn(async move {
            if let Err(err) = repo.update_title(&id, &title) {
                tracing::warn!(?err, "agent persistence: title write failed");
            }
        })
        .await;
    true
}

/// Persist the agent's last assistant reply (the sideband's `last_message`,
/// captured on `Stop`) to the session row, but only when it differs from the
/// last write — the reply rides every snapshot once captured (the poll loop
/// preserves it across tool steps), so this would otherwise write every tick. A
/// blank/absent message is ignored (keeps the prior reply). Best-effort: a write
/// failure is logged, never propagated.
async fn persist_message_if_changed(
    repo: &oximux_storage::AgentSessionRepo,
    row_id: &str,
    detail: &Option<oximux_core::SidebandDetail>,
    last_msg: &mut Option<String>,
    cx: &gpui::AsyncApp,
) {
    let Some(message) = detail
        .as_ref()
        .and_then(|d| d.last_message.as_ref())
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    if last_msg.as_deref() == Some(message.as_str()) {
        return;
    }
    *last_msg = Some(message.clone());
    let repo = repo.clone();
    let id = row_id.to_string();
    cx.background_executor()
        .spawn(async move {
            if let Err(err) = repo.update_last_message(&id, &message) {
                tracing::warn!(?err, "agent persistence: last_message write failed");
            }
        })
        .await;
}

/// Write one status to the row (+ `ended_at` when terminal) on the
/// background executor. Failures are logged, never propagated — the badge
/// keeps working off the watch channel even when row history is lossy.
async fn write_status(
    repo: &oximux_storage::AgentSessionRepo,
    row_id: &str,
    status: &AgentStatus,
    cx: &gpui::AsyncApp,
) {
    let repo = repo.clone();
    let id = row_id.to_string();
    let status = status.clone();
    cx.background_executor()
        .spawn(async move {
            if let Err(err) = repo.update_status(&id, &status) {
                tracing::warn!(?err, "agent persistence: status write failed");
            }
            if status.is_terminal()
                && let Err(err) = repo.update_ended_at(&id)
            {
                tracing::warn!(?err, "agent persistence: ended_at write failed");
            }
        })
        .await;
}
