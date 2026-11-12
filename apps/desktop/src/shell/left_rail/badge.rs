//! How many sessions need a person — the number the rail badge and the
//! status-bar chip both show.
//!
//! Its own module because `left_rail/mod.rs` is well past its size budget and
//! this is a rule rather than layout — and because the rule was wrong twice
//! before it was right. It counted *workspace status* transitions, a map that
//! holds one tracked agent per workspace and no chat sessions at all, so a
//! chat waiting on an approval sat in NEEDS YOU with the badge reading zero.
//! Then it counted transitions of the host's answer, which was correct but
//! still a tally: it needed a reset when the page opened, and it could drift
//! from what the list actually showed.
//!
//! It is a **count** now. The badge says how many sessions are asking, which
//! is the same number the chip shows and the same number the list has rows
//! for, so none of the three can disagree. It needs no resetting: dealing with
//! a session removes it from the count, which is what "dealt with" should mean
//! — and it survives a restart for free, because the host's answer does.

use oximux_agents::attention::{Attention, AttentionKind};

/// Everything that needs a person right now, worst first.
///
/// One read per pass, shared by the badge and the chip. Empty when the host
/// has not started — remote disabled, or a frame before it — which reads as
/// "nothing is asking", the same as the rail shows.
pub fn needs_you_now(cx: &gpui::App) -> Vec<(String, Attention)> {
    cx.try_global::<crate::remote_control::RemoteControl>()
        .and_then(|rc| rc.host_core())
        .map(|host| host.needs_you())
        .unwrap_or_default()
}

/// The worst kind among `needs_you`, which is what colours the chip.
///
/// `None` for an empty list, where there is nothing to colour. The order is
/// the evaluator's `priority()`, not a local one, so the chip's colour and the
/// list's top row always come from the same ranking.
pub fn worst_kind(needs_you: &[(String, Attention)]) -> Option<AttentionKind> {
    needs_you.iter().map(|(_, a)| a.kind).min_by_key(|kind| kind.priority())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::attention::AttentionSource;

    fn needing(kinds: &[AttentionKind]) -> Vec<(String, Attention)> {
        kinds
            .iter()
            .enumerate()
            .map(|(i, &kind)| {
                (
                    format!("chat:s{i}"),
                    Attention { kind, since: 0, reason: None, source: AttentionSource::Health },
                )
            })
            .collect()
    }

    #[test]
    fn the_worst_kind_decides_the_colour() {
        // A permission among failures still colours the chip warn, because a
        // permission is the thing to deal with first — the same order the list
        // puts its rows in.
        assert_eq!(
            worst_kind(&needing(&[
                AttentionKind::Failed,
                AttentionKind::NeedsPermission,
                AttentionKind::Stalled
            ])),
            Some(AttentionKind::NeedsPermission)
        );
        assert_eq!(
            worst_kind(&needing(&[AttentionKind::Stalled, AttentionKind::Failed])),
            Some(AttentionKind::Failed),
            "a failure outranks a stall"
        );
    }

    #[test]
    fn nothing_asking_has_no_colour_and_no_chip() {
        assert_eq!(worst_kind(&[]), None);
    }
}
