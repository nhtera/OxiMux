//! Status-bar usage meter — compact `NN% 5h · NN% wk` chip + click
//! popover with window details.
//!
//! Data comes from `oximux_agents::session_log::usage_probe` on a 60 s
//! background tick owned by `WorkspaceRoot`. Everything here is rendering
//! plus pure formatting; the meter renders nothing at all when no snapshot
//! is available (no account config, unknown tier) — absent beats "N/A"
//! noise. Presentation follows the snapshot source: account-API numbers
//! are exact and render plainly; local-tally numbers are estimates and
//! every surface carries the "~" prefix plus a popover caveat.

use gpui::{Div, Hsla, ParentElement, Styled, div, px};
use oximux_agents::session_log::usage::{UsageSnapshot, UsageSource, UsageWindow};
use oximux_settings::{Density, Theme, Typography};

/// Compact status-bar label: `12% 5h · 4% wk` (exact) or
/// `~63% 5h · ~41% wk` (estimate).
pub fn meter_label(snapshot: &UsageSnapshot) -> String {
    let approx = approx_prefix(snapshot);
    format!(
        "{approx}{}% 5h · {approx}{}% wk",
        pct(snapshot.five_hour.ratio()),
        pct(snapshot.weekly.ratio())
    )
}

fn approx_prefix(snapshot: &UsageSnapshot) -> &'static str {
    match snapshot.source {
        UsageSource::AccountApi => "",
        UsageSource::LocalEstimate => "~",
    }
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

/// `"6d 21h"` / `"2h 05m"` / `"45m"` until a reset timestamp; `None` when
/// already past. Days appear once the span exceeds 48 h (weekly resets).
pub fn format_reset_in(resets_at_ms: i64, now_ms: i64) -> Option<String> {
    let remaining = resets_at_ms - now_ms;
    if remaining <= 0 {
        return None;
    }
    let mins = remaining / 60_000;
    let (h, m) = (mins / 60, mins % 60);
    Some(if h >= 48 {
        format!("{}d {}h", h / 24, h % 24)
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    })
}

/// `"just now"` / `"5m ago"` / `"2h ago"` — coarse age of a cached reading,
/// matching the reference cockpit's freshness wording.
pub fn format_time_ago(captured_at_ms: i64, now_ms: i64) -> String {
    let diff = now_ms - captured_at_ms;
    if diff < 60_000 {
        return "just now".to_string();
    }
    let mins = diff / 60_000;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    format!("{}h ago", mins / 60)
}

/// Popover title + footer, derived from the snapshot source and freshness.
/// A cached exact reading (live fetch unavailable this tick) keeps the
/// numbers visible but discloses "updated N ago" rather than passing the
/// reading off as live — the same freshness contract the reference cockpit
/// uses for its stale bars.
fn popover_caption(snapshot: &UsageSnapshot, now_ms: i64) -> (&'static str, String) {
    let tier_suffix = if snapshot.tier.is_empty() {
        String::new()
    } else {
        format!(" · tier {}", snapshot.tier)
    };
    match (snapshot.source, snapshot.captured_at_ms) {
        (UsageSource::AccountApi, Some(captured)) => (
            "Agent usage",
            format!(
                "Showing cached usage · updated {}{tier_suffix}",
                format_time_ago(captured, now_ms)
            ),
        ),
        (UsageSource::AccountApi, None) => {
            ("Agent usage", format!("From the account usage API{tier_suffix}"))
        }
        (UsageSource::LocalEstimate, _) => (
            "Agent usage (estimated)",
            format!("Estimated from local session logs{tier_suffix}"),
        ),
    }
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

/// One popover line. Exact windows show the percentage plainly; estimate
/// windows keep the "~" and disclose the token math behind the guess.
fn window_line(window: &UsageWindow, name: &str, reset: String, source: UsageSource) -> String {
    match source {
        UsageSource::AccountApi => {
            format!("{name}: {}% used · {reset}", pct(window.ratio()))
        }
        UsageSource::LocalEstimate => format!(
            "{name}: ~{}%  ({} / {} tokens) · {reset}",
            pct(window.ratio()),
            format_tokens(window.used_tokens),
            format_tokens(window.budget_tokens),
        ),
    }
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
    let five_line = window_line(
        &snapshot.five_hour,
        "5-hour block",
        five_reset,
        snapshot.source,
    );
    // The account API knows the real weekly reset; the local estimate can
    // only describe its rolling window.
    let weekly_reset = snapshot
        .weekly
        .resets_at_ms
        .and_then(|t| format_reset_in(t, now_ms))
        .map(|in_| format!("resets in {in_}"))
        .unwrap_or_else(|| "rolling 7 days".to_string());
    let weekly_line = window_line(&snapshot.weekly, "Weekly", weekly_reset, snapshot.source);
    let (title, footer) = popover_caption(snapshot, now_ms);

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
                .child(title),
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
                .child(footer),
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
            source: UsageSource::LocalEstimate,
            captured_at_ms: None,
        }
    }

    fn exact_snapshot(five_pct: f64, weekly_pct: f64) -> UsageSnapshot {
        UsageSnapshot {
            five_hour: UsageWindow {
                used_tokens: five_pct,
                budget_tokens: 100.0,
                resets_at_ms: Some(10_000_000),
            },
            weekly: UsageWindow {
                used_tokens: weekly_pct,
                budget_tokens: 100.0,
                resets_at_ms: Some(600_000_000),
            },
            tier: "default_claude_max_5x".to_string(),
            source: UsageSource::AccountApi,
            captured_at_ms: None,
        }
    }

    fn stale_exact_snapshot(captured_at_ms: i64) -> UsageSnapshot {
        let mut s = exact_snapshot(26.0, 27.0);
        s.captured_at_ms = Some(captured_at_ms);
        s
    }

    #[test]
    fn format_time_ago_buckets() {
        assert_eq!(format_time_ago(1_000_000, 1_030_000), "just now"); // 30s
        assert_eq!(format_time_ago(1_000_000, 1_300_000), "5m ago"); // 5m
        assert_eq!(format_time_ago(1_000_000, 1_000_000 + 2 * 3_600_000), "2h ago");
    }

    #[test]
    fn popover_caption_discloses_cached_reading() {
        // Fresh exact → plain account-API caption.
        let (title, footer) = popover_caption(&exact_snapshot(26.0, 27.0), 0);
        assert_eq!(title, "Agent usage");
        assert_eq!(footer, "From the account usage API · tier default_claude_max_5x");

        // Cached exact (captured 5m ago) → discloses staleness, keeps numbers.
        let now = 1_000_000_000;
        let (title, footer) = popover_caption(&stale_exact_snapshot(now - 300_000), now);
        assert_eq!(title, "Agent usage");
        assert_eq!(
            footer,
            "Showing cached usage · updated 5m ago · tier default_claude_max_5x"
        );

        // Estimate keeps its own caption.
        let (title, footer) = popover_caption(&snapshot(0.6, 0.4), 0);
        assert_eq!(title, "Agent usage (estimated)");
        assert_eq!(footer, "Estimated from local session logs · tier default_claude_max_5x");
    }

    #[test]
    fn meter_label_exact_source_drops_tilde() {
        assert_eq!(meter_label(&exact_snapshot(12.0, 4.0)), "12% 5h · 4% wk");
    }

    #[test]
    fn window_line_exact_source_skips_token_counts() {
        let snap = exact_snapshot(12.0, 4.0);
        let line = window_line(
            &snap.five_hour,
            "5-hour block",
            "resets in 4h 33m".to_string(),
            snap.source,
        );
        assert_eq!(line, "5-hour block: 12% used · resets in 4h 33m");
    }

    #[test]
    fn format_reset_in_shows_days_for_long_spans() {
        // 6 days 21 hours out.
        let span_ms = (6 * 24 + 21) * 3_600_000_i64 + 5 * 60_000;
        assert_eq!(format_reset_in(span_ms, 0).as_deref(), Some("6d 21h"));
        assert_eq!(format_reset_in(3_900_000, 0).as_deref(), Some("1h 05m"));
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
