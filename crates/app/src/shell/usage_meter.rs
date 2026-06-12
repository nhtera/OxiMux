//! Status-bar usage meter — compact `~NN% 5h · ~NN% wk` chip + click
//! popover with window details.
//!
//! Data comes from `oximux_agents::session_log::usage_probe` on a 60 s
//! background tick owned by `WorkspaceRoot`. Everything here is rendering
//! plus pure formatting; the meter renders nothing at all when no snapshot
//! is available (no account config, unknown tier) — absent beats "N/A"
//! noise. All numbers are estimates from local session logs; every surface
//! carries the "~" prefix and the popover spells the caveat out.

use gpui::{Div, Hsla, ParentElement, Styled, div, px};
use oximux_agents::session_log::usage::{UsageSnapshot, UsageWindow};
use oximux_settings::{Density, Theme, Typography};

/// Compact status-bar label: `~63% 5h · ~41% wk`.
pub fn meter_label(snapshot: &UsageSnapshot) -> String {
    format!(
        "~{}% 5h · ~{}% wk",
        pct(snapshot.five_hour.ratio()),
        pct(snapshot.weekly.ratio())
    )
}

/// Meter color keyed to the hotter window: error ≥ 90 %, warn ≥ 70 %,
/// muted below — same vocabulary as the agent verb chips.
pub fn meter_color(snapshot: &UsageSnapshot, theme: Theme) -> Hsla {
    let worst = snapshot.five_hour.ratio().max(snapshot.weekly.ratio());
    if worst >= 0.9 {
        theme.status_error
    } else if worst >= 0.7 {
        theme.status_warn
    } else {
        theme.fg_muted
    }
}

fn pct(ratio: f32) -> u8 {
    (ratio * 100.0).round().clamp(0.0, 100.0) as u8
}

/// `"2h 05m"` / `"45m"` until a reset timestamp; `None` when already past.
pub fn format_reset_in(resets_at_ms: i64, now_ms: i64) -> Option<String> {
    let remaining = resets_at_ms - now_ms;
    if remaining <= 0 {
        return None;
    }
    let mins = remaining / 60_000;
    let (h, m) = (mins / 60, mins % 60);
    Some(if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    })
}

/// `139k` / `2.2M` / `980` — compact token counts for the popover.
pub fn format_tokens(tokens: f64) -> String {
    if tokens >= 1_000_000.0 {
        format!("{:.1}M", tokens / 1_000_000.0)
    } else if tokens >= 1_000.0 {
        format!("{:.0}k", tokens / 1_000.0)
    } else {
        format!("{:.0}", tokens)
    }
}

fn window_line(window: &UsageWindow, name: &str, reset: String) -> String {
    format!(
        "{name}: ~{}%  ({} / {} tokens) · {reset}",
        pct(window.ratio()),
        format_tokens(window.used_tokens),
        format_tokens(window.budget_tokens),
    )
}

/// The popover card body: one line per window + the estimate disclosure.
/// The caller owns positioning + the click-away backdrop.
pub fn render_usage_popover(
    snapshot: &UsageSnapshot,
    now_ms: i64,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    let five_reset = snapshot
        .five_hour
        .resets_at_ms
        .and_then(|t| format_reset_in(t, now_ms))
        .map(|in_| format!("resets in {in_}"))
        .unwrap_or_else(|| "no active block".to_string());
    let five_line = window_line(&snapshot.five_hour, "5-hour block", five_reset);
    let weekly_line = window_line(&snapshot.weekly, "Weekly", "rolling 7 days".to_string());

    div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline))
        .p(px(density.pad_panel))
        .rounded(px(density.r_card))
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_active)
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child("Agent usage (estimated)"),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(meter_color(snapshot, theme))
                .child(five_line),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(weekly_line),
        )
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(format!(
                    "Estimated from local session logs · tier {}",
                    snapshot.tier
                )),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(five_ratio: f64, weekly_ratio: f64) -> UsageSnapshot {
        UsageSnapshot {
            five_hour: UsageWindow {
                used_tokens: five_ratio * 1000.0,
                budget_tokens: 1000.0,
                resets_at_ms: Some(10_000_000),
            },
            weekly: UsageWindow {
                used_tokens: weekly_ratio * 10_000.0,
                budget_tokens: 10_000.0,
                resets_at_ms: None,
            },
            tier: "default_claude_max_5x".to_string(),
        }
    }

    #[test]
    fn meter_label_formats_both_windows() {
        assert_eq!(meter_label(&snapshot(0.63, 0.41)), "~63% 5h · ~41% wk");
    }

    #[test]
    fn meter_label_caps_at_hundred() {
        assert_eq!(meter_label(&snapshot(2.0, 0.0)), "~100% 5h · ~0% wk");
    }

    #[test]
    fn meter_color_tiers() {
        let t = Theme::charcoal();
        assert_eq!(meter_color(&snapshot(0.2, 0.3), t), t.fg_muted);
        assert_eq!(meter_color(&snapshot(0.75, 0.1), t), t.status_warn);
        assert_eq!(meter_color(&snapshot(0.1, 0.95), t), t.status_error);
    }

    #[test]
    fn format_reset_in_hours_and_minutes() {
        assert_eq!(format_reset_in(7_500_000, 0).unwrap(), "2h 05m");
        assert_eq!(format_reset_in(2_700_000, 0).unwrap(), "45m");
        assert!(format_reset_in(100, 200).is_none());
    }

    #[test]
    fn format_tokens_scales() {
        assert_eq!(format_tokens(980.0), "980");
        assert_eq!(format_tokens(139_400.0), "139k");
        assert_eq!(format_tokens(2_200_000.0), "2.2M");
    }
}
