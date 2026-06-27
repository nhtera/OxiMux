//! Agent status badge — colored dot prepended to agent tab labels.
//!
//! Renders a 6 px solid disc whose color reflects the live `AgentStatus`
//! piped through `WorkspaceTabKind::Agent { status_rx }`. The dot is the
//! single visual cue separating an agent tab from a terminal tab; terminal
//! tabs never carry one. Color mapping uses tokens from `Theme`'s status
//! group so theme switching (Phase 6) recolors the badge without code edits.
//!
//! Payload-bearing variants (`NeedsApproval(reason)`, `Failed(message)`,
//! `Done { code: Some(_) }`) attach a tooltip so the user can read the
//! reason / exit code on hover without scrolling the PTY buffer. Variants
//! whose state is fully described by color alone (`Idle`, `Running`,
//! `WaitingForInput`, clean `Done { code: Some(0) }`, signal-killed
//! `Done { code: None }`) carry no tooltip.

use gpui::{
    ElementId, Hsla, InteractiveElement, IntoElement, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::tooltip::Tooltip;
use oximux_core::AgentStatus;
use oximux_settings::Theme;

const DOT_SIZE: f32 = 6.0;

/// Maximum tooltip text length. The payload strings come from agent PTY
/// output (`NeedsApproval`) or runtime error messages (`Failed`); truncate
/// so a multi-kilobyte stack trace can't blow up the hover popup.
const TOOLTIP_MAX_LEN: usize = 80;

/// Map an `AgentStatus` to its dot color via theme tokens.
///
/// - `Idle` and signal-killed `Done { code: None }` both render in
///   `status_muted` — both are "nothing actionable here", and the user can
///   distinguish them by whether the tab is still alive.
/// - `WaitingForInput` and `NeedsApproval` share `status_warn` (amber). The
///   step-12 notifier distinguishes them — only `NeedsApproval` pings macOS
///   notifications. In the strip, the tooltip carries the differentiation.
pub fn status_color(status: &AgentStatus, theme: Theme) -> Hsla {
    match status {
        AgentStatus::Idle => theme.status_muted,
        AgentStatus::Running => theme.focus_ring,
        AgentStatus::WaitingForInput => theme.status_warn,
        AgentStatus::NeedsApproval(_) => theme.status_warn,
        AgentStatus::Done { code: Some(0) } => theme.status_ok,
        AgentStatus::Done { code: Some(_) } => theme.status_error,
        AgentStatus::Done { code: None } => theme.status_muted,
        AgentStatus::Failed(_) => theme.status_error,
        // Stopped without finishing — user cancel, or alive at shutdown and
        // not resumed. Deliberate or environmental, not a failure: muted,
        // matching the "Stopped" verb on the rail/dashboard rows.
        AgentStatus::Interrupted => theme.status_muted,
    }
}

/// Truncate at character boundary. Avoids splitting a multi-byte UTF-8
/// codepoint mid-byte when the payload contains non-ASCII characters
/// (an agent might emit emoji or accented locale strings).
fn truncate_for_tooltip(s: &str) -> String {
    if s.chars().count() <= TOOLTIP_MAX_LEN {
        return s.to_string();
    }
    let mut out: String = s.chars().take(TOOLTIP_MAX_LEN.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Build the tooltip string for a status, or `None` when the variant
/// carries no payload worth surfacing on hover. Pure helper — testable
/// without any GPUI harness.
pub fn tooltip_for(status: &AgentStatus) -> Option<String> {
    match status {
        AgentStatus::NeedsApproval(reason) => {
            Some(format!("Needs approval: {}", truncate_for_tooltip(reason)))
        }
        AgentStatus::Failed(msg) => Some(format!("Failed: {}", truncate_for_tooltip(msg))),
        AgentStatus::Done { code: Some(c) } if *c != 0 => Some(format!("Exited with code {c}")),
        AgentStatus::Interrupted => Some("Stopped before finishing".to_string()),
        _ => None,
    }
}

/// Render the colored dot for one agent tab. Returns a 6 px disc;
/// terminal tabs do not call this path.
///
/// `id` must be stable across renders for the same tab so GPUI's tooltip
/// machinery can track hover state. The call site uses the tab index
/// (`"agent-status-dot-{ix}"`) — re-ordering tabs preserves identity by
/// position, which matches the tab strip's mental model.
pub fn render_dot(
    id: impl Into<ElementId>,
    status: &AgentStatus,
    theme: Theme,
) -> impl IntoElement {
    let color = status_color(status, theme);
    let tip = tooltip_for(status);
    div()
        .id(id.into())
        .w(px(DOT_SIZE))
        .h(px(DOT_SIZE))
        .rounded_full()
        .bg(color)
        .flex_shrink_0()
        .when_some(tip, |this, text| {
            this.tooltip(move |window: &mut Window, cx| {
                Tooltip::new(text.clone()).build(window, cx)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::charcoal()
    }

    #[test]
    fn idle_uses_status_muted() {
        assert_eq!(status_color(&AgentStatus::Idle, t()), t().status_muted);
    }

    #[test]
    fn running_uses_focus_ring() {
        assert_eq!(status_color(&AgentStatus::Running, t()), t().focus_ring);
    }

    #[test]
    fn waiting_for_input_uses_status_warn() {
        assert_eq!(
            status_color(&AgentStatus::WaitingForInput, t()),
            t().status_warn
        );
    }

    #[test]
    fn needs_approval_uses_status_warn() {
        let s = AgentStatus::NeedsApproval("write /etc/hosts?".into());
        assert_eq!(status_color(&s, t()), t().status_warn);
    }

    #[test]
    fn done_zero_uses_status_ok() {
        let s = AgentStatus::Done { code: Some(0) };
        assert_eq!(status_color(&s, t()), t().status_ok);
    }

    #[test]
    fn done_nonzero_uses_status_error() {
        let s = AgentStatus::Done { code: Some(1) };
        assert_eq!(status_color(&s, t()), t().status_error);
    }

    #[test]
    fn done_signal_killed_uses_status_muted() {
        let s = AgentStatus::Done { code: None };
        assert_eq!(status_color(&s, t()), t().status_muted);
    }

    #[test]
    fn failed_uses_status_error() {
        let s = AgentStatus::Failed("spawn refused".into());
        assert_eq!(status_color(&s, t()), t().status_error);
    }

    #[test]
    fn interrupted_uses_status_muted() {
        assert_eq!(
            status_color(&AgentStatus::Interrupted, t()),
            t().status_muted
        );
    }

    #[test]
    fn tooltip_for_interrupted_says_stopped() {
        let tip = tooltip_for(&AgentStatus::Interrupted).expect("tooltip");
        assert!(tip.contains("Stopped"));
    }

    #[test]
    fn tooltip_for_idle_is_none() {
        assert!(tooltip_for(&AgentStatus::Idle).is_none());
    }

    #[test]
    fn tooltip_for_running_is_none() {
        assert!(tooltip_for(&AgentStatus::Running).is_none());
    }

    #[test]
    fn tooltip_for_waiting_for_input_is_none() {
        // Generic prompt — no payload to surface.
        assert!(tooltip_for(&AgentStatus::WaitingForInput).is_none());
    }

    #[test]
    fn tooltip_for_needs_approval_includes_reason() {
        let s = AgentStatus::NeedsApproval("delete db".into());
        let tip = tooltip_for(&s).expect("payload variant must produce tooltip");
        assert!(tip.starts_with("Needs approval:"));
        assert!(tip.contains("delete db"));
    }

    #[test]
    fn tooltip_for_failed_includes_message() {
        let s = AgentStatus::Failed("PATH lookup failed".into());
        let tip = tooltip_for(&s).expect("payload variant must produce tooltip");
        assert!(tip.starts_with("Failed:"));
        assert!(tip.contains("PATH lookup failed"));
    }

    #[test]
    fn tooltip_for_clean_done_is_none() {
        let s = AgentStatus::Done { code: Some(0) };
        assert!(tooltip_for(&s).is_none());
    }

    #[test]
    fn tooltip_for_failing_done_shows_exit_code() {
        let s = AgentStatus::Done { code: Some(137) };
        let tip = tooltip_for(&s).expect("non-zero exit must produce tooltip");
        assert!(tip.contains("137"));
    }

    #[test]
    fn tooltip_for_signal_killed_done_is_none() {
        let s = AgentStatus::Done { code: None };
        assert!(tooltip_for(&s).is_none());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let long: String = "é".repeat(200);
        let out = truncate_for_tooltip(&long);
        // Must end with the ellipsis and never panic on a multi-byte boundary.
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= TOOLTIP_MAX_LEN);
    }

    #[test]
    fn truncate_short_strings_passthrough() {
        assert_eq!(truncate_for_tooltip("short"), "short");
    }
}
