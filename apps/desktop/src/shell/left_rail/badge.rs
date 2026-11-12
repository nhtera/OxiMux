//! The Agents nav badge: how many sessions have started needing someone.
//!
//! Its own module because `left_rail/mod.rs` is well past its size budget and
//! this is a rule rather than layout — and because the rule was wrong in a way
//! that took a reviewer to see. The badge used to count *workspace status*
//! transitions, a map that holds one tracked agent per workspace and no chat
//! sessions at all, so a chat waiting on an approval could sit in NEEDS YOU
//! with the badge reading zero.
//!
//! It counts the host's answer now, the same one the rail and the palette
//! read. Still transitions, not a total: the badge is "what happened while you
//! were not looking", so a session that has been waiting for an hour must not
//! re-count on every refresh.

use std::collections::HashSet;

/// How many new alerts to add after this pass.
///
/// `previous` and `current` are the sets of targets that needed a person then
/// and now. A target in both has already been counted; one that has left does
/// not take its count with it (the person still has not looked — opening the
/// page is what clears the badge).
pub fn newly_needing(previous: &HashSet<String>, current: &HashSet<String>) -> u32 {
    current.difference(previous).count() as u32
}

/// Every target the host says needs a person right now.
///
/// Empty when the host has not started — remote disabled, or a frame before
/// it — which reads as "nothing is asking", the same as the rail shows.
pub fn targets_needing_you(cx: &gpui::App) -> HashSet<String> {
    cx.try_global::<crate::remote_control::RemoteControl>()
        .and_then(|rc| rc.host_core())
        .map(|host| host.needs_you().into_iter().map(|(target, _)| target).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(targets: &[&str]) -> HashSet<String> {
        targets.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn only_a_target_that_newly_needs_someone_counts() {
        let before = set(&["chat:s1"]);
        let after = set(&["chat:s1", "cockpit:7"]);
        assert_eq!(newly_needing(&before, &after), 1, "the one that is new");
        assert_eq!(
            newly_needing(&after, &after),
            0,
            "a session still waiting has already been counted, however many \
             times the rail refreshes"
        );
    }

    #[test]
    fn an_alert_that_was_dealt_with_does_not_subtract() {
        // The badge is what happened while the person was away, not a live
        // total: dealing with one thing does not un-see another.
        let before = set(&["chat:s1", "cockpit:7"]);
        let after = set(&["cockpit:7"]);
        assert_eq!(newly_needing(&before, &after), 0);
    }

    #[test]
    fn a_chat_session_counts_like_any_other() {
        // The bug this module exists for: the old counter read a map of
        // workspace agent statuses, which has no chat rows in it at all.
        assert_eq!(newly_needing(&HashSet::new(), &set(&["chat:s1"])), 1);
    }
}
