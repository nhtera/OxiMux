//! Status-bar usage meter — one compact segment per configured account, and
//! the click popover that spells all of them out.
//!
//! Data comes from `oximux_agents::session_log::usage_probe` on a 60 s
//! background tick owned by `WorkspaceRoot`. Everything here is rendering plus
//! pure formatting.
//!
//! # One segment per account, not one chip
//!
//! Providers do not share a rate limit, so there is no single number to show.
//! Each gets its own segment, identified by its icon rather than its name —
//! that is what keeps the row readable as accounts are added. How much each
//! segment spells out is the user's call ([`UsageDetail`]): every window, or
//! only the one nearest its ceiling.
//!
//! An account that has left no trace on the machine produces no row upstream
//! and so no segment here. A configured account that cannot be read keeps its
//! segment and says so — "you don't use this" and "this is broken" must not
//! look alike.

use gpui::{Div, Hsla, ParentElement, Styled, div, px, relative};
use gpui_component::Icon;
use oximux_agents::session_log::usage::{
    ProviderUsage, UsageProvider, UsageSnapshot, UsageState, UsageWindow,
};
use oximux_settings::{Density, Theme, Typography, UsageDetail};

/// Status-bar label for a configured account that could not be read. The
/// reason itself lives in the popover, as it always has.
pub const UNAVAILABLE_LABEL: &str = "unavailable";

/// The icon standing in for a provider's name in the status bar and at the top
/// of its popover block.
pub fn provider_icon(provider: UsageProvider) -> &'static str {
    match provider {
        UsageProvider::ClaudeCode => "icons/claude-code.svg",
        UsageProvider::Codex => "icons/codex.svg",
    }
}

/// One status-bar segment: a provider's icon and its numbers, already colored.
pub struct MeterSegment {
    pub icon_path: &'static str,
    pub label: String,
    pub color: Hsla,
}

/// A segment per provider, in the order the probe reported them.
pub fn meter_segments(
    rows: &[ProviderUsage],
    detail: UsageDetail,
    now_ms: i64,
    theme: Theme,
) -> Vec<MeterSegment> {
    rows.iter()
        .map(|row| MeterSegment {
            icon_path: provider_icon(row.provider),
            label: match &row.state {
                UsageState::Available(snapshot) => meter_label(snapshot, detail, now_ms),
                UsageState::Unavailable { .. } => UNAVAILABLE_LABEL.to_string(),
            },
            color: match &row.state {
                UsageState::Available(snapshot) => meter_color(snapshot, theme),
                UsageState::Unavailable { .. } => theme.status_warn,
            },
        })
        .collect()
}

/// One provider's compact label.
///
/// Verbose spells out every window (`12% 5h · 4% wk`). Compact shows only the
/// window nearest its ceiling and how long until it resets (`12% 4h 12m`) —
/// the reset time rather than the window length, because with one number on
/// screen the useful question is not how long the window is but how long until
/// it lets go.
pub fn meter_label(snapshot: &UsageSnapshot, detail: UsageDetail, now_ms: i64) -> String {
    match detail {
        UsageDetail::Verbose => snapshot
            .windows
            .iter()
            .map(|w| format!("{}% {}", pct(w.ratio()), w.short_label()))
            .collect::<Vec<_>>()
            .join(" · "),
        UsageDetail::Compact => snapshot
            .tightest()
            .map(|w| format!("{}% {}", pct(w.ratio()), window_horizon(w, now_ms)))
            .unwrap_or_else(|| UNAVAILABLE_LABEL.to_string()),
    }
}

/// How long this window has left, falling back to its length when it carries
/// no reset time to count down to.
fn window_horizon(window: &UsageWindow, now_ms: i64) -> String {
    window
        .resets_at_ms
        .and_then(|t| format_reset_in(t, now_ms))
        .unwrap_or_else(|| window.short_label())
}

/// Meter color keyed to the account's hottest window: error ≥ 90 %, warn
/// ≥ 70 %, muted below — same vocabulary as the agent verb chips.
pub fn meter_color(snapshot: &UsageSnapshot, theme: Theme) -> Hsla {
    let worst = snapshot.tightest().map(UsageWindow::ratio).unwrap_or(0.0);
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

/// `"just now"` / `"5m ago"` / `"2h ago"` — coarse age of a captured reading.
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

/// A provider block's footer, derived from freshness and plan.
///
/// A reading that was *captured* rather than fetched live discloses how old it
/// is instead of passing for current. Both kinds of captured reading land here:
/// one whose live fetch failed this tick, and one from a source that only ever
/// publishes to disk and so is never live.
fn provider_footer(snapshot: &UsageSnapshot, now_ms: i64) -> String {
    let pretty = pretty_tier(&snapshot.tier);
    let tier_suffix = if pretty.is_empty() {
        String::new()
    } else {
        format!(" · {pretty}")
    };
    match snapshot.captured_at_ms {
        Some(captured) => format!("Updated {}{tier_suffix}", format_time_ago(captured, now_ms)),
        None => format!("Live{tier_suffix}"),
    }
}

/// A readable plan label from the raw tier slug (`default_claude_max_5x` →
/// `Max 5x`, `plus` → `Plus`); an unrecognized slug is title-cased rather than
/// dropped, so a new plan name still reads as one.
fn pretty_tier(tier: &str) -> String {
    if tier.is_empty() {
        String::new()
    } else if tier.contains("max_20x") {
        "Max 20x".to_string()
    } else if tier.contains("max_5x") {
        "Max 5x".to_string()
    } else if tier.contains("pro") {
        "Pro".to_string()
    } else if let Some(first) = tier.chars().next()
        && tier.chars().all(|c| c.is_ascii_lowercase())
    {
        format!("{}{}", first.to_ascii_uppercase(), &tier[1..])
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
/// red near the ceiling.
fn headroom_color(left: f32, theme: Theme) -> Hsla {
    if left >= 0.4 {
        theme.status_ok
    } else if left >= 0.2 {
        theme.status_warn
    } else {
        theme.status_error
    }
}

/// The popover card: a header, then a block per account — its windows as
/// progress bars, or the reason it could not be read. The caller owns
/// positioning and dismissal, and sizes its host to [`popover_height`].
pub fn render_usage_popover(
    rows: &[ProviderUsage],
    now_ms: i64,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    // Fills the host popup window (sized to fit in `usage_popover::open`); the
    // panel is borderless + transparent, so the card's panel background + radius
    // is what the user sees.
    let mut card = div()
        .flex()
        .flex_col()
        .size_full()
        .gap(px(density.gap_inline * 1.5))
        .p(px(density.pad_panel))
        .rounded(px(density.r_card))
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_active)
        .child(header(theme, typography));

    if rows.is_empty() {
        return card.child(divider(theme)).child(footer(
            "No agent accounts are set up on this machine.".to_string(),
            theme,
            typography,
        ));
    }

    for row in rows {
        card = card
            .child(divider(theme))
            .child(provider_block(row, now_ms, theme, density, typography));
    }
    card
}

/// One account's block: who it is, its windows or its failure, and a footer
/// naming freshness and plan.
fn provider_block(
    row: &ProviderUsage,
    now_ms: i64,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    let block = div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline))
        .child(provider_title(row.provider, theme, density, typography));

    match &row.state {
        UsageState::Available(snapshot) => {
            let mut block = block;
            for window in &snapshot.windows {
                block = block.child(window_block(window, now_ms, theme, density, typography));
            }
            block.child(footer(
                provider_footer(snapshot, now_ms),
                theme,
                typography,
            ))
        }
        UsageState::Unavailable { reason } => block
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_base)
                    .child("Unavailable"),
            )
            .child(footer(reason.clone(), theme, typography)),
    }
}

/// Card header.
///
/// Text alone: the header used to carry the primary CLI's icon, which now
/// belongs to that account's own block. Any single provider's mark at the top
/// of a card listing several would name the wrong thing.
fn header(theme: Theme, typography: &Typography) -> Div {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .child("Agent usage")
}

/// A block's title row: the provider's icon and its name.
fn provider_title(
    provider: UsageProvider,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline * 0.6))
        .text_color(theme.fg_base)
        .child(
            Icon::default()
                .path(provider_icon(provider))
                .text_color(theme.fg_base),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(provider.name()),
        )
}

/// One window block: a name, a headroom bar, and a `NN% left … resets in X`
/// row (label left, reset right).
fn window_block(
    window: &UsageWindow,
    now_ms: i64,
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
        .unwrap_or_else(|| window.idle_note().to_string());

    div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline * 0.5))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(window.name()),
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

// ---------------------------------------------------------------------------
// Popover sizing
// ---------------------------------------------------------------------------
//
// The card is hosted in a real window on macOS, so its height has to be known
// before anything renders — it cannot be discovered from the laid-out content.
// The measurement below is built from the same tokens the blocks above lay
// themselves out with, so a density or zoom change moves both together. It
// used to be a constant, which meant a Comfortable or zoomed cockpit drew a
// taller card into a window sized for a tight one.

/// Card width. Unchanged by provider count: the widest line is a window's
/// `NN% left … Resets in X` row, which is the same whoever owns it.
pub const POPOVER_WIDTH: f32 = 264.0;

/// GPUI lays text out at `phi` times its font size by default (`Style::default`
/// → `line_height: phi()`), rounded to a pixel. Nothing here overrides it, so
/// this is what one line of each token actually occupies.
const PHI: f32 = 1.618_034;

/// Height of the headroom bar, matching `usage_bar`.
const BAR_H: f32 = 5.0;

/// Height of a divider, matching `divider`.
const DIVIDER_H: f32 = 1.0;

/// One line of text at `size`.
fn line_h(size: f32) -> f32 {
    (size * PHI).round()
}

/// Rough line count for `text` at `size` inside a `column_w`-wide text column.
///
/// A failure reason comes from the provider, not from us — "OAuth access token
/// has expired. Re-authenticate to continue." wraps to two lines in this card,
/// and a height that assumed one clipped the block below it. The real shaper
/// cannot be reached from a decision made before anything renders, so this
/// estimates from character count and is deliberately biased to over-count:
/// a few extra pixels of panel is invisible, a short window loses a line.
fn wrapped_lines(text: &str, size: f32, column_w: f32) -> f32 {
    // Half the font size is a generous mean advance for the UI face; a wide
    // string is what must not be under-measured.
    let width = text.chars().count() as f32 * size * 0.5;
    (width / column_w).ceil().max(1.0)
}

/// How tall the popover must be to show `rows` without clipping.
///
/// Deliberately a function of the content: a second account roughly doubles the
/// card, and a fixed height would either clip it or leave a slab of empty panel
/// under a single one.
pub fn popover_height(rows: &[ProviderUsage], density: Density, typography: &Typography) -> f32 {
    // The card's own `gap` between children, and its padding.
    let gap = density.gap_inline * 1.5;
    let body = line_h(typography.t_body_sm);
    let sub = line_h(typography.t_sub_label);
    // What a line of text actually has to fit in.
    let column = POPOVER_WIDTH - density.pad_panel * 2.0;
    // A window block: name, bar, and the `NN% left … resets` row, at half gaps.
    let window_block = body + density.gap_inline * 0.5 + BAR_H + density.gap_inline * 0.5 + sub;

    let mut h = density.pad_panel * 2.0 + body;
    if rows.is_empty() {
        return h + gap + DIVIDER_H + gap + sub;
    }
    for row in rows {
        // Each block is preceded by a divider, and every child is gap-separated.
        // Within a block the children use the tighter `gap_inline`. The title
        // row measures as text: its icon is `size_4` (16 px, rem-based) and the
        // body line is taller than that at every size this ships with.
        h += gap + DIVIDER_H + gap + body;
        h += match &row.state {
            UsageState::Available(snapshot) => {
                snapshot.windows.len() as f32 * (density.gap_inline + window_block)
            }
            // The "Unavailable" line standing in for the windows.
            UsageState::Unavailable { .. } => density.gap_inline + body,
        };
        // The footer. Ours is short and fits on one line; a provider's failure
        // reason is not ours and routinely does not.
        let footer_lines = match &row.state {
            UsageState::Available(_) => 1.0,
            UsageState::Unavailable { reason } => {
                wrapped_lines(reason, typography.t_sub_label, column)
            }
        };
        h += density.gap_inline + sub * footer_lines;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::session_log::usage::{FIVE_HOUR_MINUTES, WEEK_MINUTES};

    fn snapshot(five_pct: f64, weekly_pct: f64) -> UsageSnapshot {
        UsageSnapshot {
            windows: vec![
                UsageWindow {
                    window_minutes: FIVE_HOUR_MINUTES,
                    utilization: five_pct,
                    resets_at_ms: Some(10_000_000),
                },
                UsageWindow {
                    window_minutes: WEEK_MINUTES,
                    utilization: weekly_pct,
                    resets_at_ms: Some(600_000_000),
                },
            ],
            tier: "default_claude_max_5x".to_string(),
            captured_at_ms: None,
        }
    }

    fn available(provider: UsageProvider, snapshot: UsageSnapshot) -> ProviderUsage {
        ProviderUsage {
            provider,
            state: UsageState::Available(snapshot),
        }
    }

    fn unavailable(provider: UsageProvider, reason: &str) -> ProviderUsage {
        ProviderUsage {
            provider,
            state: UsageState::Unavailable {
                reason: reason.to_string(),
            },
        }
    }

    #[test]
    fn verbose_spells_out_every_window() {
        assert_eq!(
            meter_label(&snapshot(12.0, 4.0), UsageDetail::Verbose, 0),
            "12% 5h · 4% wk"
        );
    }

    #[test]
    fn compact_shows_only_the_tightest_window_and_its_horizon() {
        // Weekly is hotter here, so it is the one that shows — with the time
        // until it resets rather than its length.
        let label = meter_label(&snapshot(12.0, 40.0), UsageDetail::Compact, 0);
        assert_eq!(label, "40% 6d 22h");
    }

    #[test]
    fn compact_falls_back_to_the_window_length_without_a_reset_time() {
        let mut snap = snapshot(12.0, 4.0);
        snap.windows[0].resets_at_ms = None;
        snap.windows[1].utilization = 0.0;
        assert_eq!(meter_label(&snap, UsageDetail::Compact, 0), "12% 5h");
    }

    #[test]
    fn a_label_caps_at_a_hundred_percent() {
        assert_eq!(
            meter_label(&snapshot(130.0, 0.0), UsageDetail::Verbose, 0),
            "100% 5h · 0% wk"
        );
    }

    #[test]
    fn one_segment_per_provider_in_probe_order() {
        let t = Theme::charcoal();
        let rows = vec![
            available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0)),
            available(UsageProvider::Codex, snapshot(0.0, 1.0)),
        ];
        let segments = meter_segments(&rows, UsageDetail::Verbose, 0, t);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].icon_path, "icons/claude-code.svg");
        assert_eq!(segments[0].label, "12% 5h · 4% wk");
        assert_eq!(segments[1].icon_path, "icons/codex.svg");
        assert_eq!(segments[1].label, "0% 5h · 1% wk");
    }

    #[test]
    fn a_failing_provider_keeps_its_segment_beside_a_working_one() {
        // The whole point of per-provider rows: one being unreadable must not
        // take the other's numbers off the bar.
        let t = Theme::charcoal();
        let rows = vec![
            available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0)),
            unavailable(UsageProvider::Codex, "Every window has reset"),
        ];
        let segments = meter_segments(&rows, UsageDetail::Verbose, 0, t);
        assert_eq!(segments[0].label, "12% 5h · 4% wk");
        assert_eq!(segments[1].label, UNAVAILABLE_LABEL);
        assert_eq!(segments[1].color, t.status_warn);
    }

    #[test]
    fn segment_color_follows_the_hottest_window() {
        let t = Theme::charcoal();
        let quiet = meter_segments(
            &[available(UsageProvider::ClaudeCode, snapshot(20.0, 30.0))],
            UsageDetail::Verbose,
            0,
            t,
        );
        assert_eq!(quiet[0].color, t.fg_muted);
        // Hot on the weekly window only — the segment still goes red.
        let hot = meter_segments(
            &[available(UsageProvider::ClaudeCode, snapshot(10.0, 95.0))],
            UsageDetail::Verbose,
            0,
            t,
        );
        assert_eq!(hot[0].color, t.status_error);
        let warm = meter_segments(
            &[available(UsageProvider::ClaudeCode, snapshot(75.0, 10.0))],
            UsageDetail::Verbose,
            0,
            t,
        );
        assert_eq!(warm[0].color, t.status_warn);
    }

    #[test]
    fn each_provider_has_its_own_icon() {
        assert_ne!(
            provider_icon(UsageProvider::ClaudeCode),
            provider_icon(UsageProvider::Codex),
            "the icon is what tells the segments apart"
        );
    }

    #[test]
    fn footer_distinguishes_live_from_captured() {
        let live = snapshot(26.0, 27.0);
        assert_eq!(provider_footer(&live, 0), "Live · Max 5x");

        let mut captured = snapshot(26.0, 27.0);
        let now = 1_000_000_000;
        captured.captured_at_ms = Some(now - 300_000);
        assert_eq!(provider_footer(&captured, now), "Updated 5m ago · Max 5x");
    }

    #[test]
    fn pretty_tier_titles_an_unknown_plan_rather_than_dropping_it() {
        assert_eq!(pretty_tier("default_claude_max_5x"), "Max 5x");
        assert_eq!(pretty_tier("default_claude_max_20x"), "Max 20x");
        assert_eq!(pretty_tier("plus"), "Plus");
        assert_eq!(pretty_tier(""), "");
    }

    #[test]
    fn left_fraction_is_complement_of_used() {
        assert_eq!(left_fraction(&snapshot(35.0, 0.0).windows[0]), 0.65);
        assert_eq!(left_fraction(&snapshot(130.0, 0.0).windows[0]), 0.0);
    }

    #[test]
    fn headroom_color_tiers() {
        let t = Theme::charcoal();
        assert_eq!(headroom_color(0.65, t), t.status_ok);
        assert_eq!(headroom_color(0.30, t), t.status_warn);
        assert_eq!(headroom_color(0.10, t), t.status_error);
    }

    #[test]
    fn format_time_ago_buckets() {
        assert_eq!(format_time_ago(1_000_000, 1_030_000), "just now");
        assert_eq!(format_time_ago(1_000_000, 1_300_000), "5m ago");
        assert_eq!(
            format_time_ago(1_000_000, 1_000_000 + 2 * 3_600_000),
            "2h ago"
        );
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

    fn height(rows: &[ProviderUsage]) -> f32 {
        popover_height(rows, Density::cockpit(), &Typography::cockpit())
    }

    #[test]
    fn the_popover_grows_with_each_account() {
        let one = vec![available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0))];
        let two = vec![
            available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0)),
            available(UsageProvider::Codex, snapshot(0.0, 1.0)),
        ];
        assert!(
            height(&two) > height(&one),
            "a second account must not be clipped into the first one's card"
        );
        // A failing account is shorter than a reading one, but still present.
        let failing = vec![
            available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0)),
            unavailable(UsageProvider::Codex, "Every window has reset"),
        ];
        assert!(height(&failing) > height(&one));
        assert!(height(&failing) < height(&two));
        // And nothing configured still leaves a card big enough to say so.
        assert!(height(&[]) > 0.0);
    }

    #[test]
    fn a_provider_reporting_one_window_sizes_smaller_than_one_reporting_two() {
        let mut single = snapshot(12.0, 4.0);
        single.windows.truncate(1);
        let one_window = vec![available(UsageProvider::Codex, single)];
        let two_windows = vec![available(UsageProvider::Codex, snapshot(12.0, 4.0))];
        assert!(height(&one_window) < height(&two_windows));
    }

    #[test]
    fn a_wrapping_failure_reason_gets_the_lines_it_needs() {
        // Caught live: this exact message wraps to two lines in the card, and a
        // height that counted one sliced the last line off the block below.
        let long = "OAuth access token has expired. Re-authenticate to continue.";
        let wrapping = vec![
            unavailable(UsageProvider::ClaudeCode, long),
            available(UsageProvider::Codex, snapshot(0.0, 1.0)),
        ];
        let short = vec![
            unavailable(UsageProvider::ClaudeCode, "Not signed in"),
            available(UsageProvider::Codex, snapshot(0.0, 1.0)),
        ];
        assert!(
            height(&wrapping) > height(&short),
            "a reason that wraps must buy its own extra line"
        );
    }

    #[test]
    fn wrapped_lines_counts_a_full_column_as_more_than_one() {
        let column = POPOVER_WIDTH - Density::cockpit().pad_panel * 2.0;
        let size = Typography::cockpit().t_sub_label;
        assert_eq!(wrapped_lines("Not signed in", size, column), 1.0);
        assert_eq!(wrapped_lines("", size, column), 1.0, "an empty line is still a line");
        assert!(
            wrapped_lines(
                "OAuth access token has expired. Re-authenticate to continue.",
                size,
                column
            ) >= 2.0
        );
    }

    #[test]
    fn a_roomier_or_zoomed_cockpit_gets_a_taller_card() {
        // The card lays itself out from these tokens, so the window sized for
        // it has to move with them — this is what the old fixed height missed.
        let rows = vec![available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0))];
        let tight = popover_height(&rows, Density::cockpit(), &Typography::cockpit());
        let roomy = popover_height(&rows, Density::comfortable(), &Typography::cockpit());
        assert!(roomy > tight, "more air around the same text needs more card");

        let zoomed_tokens = Typography::for_appearance(oximux_settings::Appearance {
            scale: oximux_settings::UiScale::from_percent(150),
            ..oximux_settings::Appearance::default()
        });
        let zoomed = popover_height(&rows, Density::cockpit(), &zoomed_tokens);
        assert!(zoomed > tight, "bigger text needs more card");
    }

    #[test]
    fn one_account_lands_near_the_height_this_card_used_to_be_fixed_at() {
        // Sanity rail on the arithmetic: the single-account card is the one
        // shape that shipped before, at a hardcoded 210 px. The layout lost a
        // subtitle and a divider and gained a title row, so an exact match is
        // wrong — but a wild answer here means a token was mismeasured.
        let one = vec![available(UsageProvider::ClaudeCode, snapshot(12.0, 4.0))];
        let h = height(&one);
        assert!((180.0..=230.0).contains(&h), "unexpected card height: {h}");
    }
}
