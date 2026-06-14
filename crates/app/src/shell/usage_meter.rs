//! Status-bar usage meter — compact `NN% 5h · NN% wk` chip + click popover
//! with window details, or an "unavailable" chip + reason when the account
//! usage API can't be reached.
//!
//! Data comes from `oximux_agents::session_log::usage_probe` on a 60 s
//! background tick owned by `WorkspaceRoot`. Everything here is rendering
//! plus pure formatting. The numbers are exact (the account usage API), so
//! they render plainly; a *cached* reading (live fetch down this tick) keeps
//! the numbers but discloses "updated N ago".

use gpui::{Div, Hsla, ParentElement, Styled, div, px, relative};
use gpui_component::Icon;
use oximux_agents::session_log::usage::{UsageSnapshot, UsageState, UsageWindow};
use oximux_settings::{Density, Theme, Typography};

/// Status-bar label for the unavailable state.
pub const UNAVAILABLE_LABEL: &str = "Usage unavailable";

/// Compact status-bar label: `12% 5h · 4% wk`.
pub fn meter_label(snapshot: &UsageSnapshot) -> String {
    format!(
        "{}% 5h · {}% wk",
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

/// Popover footer text, derived from freshness. A cached reading (live fetch
/// unavailable this tick) discloses "updated N ago" rather than passing a
/// slightly-old reading off as live — the same freshness contract the
/// reference cockpit uses for its stale bars.
fn popover_footer(snapshot: &UsageSnapshot, now_ms: i64) -> String {
    let pretty = pretty_tier(&snapshot.tier);
    let tier_suffix = if pretty.is_empty() {
        String::new()
    } else {
        format!(" · {pretty}")
    };
    match snapshot.captured_at_ms {
        Some(captured) => format!(
            "Cached · updated {}{tier_suffix}",
            format_time_ago(captured, now_ms)
        ),
        None => format!("Account usage API{tier_suffix}"),
    }
}

/// A readable plan label from the raw tier slug (`default_claude_max_5x` →
/// `Max 5x`); unknown slugs pass through unchanged, empty stays empty.
fn pretty_tier(tier: &str) -> String {
    if tier.is_empty() {
        String::new()
    } else if tier.contains("max_20x") {
        "Max 20x".to_string()
    } else if tier.contains("max_5x") {
        "Max 5x".to_string()
    } else if tier.contains("pro") {
        "Pro".to_string()
    } else {
        tier.to_string()
    }
}

/// Fraction of a window still available (0–1) — the bar fill and the
/// `NN% left` label both read from this.
fn left_fraction(window: &UsageWindow) -> f32 {
    (1.0 - window.ratio()).clamp(0.0, 1.0)
}

/// Bar / label color keyed to headroom — green with room, amber tightening,
/// red near the ceiling (mirrors the reference cockpit's remaining-based
/// coloring).
fn headroom_color(left: f32, theme: Theme) -> Hsla {
    if left >= 0.4 {
        theme.status_ok
    } else if left >= 0.2 {
        theme.status_warn
    } else {
        theme.status_error
    }
}

/// `Updated just now` / `Updated 5m ago` freshness line under the header.
fn freshness_text(snapshot: &UsageSnapshot, now_ms: i64) -> String {
    match snapshot.captured_at_ms {
        Some(captured) => format!("Updated {}", format_time_ago(captured, now_ms)),
        None => "Updated just now".to_string(),
    }
}

/// The popover card: a provider-style header, a progress bar per window with
/// its headroom + reset, and a freshness/plan footer — or the failure reason
/// when no reading is available. The caller owns positioning + dismissal.
pub fn render_usage_popover(
    state: &UsageState,
    now_ms: i64,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    // Fills the host popup window (sized to fit in `usage_popover::open`); the
    // panel is borderless + transparent, so the card's panel background + radius
    // is what the user sees.
    let card = div()
        .flex()
        .flex_col()
        .size_full()
        .gap(px(density.gap_inline * 1.5))
        .p(px(density.pad_panel))
        .rounded(px(density.r_card))
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_active);

    match state {
        UsageState::Available(snapshot) => card
            .child(header(
                Some(freshness_text(snapshot, now_ms)),
                theme,
                density,
                typography,
            ))
            .child(divider(theme))
            .child(window_block(
                "Session",
                &snapshot.five_hour,
                now_ms,
                "no active block",
                theme,
                density,
                typography,
            ))
            .child(window_block(
                "Weekly",
                &snapshot.weekly,
                now_ms,
                "rolling 7 days",
                theme,
                density,
                typography,
            ))
            .child(divider(theme))
            .child(footer(popover_footer(snapshot, now_ms), theme, typography)),
        UsageState::Unavailable { reason } => card
            .child(header(None, theme, density, typography))
            .child(divider(theme))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_base)
                    .child("Unavailable"),
            )
            .child(footer(reason.clone(), theme, typography)),
    }
}

/// Provider-style header: a gauge icon + "Agent usage", with an optional
/// freshness subtitle (the available state's "Updated N ago").
fn header(
    subtitle: Option<String>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline * 0.4))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(density.gap_inline * 0.6))
                .text_color(theme.fg_base)
                .child(
                    Icon::default()
                        .path("icons/claude-code.svg")
                        .text_color(theme.fg_base),
                )
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child("Agent usage"),
                ),
        )
        .children(subtitle.map(|s| {
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(s)
        }))
}

/// One window block: a name, a headroom bar, and a `NN% left … resets in X`
/// row (label left, reset right) — the reference cockpit's layout.
#[allow(clippy::too_many_arguments)]
fn window_block(
    name: &str,
    window: &UsageWindow,
    now_ms: i64,
    idle: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    let left = left_fraction(window);
    let color = headroom_color(left, theme);
    let reset = window
        .resets_at_ms
        .and_then(|t| format_reset_in(t, now_ms))
        .map(|in_| format!("Resets in {in_}"))
        .unwrap_or_else(|| idle.to_string());

    div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline * 0.5))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(name.to_string()),
        )
        .child(usage_bar(left, color, theme))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(color)
                        .child(format!("{}% left", (left * 100.0).round() as u8)),
                )
                .child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(reset),
                ),
        )
}

/// A rounded headroom bar: a muted track with a colored fill at `fraction`.
fn usage_bar(fraction: f32, color: Hsla, theme: Theme) -> Div {
    div()
        .w_full()
        .h(px(5.0))
        .rounded_full()
        .bg(theme.border_inactive)
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(color),
        )
}

fn divider(theme: Theme) -> Div {
    div().w_full().h(px(1.0)).bg(theme.border_inactive)
}

fn footer(text: String, theme: Theme, typography: &Typography) -> Div {
    div()
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_snapshot(five_pct: f64, weekly_pct: f64) -> UsageSnapshot {
        UsageSnapshot {
            five_hour: UsageWindow {
                utilization: five_pct,
                resets_at_ms: Some(10_000_000),
            },
            weekly: UsageWindow {
                utilization: weekly_pct,
                resets_at_ms: Some(600_000_000),
            },
            tier: "default_claude_max_5x".to_string(),
            captured_at_ms: None,
        }
    }

    #[test]
    fn left_fraction_is_complement_of_used() {
        // 35% used → 65% left; clamps for over-budget readings.
        assert_eq!(left_fraction(&exact_snapshot(35.0, 0.0).five_hour), 0.65);
        assert_eq!(left_fraction(&exact_snapshot(130.0, 0.0).five_hour), 0.0);
    }

    #[test]
    fn headroom_color_tiers() {
        let t = Theme::charcoal();
        assert_eq!(headroom_color(0.65, t), t.status_ok);
        assert_eq!(headroom_color(0.30, t), t.status_warn);
        assert_eq!(headroom_color(0.10, t), t.status_error);
    }

    #[test]
    fn freshness_text_live_vs_cached() {
        let live = exact_snapshot(10.0, 10.0);
        assert_eq!(freshness_text(&live, 0), "Updated just now");
        let mut cached = exact_snapshot(10.0, 10.0);
        let now = 1_000_000_000;
        cached.captured_at_ms = Some(now - 300_000);
        assert_eq!(freshness_text(&cached, now), "Updated 5m ago");
    }

    #[test]
    fn meter_label_formats_both_windows_plainly() {
        assert_eq!(meter_label(&exact_snapshot(12.0, 4.0)), "12% 5h · 4% wk");
    }

    #[test]
    fn meter_label_caps_at_hundred() {
        assert_eq!(meter_label(&exact_snapshot(130.0, 0.0)), "100% 5h · 0% wk");
    }

    #[test]
    fn popover_footer_fresh_vs_cached() {
        let fresh = exact_snapshot(26.0, 27.0);
        assert_eq!(popover_footer(&fresh, 0), "Account usage API · Max 5x");

        let mut cached = exact_snapshot(26.0, 27.0);
        let now = 1_000_000_000;
        cached.captured_at_ms = Some(now - 300_000);
        assert_eq!(
            popover_footer(&cached, now),
            "Cached · updated 5m ago · Max 5x"
        );
    }

    #[test]
    fn format_time_ago_buckets() {
        assert_eq!(format_time_ago(1_000_000, 1_030_000), "just now");
        assert_eq!(format_time_ago(1_000_000, 1_300_000), "5m ago");
        assert_eq!(format_time_ago(1_000_000, 1_000_000 + 2 * 3_600_000), "2h ago");
    }

    #[test]
    fn meter_color_tiers() {
        let t = Theme::charcoal();
        assert_eq!(meter_color(&exact_snapshot(20.0, 30.0), t), t.fg_muted);
        assert_eq!(meter_color(&exact_snapshot(75.0, 10.0), t), t.status_warn);
        assert_eq!(meter_color(&exact_snapshot(10.0, 95.0), t), t.status_error);
    }

    #[test]
    fn format_reset_in_shows_days_for_long_spans() {
        let span_ms = (6 * 24 + 21) * 3_600_000_i64 + 5 * 60_000;
        assert_eq!(format_reset_in(span_ms, 0).as_deref(), Some("6d 21h"));
        assert_eq!(format_reset_in(3_900_000, 0).as_deref(), Some("1h 05m"));
    }

    #[test]
    fn format_reset_in_hours_and_minutes() {
        assert_eq!(format_reset_in(7_500_000, 0).unwrap(), "2h 05m");
        assert_eq!(format_reset_in(2_700_000, 0).unwrap(), "45m");
        assert!(format_reset_in(100, 200).is_none());
    }
}
