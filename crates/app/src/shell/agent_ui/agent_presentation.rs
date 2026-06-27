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
    /// "Stopped", "Idle", "Ready").
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
/// - `Interrupted`         → "Stopped" / `status_muted` (covers both a
///   user cancel and a session that was alive at shutdown — in either case
///   the agent stopped without finishing, through no fault of its own)
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
            label: "Stopped",
            color: theme.status_muted,
        },
    }
}

/// A live agent detected in a plain terminal: its status plus, when
/// recognizable, the agent's display name. Carried through the rail so a
/// hand-launched agent shows up by name on the card, not just by status.
///
/// Status comes from one of two sources, in order of preference:
///  1. the OSC-9999 hook sideband (`detail` is `Some`) — rich and stable,
///     identical to a spawned agent's status;
///  2. the OSC title heuristic (`detail` is `None`) — coarse but immediate,
///     so an agent that launched but hasn't emitted a hook still shows up.
#[derive(Clone, Debug, PartialEq)]
pub struct AmbientAgent {
    pub status: AgentStatus,
    /// Display name (e.g. "Claude Code"), or `None` when the source classifies
    /// as agent activity but the specific CLI couldn't be named.
    pub label: Option<&'static str>,
    /// Hook sideband detail (prompt title + live tool step) when the status
    /// came from a sideband packet; `None` for a title-only reading. Lets the
    /// rail show a hand-typed agent's prompt as its row title.
    pub detail: Option<oximux_core::SidebandDetail>,
}

/// Map a tracked-session adapter id (as stored in the agent-session row) to a
/// display name for the card. Unknown ids fall back to a generic label.
pub fn adapter_display_name(adapter_id: &str) -> &'static str {
    match adapter_id {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "aider" => "Aider",
        "gemini" => "Gemini CLI",
        _ => "Agent",
    }
}

/// Map an ambient terminal title's display label back to the registry adapter
/// slug used by icons and agent rows. Labels without dedicated first-class
/// support fall back to the generic agent slug.
pub fn adapter_id_for_label(label: &str) -> &'static str {
    match label {
        "Claude Code" => "claude-code",
        "Codex" => "codex",
        "Aider" => "aider",
        "Gemini CLI" => "gemini",
        _ => "agent",
    }
}

/// Bundled SVG glyph path for an agent's launcher icon, so a row or chip can
/// show the CLI's mark instead of a generic dot. Unknown ids fall back to the
/// generic sparkles glyph shared with the Agents nav entry.
pub fn adapter_icon_path(adapter_id: &str) -> &'static str {
    match adapter_id {
        "claude-code" => "icons/claude-code.svg",
        "codex" => "icons/codex.svg",
        "aider" => "icons/aider.svg",
        _ => "icons/sparkles.svg",
    }
}

/// Attention rank for an ambient (title-derived) status, used to pick the
/// strongest reading when several terminals/views in one worktree each
/// classify. Higher wins: a blocking prompt beats active work beats idle.
pub fn ambient_status_rank(status: &AgentStatus) -> u8 {
    match status {
        AgentStatus::NeedsApproval(_) | AgentStatus::WaitingForInput => 3,
        AgentStatus::Running => 2,
        AgentStatus::Idle => 1,
        _ => 0,
    }
}

/// Resolve the status a workspace card should show by combining its tracked
/// agent-session status (from the session store, possibly stale) with an
/// ambient status detected live from a plain terminal's title.
///
/// A tracked session that is currently *active* (working or blocking) stays
/// authoritative — its `StatusMachine` is the ground truth and must not be
/// shadowed by a hand-launched terminal in the same worktree. Otherwise the
/// live ambient reading (if any) replaces an absent, idle, or finished
/// tracked status, so a hand-typed agent surfaces instead of the last
/// session's stale "Done"/"Stopped".
pub fn resolve_effective_status(
    tracked: Option<AgentStatus>,
    ambient: Option<AgentStatus>,
) -> Option<AgentStatus> {
    let tracked_is_active = matches!(
        tracked,
        Some(AgentStatus::Running)
            | Some(AgentStatus::WaitingForInput)
            | Some(AgentStatus::NeedsApproval(_))
    );
    if tracked_is_active {
        tracked
    } else {
        ambient.or(tracked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::charcoal()
    }

    // ── effective-status resolution (tracked × ambient) ──────────────────────

    #[test]
    fn ambient_fills_when_no_tracked_session() {
        let eff = resolve_effective_status(None, Some(AgentStatus::Running));
        assert_eq!(eff, Some(AgentStatus::Running));
    }

    #[test]
    fn ambient_overrides_stale_terminal_tracked_status() {
        // The reported bug: a finished/cancelled session pins "Stopped" while
        // a hand-launched agent is live. Ambient must win.
        let eff =
            resolve_effective_status(Some(AgentStatus::Interrupted), Some(AgentStatus::Running));
        assert_eq!(eff, Some(AgentStatus::Running));
        let eff = resolve_effective_status(
            Some(AgentStatus::Done { code: Some(0) }),
            Some(AgentStatus::Idle),
        );
        assert_eq!(eff, Some(AgentStatus::Idle));
    }

    #[test]
    fn active_tracked_session_is_not_shadowed_by_ambient() {
        // A real spawned agent that is working stays authoritative.
        let eff = resolve_effective_status(Some(AgentStatus::Running), Some(AgentStatus::Idle));
        assert_eq!(eff, Some(AgentStatus::Running));
        let eff = resolve_effective_status(
            Some(AgentStatus::NeedsApproval("tool".into())),
            Some(AgentStatus::Running),
        );
        assert_eq!(eff, Some(AgentStatus::NeedsApproval("tool".into())));
    }

    #[test]
    fn no_ambient_keeps_tracked_status() {
        let eff = resolve_effective_status(Some(AgentStatus::Interrupted), None);
        assert_eq!(eff, Some(AgentStatus::Interrupted));
        assert_eq!(resolve_effective_status(None, None), None);
    }

    #[test]
    fn adapter_display_name_maps_known_ids() {
        assert_eq!(adapter_display_name("claude-code"), "Claude Code");
        assert_eq!(adapter_display_name("codex"), "Codex");
        assert_eq!(adapter_display_name("something-else"), "Agent");
    }

    #[test]
    fn adapter_id_for_label_maps_ambient_labels() {
        assert_eq!(adapter_id_for_label("Claude Code"), "claude-code");
        assert_eq!(adapter_id_for_label("Codex"), "codex");
        assert_eq!(adapter_id_for_label("Aider"), "aider");
        assert_eq!(adapter_id_for_label("Gemini CLI"), "gemini");
        assert_eq!(adapter_id_for_label("Unknown"), "agent");
    }

    #[test]
    fn adapter_icon_path_maps_known_ids_and_falls_back() {
        assert_eq!(adapter_icon_path("claude-code"), "icons/claude-code.svg");
        assert_eq!(adapter_icon_path("codex"), "icons/codex.svg");
        assert_eq!(adapter_icon_path("aider"), "icons/aider.svg");
        assert_eq!(adapter_icon_path("something-else"), "icons/sparkles.svg");
    }

    #[test]
    fn ambient_rank_orders_by_attention() {
        assert!(
            ambient_status_rank(&AgentStatus::NeedsApproval("x".into()))
                > ambient_status_rank(&AgentStatus::Running)
        );
        assert!(
            ambient_status_rank(&AgentStatus::Running) > ambient_status_rank(&AgentStatus::Idle)
        );
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
    fn interrupted_is_stopped_status_muted() {
        let v = agent_verb(Some(&AgentStatus::Interrupted), false, t());
        assert_eq!(v.label, "Stopped");
        assert_eq!(v.color, t().status_muted);
    }
}
