//! The composer's live context-usage meter: a compact bar-in-pill showing how
//! full the model's context window is for the current turn.
//!
//! Follows the rate-limit meter's fill-in-a-track pattern
//! (`usage/usage_meter.rs::usage_bar`), but measures context USED (not remaining
//! headroom), so the thresholds are inverted: >90% used is red, ≥70% amber, else
//! a muted info color. When the window size isn't known yet — a brand-new Claude
//! chat's first live turn, whose stream events omit the window — there is no
//! denominator: the meter shows the raw token count with no bar and a neutral
//! color (a designed state, not a missing feature).

use gpui::{div, px, relative, Hsla, InteractiveElement, IntoElement, ParentElement, Styled};
use oximux_settings::{Density, Theme, Typography};

/// Threshold color for the fraction of the context window USED (the inverse of
/// the rate-limit meter's headroom coloring): >90% error, ≥70% warn, else a
/// muted info color. Pure and testable.
pub fn meter_color(used_fraction: f32, theme: &Theme) -> Hsla {
    if used_fraction > 0.90 {
        theme.status_error
    } else if used_fraction >= 0.70 {
        theme.status_warn
    } else {
        theme.status_info
    }
}

/// Compact token label, e.g. `900`, `1.2k`, `47.0k`.
pub fn compact_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The meter element: a small pill carrying a label (a percent when the window
/// is known, else the raw token count) plus, when a denominator exists, a thin
/// colored bar. Hover shows `tooltip_text` (exact %, used/max, cost since open).
pub fn context_meter(
    used_fraction: Option<f32>,
    label: String,
    tooltip_text: String,
    theme: &Theme,
    typography: &Typography,
    density: &Density,
) -> impl IntoElement {
    let mut pill = div()
        .id("chat-context-meter")
        // `relative` + `group` anchor the centered hover tooltip below; the pill
        // itself is not absolutely positioned, so this doesn't clobber layout.
        .relative()
        .group("chat-context-meter-grp")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(density.r_card))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_muted)
        .child(div().child(label));
    if let Some(f) = used_fraction {
        let fill = meter_color(f, theme);
        pill = pill.child(
            div()
                .w(px(48.0))
                .h(px(5.0))
                .rounded_full()
                .bg(theme.border_inactive)
                .child(
                    div()
                        .h_full()
                        .w(relative(f.clamp(0.0, 1.0)))
                        .rounded_full()
                        .bg(fill),
                ),
        );
    }
    // Centered hover tooltip above the meter. gpui's native `.tooltip()` anchors
    // to the mouse CURSOR, so it drifted off to the meter's side; this hand-rolled
    // overlay is full-width + center-justified so it centers over the pill without
    // measuring the label, and `group_hover` reveals it on hover. Each `\n`-split
    // line is a nowrap row so the optional "Cost since open" line wraps correctly.
    pill.child(
        div()
            .absolute()
            .bottom_full()
            .left_0()
            .w_full()
            .pb(px(6.0))
            .flex()
            .justify_center()
            .invisible()
            .group_hover("chat-context-meter-grp", |s| s.visible())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(density.r_xs))
                    .bg(theme.bg_overlay)
                    .border_1()
                    .border_color(theme.border_inactive)
                    .text_color(theme.fg_base)
                    .text_size(px(typography.t_body_sm))
                    .shadow_md()
                    .children(
                        tooltip_text
                            .split('\n')
                            .map(|line| div().whitespace_nowrap().child(line.to_string()))
                            .collect::<Vec<_>>(),
                    ),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::Theme;

    #[test]
    fn thresholds_match_used_context_bands() {
        let t = Theme::default();
        // >90% used → error; ≥70% → warn; below → info (muted).
        assert_eq!(meter_color(0.95, &t), t.status_error);
        assert_eq!(meter_color(0.91, &t), t.status_error);
        assert_eq!(meter_color(0.90, &t), t.status_warn, "exactly 90% is not yet red");
        assert_eq!(meter_color(0.70, &t), t.status_warn);
        assert_eq!(meter_color(0.69, &t), t.status_info);
        assert_eq!(meter_color(0.10, &t), t.status_info);
    }

    #[test]
    fn compact_tokens_formats_thousands() {
        assert_eq!(compact_tokens(900), "900");
        assert_eq!(compact_tokens(1200), "1.2k");
        assert_eq!(compact_tokens(47000), "47.0k");
    }
}
