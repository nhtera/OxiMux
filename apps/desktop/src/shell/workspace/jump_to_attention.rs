//! Walking the sessions that need a person.
//!
//! ⌘⇧U takes you to the next one. The order is the host's — worst kind first,
//! then longest waiting — so the chord and the list agree about what "next"
//! means, and neither has an order of its own.
//!
//! **The cursor is the session, not an index.** An index into a list that
//! changes under you is the wrong state to keep: answering the second of four
//! takes it out, and the next press would land on the fourth while skipping
//! the third. Remembering *which session* you were last sent to means the next
//! press is always "the one after that one, in the order as it stands now" —
//! and a session that has been dealt with is simply not in the list to find,
//! so the walk falls back to the front.

use oximux_agents::attention::Attention;

use crate::workspace_root::WorkspaceRoot;

/// The next target to open, given where the walk last was.
///
/// `needs_you` is the host's order. `last` is the target the previous press
/// sent the person to, if any. Returns `None` only when nothing is asking.
///
/// Wraps: the press after the last row goes back to the first, because an
/// inbox chord that stops working at the bottom of the list makes the person
/// find the list anyway.
pub fn next_target(needs_you: &[(String, Attention)], last: Option<&str>) -> Option<String> {
    let first = needs_you.first().map(|(target, _)| target.clone());
    let Some(last) = last else {
        return first;
    };
    // The one after `last`, if `last` is still in the list at all. A session
    // that has been answered since is not, and then the walk starts over
    // rather than guessing where it would have been.
    let at = needs_you.iter().position(|(target, _)| target == last);
    match at {
        Some(at) => needs_you.get(at + 1).map(|(target, _)| target.clone()).or(first),
        None => first,
    }
}

impl WorkspaceRoot {
    /// Open the next session that needs a person.
    ///
    /// A no-op when nothing is asking — deliberately silent rather than a
    /// toast: the chord is pressed on the way past, and "nothing needs you"
    /// is the answer the empty rail already gives.
    pub(crate) fn jump_to_next_attention(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let needing = crate::shell::left_rail::badge::needs_you_now(cx);
        let Some(target) = next_target(&needing, self.last_attention_jump.as_deref()) else {
            return;
        };
        self.last_attention_jump = Some(target.clone());
        // The same routing a banner's click uses, including the fallback to
        // the Agents page for a session with no tab to focus.
        self.navigate_to_attention_target(&target, window, cx);
    }

    /// Open the Agents page on what needs someone.
    pub(crate) fn show_needs_you(&mut self, cx: &mut gpui::Context<Self>) {
        self.show_agents_page(cx);
    }

    /// Keep the focused session standing out although it has been read.
    ///
    /// Reads the focused rail row rather than a selection of its own: the
    /// thing the person means by "this session" is the one they are looking
    /// at, and the rail already tracks that.
    pub(crate) fn mark_focused_unread(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(target) = self.focused_attention_target(cx) else {
            return;
        };
        if let Some(host) = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
        {
            host.mark_unread(&target);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::attention::{AttentionKind, AttentionSource};

    fn needing(targets: &[&str]) -> Vec<(String, Attention)> {
        targets
            .iter()
            .map(|t| {
                (
                    t.to_string(),
                    Attention {
                        kind: AttentionKind::NeedsPermission,
                        since: 0,
                        reason: None,
                        source: AttentionSource::Health,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn the_first_press_goes_to_the_front_of_the_list() {
        let list = needing(&["chat:a", "chat:b"]);
        assert_eq!(next_target(&list, None).as_deref(), Some("chat:a"));
    }

    #[test]
    fn each_press_takes_the_next_one_and_wraps() {
        let list = needing(&["chat:a", "chat:b", "chat:c"]);
        assert_eq!(next_target(&list, Some("chat:a")).as_deref(), Some("chat:b"));
        assert_eq!(next_target(&list, Some("chat:b")).as_deref(), Some("chat:c"));
        assert_eq!(
            next_target(&list, Some("chat:c")).as_deref(),
            Some("chat:a"),
            "a chord that stops at the bottom makes the person find the list anyway"
        );
    }

    #[test]
    fn an_answered_session_drops_out_of_the_cycle() {
        // The reason the cursor is a session and not an index: answering the
        // one you were just sent to must not skip the one after it. With an
        // index, "2 of 4" would become "2 of 3" and the next press would land
        // on the old fourth.
        let before = needing(&["chat:a", "chat:b", "chat:c"]);
        assert_eq!(next_target(&before, Some("chat:a")).as_deref(), Some("chat:b"));

        // `chat:b` is answered while the person is looking at it.
        let after = needing(&["chat:a", "chat:c"]);
        assert_eq!(
            next_target(&after, Some("chat:b")).as_deref(),
            Some("chat:a"),
            "the one it was on is gone, so the walk starts over rather than guessing"
        );
    }

    #[test]
    fn one_session_asking_is_pressed_again_and_again() {
        // Not nothing: the person may be pressing it to get back to the row
        // they are working through.
        let list = needing(&["chat:a"]);
        assert_eq!(next_target(&list, Some("chat:a")).as_deref(), Some("chat:a"));
    }

    #[test]
    fn a_session_read_through_the_chord_leaves_the_cycle() {
        // The reason the walk is honest: opening a session records the look,
        // so a failure read this way leaves `needs_you` — and the next press
        // moves on instead of coming back to the same row. The host's own
        // tests cover the clearing; this pins the shape the walk relies on,
        // which is that the list it reads is the list after the look.
        let before = needing(&["chat:a", "chat:b"]);
        assert_eq!(next_target(&before, None).as_deref(), Some("chat:a"));

        // `chat:a` was read and is gone from the host's answer.
        let after = needing(&["chat:b"]);
        assert_eq!(
            next_target(&after, Some("chat:a")).as_deref(),
            Some("chat:b"),
            "the one just read is not revisited"
        );
    }

    #[test]
    fn nothing_asking_goes_nowhere() {
        assert_eq!(next_target(&[], None), None);
        assert_eq!(next_target(&[], Some("chat:a")), None);
    }
}
