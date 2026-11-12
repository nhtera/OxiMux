//! How an attention kind looks on a row.
//!
//! The words and the tone are the evaluator's (`AttentionKind::verb` and
//! `::tone`, in `oximux_agents`), because they are part of what "needs you"
//! means and every surface has to say the same thing. What belongs here is the
//! half the evaluator must not know: which theme token a tone resolves to, and
//! which glyph a kind draws.
//!
//! This is what PR B left owing. A row's state glyph and its state line read
//! the parsed `AgentStatus`, which cannot tell a failure that has been dealt
//! with from one that has not — so a chat row sitting under NEEDS YOU drew an
//! idle dot and said "Idle". Rather than map attention *back* into a status to
//! feed the old helpers, the row reads the attention directly when it has one
//! and falls back to the status when it does not.

use gpui::{AnyElement, Hsla, IntoElement, ParentElement, Styled, div, px};
use gpui_component::Icon;
use oximux_agents::attention::{Attention, AttentionKind, AttentionTone};
use oximux_settings::Theme;

/// Diameter of the state dot, matching the rail's own indicator so a card and
/// a sub-row read alike.
const STATE_DOT: f32 = 6.0;
/// The box both the dot and the check sit in, so swapping between them never
/// shifts the text beside it.
const STATE_BOX: f32 = 14.0;
/// The finished check, which reads heavier than a dot at the same size.
const STATE_CHECK: f32 = 12.0;

/// The theme token a tone names.
///
/// One place, so a row's dot, its accent bar and (later) the status-bar chip
/// cannot drift into three slightly different reds.
pub fn tone_color(tone: AttentionTone, theme: Theme) -> Hsla {
    match tone {
        AttentionTone::Warn => theme.status_warn,
        AttentionTone::Error => theme.status_error,
        AttentionTone::Muted => theme.status_muted,
        AttentionTone::Ok => theme.status_ok,
    }
}

/// The colour of the 2px accent bar for a row that wants something, or `None`
/// for a row that wants nothing and so wears no bar.
///
/// A finished turn nobody has read is deliberately excluded: it is work to
/// collect, not a problem to look at, and an accent on every completed agent
/// would leave the list striped. Its check glyph carries it instead.
pub fn accent_color(attention: Option<&Attention>, theme: Theme) -> Option<Hsla> {
    let attention = attention?;
    (attention.kind != AttentionKind::FinishedUnread)
        .then(|| tone_color(attention.kind.tone(), theme))
}

/// The state glyph for a kind: a check for a finished turn, a dot in the
/// kind's own tone for everything else.
///
/// No spinner here — a row that wants a person is not working, and `Running`
/// has no attention kind at all, so the status path still draws that.
pub fn attention_indicator(kind: AttentionKind, theme: Theme) -> AnyElement {
    let box_el = div().flex().items_center().justify_center().size(px(STATE_BOX)).flex_shrink_0();
    if kind == AttentionKind::FinishedUnread {
        return box_el
            .child(
                Icon::default()
                    .path("icons/circle-check.svg")
                    .size(px(STATE_CHECK))
                    .text_color(theme.status_ok),
            )
            .into_any_element();
    }
    box_el
        .child(
            div()
                .size(px(STATE_DOT))
                .rounded_full()
                .bg(tone_color(kind.tone(), theme)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::attention::AttentionSource;

    fn attention(kind: AttentionKind) -> Attention {
        Attention { kind, since: 0, reason: None, source: AttentionSource::Health }
    }

    fn theme() -> Theme {
        Theme::charcoal()
    }

    #[test]
    fn every_tone_resolves_to_its_own_token() {
        // Four tones, four tokens: a mapping that collapsed two of them would
        // make a stall and a failure look the same on the row.
        let t = theme();
        let colors = [
            tone_color(AttentionTone::Warn, t),
            tone_color(AttentionTone::Error, t),
            tone_color(AttentionTone::Muted, t),
            tone_color(AttentionTone::Ok, t),
        ];
        for (i, a) in colors.iter().enumerate() {
            for b in colors.iter().skip(i + 1) {
                assert_ne!(a, b, "two tones paint the same colour");
            }
        }
    }

    #[test]
    fn a_row_that_wants_something_wears_its_own_accent() {
        let t = theme();
        assert_eq!(
            accent_color(Some(&attention(AttentionKind::NeedsPermission)), t),
            Some(t.status_warn)
        );
        assert_eq!(accent_color(Some(&attention(AttentionKind::Failed)), t), Some(t.status_error));
        assert_eq!(
            accent_color(Some(&attention(AttentionKind::Stalled)), t),
            Some(t.status_muted),
            "a stall is not an error: nothing has gone wrong, it has gone quiet"
        );
    }

    #[test]
    fn a_finished_turn_and_a_quiet_row_wear_no_accent() {
        let t = theme();
        assert_eq!(
            accent_color(Some(&attention(AttentionKind::FinishedUnread)), t),
            None,
            "unread work is a thing to collect, not a thing to be alarmed by — \
             and an accent on every finished agent would stripe the whole list"
        );
        assert_eq!(accent_color(None, t), None);
    }
}
