//! Shared agent-state presentation helpers.
//!
//! Single source of truth for mapping an `AgentStatus` + `is_live` flag to a
//! human-readable verb label and a theme-token color. Both the left-rail status
//! dot and the rich-card agent-verb line delegate here so their output is always
//! in sync — updating this one function propagates to every surface.
//!
//! Scope note: this module covers rail-dot semantics only. The tab-strip badge
//! (`agent_status_badge`) uses a different color mapping (Idle→muted,
//! Running→focus_ring) — that distinction is intentional and out of scope here.

use gpui::Hsla;
use oximux_core::AgentStatus;
use oximux_settings::Theme;

/// A resolved verb label + color for one agent state. Produced by
/// `agent_verb` and consumed by the card painter and `status_dot_color`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentVerb {
    /// Short human-readable verb for the current state ("Running",
    /// "Waiting for input", "Needs approval", "Done", "Failed",
    /// "Interrupted", "Idle", "Ready").
    pub label: &'static str,
    /// Status-token color matching the verb — same mapping as the rail dot.
    pub color: Hsla,
}

/// Resolve the agent verb + color for a workspace given its latest agent
/// session status (or `None` when no sessions have ever started) and whether
/// the workspace currently has a live (open) agent tab.
///
/// Semantics (mirrors `status_dot_color` exactly — they are kept in lock-step
/// by delegating dot color to this function):
///
/// - No session / `Idle` + live  → "Ready" / `status_ok`
/// - No session / `Idle` + not live → "Idle" / `fg_subtle`
/// - `Running`             → "Running" / `status_info`
/// - `WaitingForInput`     → "Waiting for input" / `status_warn`
/// - `NeedsApproval`       → "Needs approval" / `status_warn`
/// - `Done { code: 0 }`    → "Done" / `status_ok`
/// - `Done { code != 0 }` or `Failed` → "Failed" / `status_error`
/// - `Interrupted`         → "Interrupted" / `status_muted`
pub fn agent_verb(status: Option<&AgentStatus>, is_live: bool, theme: Theme) -> AgentVerb {
    match status {
        None | Some(AgentStatus::Idle) => {
            if is_live {
                AgentVerb {
                    label: "Ready",
                    color: theme.status_ok,
                }
            } else {
                AgentVerb {
                    label: "Idle",
                    color: theme.fg_subtle,
                }
            }
        }
        Some(AgentStatus::Running) => AgentVerb {
            label: "Running",
            color: theme.status_info,
        },
        Some(AgentStatus::WaitingForInput) => AgentVerb {
            label: "Waiting for input",
            color: theme.status_warn,
        },
        Some(AgentStatus::NeedsApproval(_)) => AgentVerb {
            label: "Needs approval",
            color: theme.status_warn,
        },
        Some(AgentStatus::Done { code: Some(0) }) => AgentVerb {
            label: "Done",
            color: theme.status_ok,
        },
        Some(AgentStatus::Done { .. }) => AgentVerb {
            label: "Failed",
            color: theme.status_error,
        },
        Some(AgentStatus::Failed(_)) => AgentVerb {
            label: "Failed",
            color: theme.status_error,
        },
        Some(AgentStatus::Interrupted) => AgentVerb {
            label: "Interrupted",
            color: theme.status_muted,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::charcoal()
    }

    // ── dot-color parity: each case must match status_dot_color exactly ──────

    #[test]
    fn none_not_live_is_idle_fg_subtle() {
        let v = agent_verb(None, false, t());
        assert_eq!(v.label, "Idle");
        assert_eq!(v.color, t().fg_subtle);
    }

    #[test]
    fn none_live_is_ready_status_ok() {
        let v = agent_verb(None, true, t());
        assert_eq!(v.label, "Ready");
        assert_eq!(v.color, t().status_ok);
    }

    #[test]
    fn idle_not_live_is_idle_fg_subtle() {
        let v = agent_verb(Some(&AgentStatus::Idle), false, t());
        assert_eq!(v.label, "Idle");
        assert_eq!(v.color, t().fg_subtle);
    }

    #[test]
    fn idle_live_is_ready_status_ok() {
        let v = agent_verb(Some(&AgentStatus::Idle), true, t());
        assert_eq!(v.label, "Ready");
        assert_eq!(v.color, t().status_ok);
    }

    #[test]
    fn running_wins_over_live_flag() {
        // A concrete status always overrides the live flag.
        let v = agent_verb(Some(&AgentStatus::Running), true, t());
        assert_eq!(v.label, "Running");
        assert_eq!(v.color, t().status_info);
    }

    #[test]
    fn running_not_live_is_status_info() {
        let v = agent_verb(Some(&AgentStatus::Running), false, t());
        assert_eq!(v.label, "Running");
        assert_eq!(v.color, t().status_info);
    }

    #[test]
    fn waiting_for_input_is_status_warn() {
        let v = agent_verb(Some(&AgentStatus::WaitingForInput), false, t());
        assert_eq!(v.label, "Waiting for input");
        assert_eq!(v.color, t().status_warn);
    }

    #[test]
    fn needs_approval_is_status_warn() {
        let v = agent_verb(
            Some(&AgentStatus::NeedsApproval("permission".into())),
            false,
            t(),
        );
        assert_eq!(v.label, "Needs approval");
        assert_eq!(v.color, t().status_warn);
    }

    #[test]
    fn done_clean_is_status_ok() {
        let v = agent_verb(Some(&AgentStatus::Done { code: Some(0) }), false, t());
        assert_eq!(v.label, "Done");
        assert_eq!(v.color, t().status_ok);
    }

    #[test]
    fn done_nonzero_is_status_error() {
        let v = agent_verb(Some(&AgentStatus::Done { code: Some(1) }), false, t());
        assert_eq!(v.label, "Failed");
        assert_eq!(v.color, t().status_error);
    }

    #[test]
    fn done_unknown_code_is_status_error() {
        let v = agent_verb(Some(&AgentStatus::Done { code: None }), false, t());
        assert_eq!(v.label, "Failed");
        assert_eq!(v.color, t().status_error);
    }

    #[test]
    fn failed_is_status_error() {
        let v = agent_verb(Some(&AgentStatus::Failed("boom".into())), false, t());
        assert_eq!(v.label, "Failed");
        assert_eq!(v.color, t().status_error);
    }

    #[test]
    fn interrupted_is_status_muted() {
        let v = agent_verb(Some(&AgentStatus::Interrupted), false, t());
        assert_eq!(v.label, "Interrupted");
        assert_eq!(v.color, t().status_muted);
    }
}
