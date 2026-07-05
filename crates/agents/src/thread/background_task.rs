//! Background tasks: subagents (the `Agent` tool) and background bash commands
//! (`Bash` with `run_in_background`) the CLI runs asynchronously within a turn.
//!
//! The stream surfaces these as first-class `system/task_*` lifecycle events
//! (started / progress / updated / notification), keyed by a `task_id` and linked
//! to their spawning tool card via `tool_use_id`. This module models the folded
//! state the Background Tasks panel renders. Pure data + serde — no gpui.
//!
//! Note on the wire: `task_progress.usage` is empirically null and `task_started`
//! carries no start timestamp, so per-task token totals and precise durations are
//! not derived here (the panel shows status, tool activity, and the completion
//! summary instead). The completion `end_time` is kept for a finished-at readout.

use serde::{Deserialize, Serialize};

/// What kind of background work a task represents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackgroundTaskKind {
    /// A subagent launched via the `Agent` tool (`subagent_type` names the agent).
    Subagent { subagent_type: String },
    /// A `Bash` tool call with `run_in_background: true`.
    BackgroundBash,
}

impl BackgroundTaskKind {
    /// Map the wire `task_type` (+ optional `subagent_type`) to a kind. Anything
    /// that isn't a background bash is treated as a subagent (the richer surface),
    /// so a future `task_type` still renders something sensible.
    pub fn from_wire(task_type: &str, subagent_type: Option<&str>) -> Self {
        match task_type {
            "local_bash" => BackgroundTaskKind::BackgroundBash,
            _ => BackgroundTaskKind::Subagent {
                subagent_type: subagent_type.filter(|s| !s.is_empty()).unwrap_or("agent").to_string(),
            },
        }
    }
}

/// Terminal + in-flight status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
}

/// One background task, folded from its `system/task_*` events. `tool_use_id`
/// links it to its tool card in the main transcript; `task_id` keys the lifecycle
/// events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub task_id: String,
    pub tool_use_id: String,
    pub kind: BackgroundTaskKind,
    pub description: String,
    pub status: TaskStatus,
    /// Progress steps observed so far — a proxy for the task's tool-use count.
    pub tool_uses: u32,
    /// The tool the task most recently ran (`task_progress.last_tool_name`).
    pub last_tool: Option<String>,
    /// Wall-clock completion (ms since epoch, from the completion patch), for a
    /// finished-at readout.
    pub ended_at_ms: Option<u64>,
    /// One-line completion summary (`task_notification.summary`).
    pub summary: Option<String>,
    /// Path to the task's own output — bash stdout or the subagent's transcript —
    /// used by "View transcript".
    pub output_file: Option<String>,
}

impl BackgroundTask {
    /// Create a Running task from a `task_started` event.
    pub fn started(
        task_id: String,
        tool_use_id: String,
        kind: BackgroundTaskKind,
        description: String,
    ) -> Self {
        Self {
            task_id,
            tool_use_id,
            kind,
            description,
            status: TaskStatus::Running,
            tool_uses: 0,
            last_tool: None,
            ended_at_ms: None,
            summary: None,
            output_file: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.status == TaskStatus::Running
    }

    /// A short human label for the kind — the panel row's leading tag.
    pub fn kind_label(&self) -> &str {
        match &self.kind {
            BackgroundTaskKind::Subagent { subagent_type } => subagent_type,
            BackgroundTaskKind::BackgroundBash => "bash",
        }
    }

    /// Whether a transcript / output is available to open (a finished task with a
    /// recorded output file).
    pub fn has_transcript(&self) -> bool {
        self.output_file.as_ref().is_some_and(|f| !f.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_wire_maps_bash_and_agent() {
        assert_eq!(
            BackgroundTaskKind::from_wire("local_bash", None),
            BackgroundTaskKind::BackgroundBash
        );
        assert_eq!(
            BackgroundTaskKind::from_wire("local_agent", Some("general-purpose")),
            BackgroundTaskKind::Subagent { subagent_type: "general-purpose".into() }
        );
    }

    #[test]
    fn kind_from_wire_unknown_defaults_to_subagent() {
        assert_eq!(
            BackgroundTaskKind::from_wire("future_kind", None),
            BackgroundTaskKind::Subagent { subagent_type: "agent".into() }
        );
    }

    #[test]
    fn started_task_is_running_with_zeroed_counters() {
        let t = BackgroundTask::started(
            "tid".into(),
            "toolu_x".into(),
            BackgroundTaskKind::BackgroundBash,
            "sleep".into(),
        );
        assert!(t.is_running());
        assert_eq!(t.tool_uses, 0);
        assert!(!t.has_transcript());
        assert_eq!(t.kind_label(), "bash");
    }
}
