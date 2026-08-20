//! Left timeline tick-rail — the always-on half of the conversation timeline
//! (the labelled jump list lives in [`super::jump_menu`]).
//!
//! A compact document-minimap: one short tick per user turn, stacked tightly at
//! the top of the left gutter (NOT spread across the viewport). Tick length
//! scales with the message length, the tick nearest the top of the viewport is
//! the "active" tick (accent-highlighted), and clicking a tick scrolls that turn
//! into view (+ flash). Hovering the cluster reveals the labelled jump list
//! ([`super::jump_menu`]) beside it — the rail is the minimap, the list is its
//! expanded form.

use gpui::{div, px, AnyElement, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled};

use super::jump_menu::{user_turn_count, user_turn_lengths};
use super::{AgentChatView, RAIL_W};

/// Below this many user turns the rail is hidden — no value on a short thread.
const MIN_TICKS: usize = 2;
/// Top inset of the tick cluster within the gutter.
const RAIL_TOP: f32 = 12.0;
/// Left inset of the ticks within the gutter.
const RAIL_LEFT: f32 = 7.0;
/// Per-tick hit-band height.
const ROW_H: f32 = 6.0;
/// Gap between tick hit-bands.
const ROW_GAP: f32 = 4.0;
/// Shortest / longest tick, and the message length that saturates the scale.
const TICK_MIN_W: f32 = 9.0;
const TICK_MAX_W: f32 = 20.0;
const LEN_CAP: usize = 280;

/// Tick length for a message of `char_len` characters — grows with length and
/// clamps at [`LEN_CAP`], giving the minimap its varied-length texture. Pure so
/// it's unit-testable.
pub(super) fn tick_width(char_len: usize) -> f32 {
    let t = char_len.min(LEN_CAP) as f32 / LEN_CAP as f32;
    TICK_MIN_W + t * (TICK_MAX_W - TICK_MIN_W)
}

impl AgentChatView {
    /// The left tick-rail: a compact top-anchored stack of ticks, one per user
    /// turn. `None` when there are too few turns to bother. Meant to sit as the
    /// first child of the transcript row.
    pub(super) fn render_message_rail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let n = user_turn_count(&self.thread.entries);
        if n < MIN_TICKS {
            return None;
        }
        let theme = self.theme;
        let lengths = user_turn_lengths(&self.thread.entries);
        let active = self.current_user_ordinal().min(n - 1);

        // The ticks live in an auto-height cluster (not the full-height column),
        // so only hovering the cluster — not the whole left strip — reveals the
        // jump list. `w_full` keeps the cluster's right edge flush with the
        // gutter so its hover region meets the list's with no gap.
        let mut cluster = div()
            .id("message-rail")
            .flex()
            .flex_col()
            .w_full()
            .gap(px(ROW_GAP))
            .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                this.rail_hover = *hovered;
                cx.notify();
            }));
        // Resting ticks lift from subtle → muted while the rail is hovered, a
        // quiet cue that the minimap is interactive (and that the list it opens
        // belongs to it).
        let rest = if self.rail_hover { theme.fg_muted } else { theme.fg_subtle };
        for i in 0..n {
            let is_active = i == active;
            let w = tick_width(lengths.get(i).copied().unwrap_or(0));
            // Active (current position) reads accent + a touch taller; the rest
            // rest subtle, like a document minimap.
            let (color, h) = if is_active { (theme.focus_ring, 3.0) } else { (rest, 2.0) };
            cluster = cluster.child(
                div()
                    .id(("rail-tick", i))
                    .flex()
                    .items_center()
                    .w_full()
                    .h(px(ROW_H))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _window, cx| {
                            this.scroll_to_user_ordinal(i, cx)
                        }),
                    )
                    .child(div().w(px(w)).h(px(h)).rounded_full().bg(color)),
            );
        }
        let col = div()
            .flex_none()
            .w(px(RAIL_W))
            .h_full()
            .flex()
            .flex_col()
            .justify_start()
            .pt(px(RAIL_TOP))
            .pl(px(RAIL_LEFT))
            .child(cluster);
        Some(col.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_width_grows_with_length_then_clamps() {
        assert!(tick_width(0) < tick_width(100));
        assert!(tick_width(100) < tick_width(LEN_CAP));
        // Saturates at the cap — a huge message isn't longer than a cap-length one.
        assert_eq!(tick_width(LEN_CAP), tick_width(10_000));
    }

    #[test]
    fn tick_width_stays_within_bounds() {
        assert!(tick_width(0) >= TICK_MIN_W);
        assert!(tick_width(10_000) <= TICK_MAX_W);
    }
}
