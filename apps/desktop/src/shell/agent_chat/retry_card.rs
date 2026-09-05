//! Pure (cx-free) renderer for the queued-retry card.
//!
//! Shown in place of the error card when a turn failed on a provider limit and
//! the app has scheduled itself to send it again. The card exists so a waiting
//! turn never looks like a forgotten one: it names why the turn is held, when
//! it will go, and gives the user both overrides — send it now, or drop it.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

/// Human countdown to a wake time: `45s`, `12m`, `3h 10m`.
///
/// Rounds *down* to the unit shown, so "1m" never means "in 119 seconds". A
/// wake time already past reads `now` rather than a negative or zero duration —
/// the card can outlive its timer by a frame, and "in -1s" reads as a bug.
pub(super) fn countdown(remaining_ms: i64) -> String {
    if remaining_ms <= 0 {
        return "now".into();
    }
    let secs = remaining_ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => {
            let (h, m) = (secs / 3600, (secs % 3600) / 60);
            if m == 0 { format!("{h}h") } else { format!("{h}h {m}m") }
        }
    }
}

/// What the card says about the held turn. Grouped rather than passed loose so
/// the renderer keeps the same `(theme, typo, density, controls)` tail shape as
/// its neighbours in this directory.
pub(super) struct Held<'a> {
    /// Short human reason ("Usage limit reached").
    pub reason: &'a str,
    /// Milliseconds until the retry fires; may be negative for a frame.
    pub remaining_ms: i64,
    /// Automatic attempts already spent, zero-based.
    pub attempt: u32,
}

/// Build the queued-retry card: why the turn is held, when it goes, and the two
/// controls the view supplies (the clicks need a `Context`, so the view owns
/// them while this stays pure).
pub(super) fn retry_card(
    held: Held<'_>,
    theme: Theme,
    typo: &Typography,
    density: Density,
    send_now: impl IntoElement,
    cancel: impl IntoElement,
) -> AnyElement {
    let Held { reason, remaining_ms, attempt } = held;
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .w_full()
        // Without `min_w_0` a flex ancestor honors the longest unwrapped line
        // and overflows the column — the same trap the markdown bodies hit.
        .min_w_0()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.status_warning.opacity(0.4))
        .bg(theme.status_warning.opacity(0.08))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_muted)
                .child(format!("Attempt {} of {}", attempt + 1, oximux_agents::retry::MAX_ATTEMPTS)),
        )
        .child(
            div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_base)
                .child(format!("{reason} — retrying in {}", countdown(remaining_ms))),
        )
        .child(div().flex().gap(px(8.0)).child(send_now).child(cancel))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_rounds_down_to_the_unit_it_shows() {
        assert_eq!(countdown(45_000), "45s");
        assert_eq!(countdown(59_999), "59s");
        assert_eq!(countdown(60_000), "1m");
        // 119s is "1m", never "2m" — a countdown that overstates reads as a
        // stall when it passes.
        assert_eq!(countdown(119_000), "1m");
        assert_eq!(countdown(3_599_000), "59m");
        assert_eq!(countdown(3_600_000), "1h");
        assert_eq!(countdown(11_400_000), "3h 10m");
    }

    #[test]
    fn a_wake_time_already_past_reads_as_now() {
        assert_eq!(countdown(0), "now");
        assert_eq!(countdown(-5_000), "now");
    }
}
