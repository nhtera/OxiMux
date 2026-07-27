//! Keeping screen captures off any surface that leaves this machine.
//!
//! A screenshot tool returns a picture of whatever was on screen — a password
//! manager mid-unlock, a private document, someone else's message. On the
//! desktop that is the point: the user is looking at the same screen and can see
//! what the agent saw. On a paired phone it is not. The images ride the ordinary
//! `ToolCall.images` channel and would otherwise reach a phone through normal
//! chat, with nothing about the transfer looking unusual.
//!
//! # Not "strip every image"
//!
//! An agent that reads a PNG the user asked about should still show it on the
//! phone; that image is in the conversation because the user put it there.
//! What is removed is narrower: images produced *by the screen-control server*,
//! identified by the tool that returned them.
//!
//! # Why the event path needs to remember something
//!
//! [`ThreadEvent::ToolResultImages`] carries a `tool_use_id` and pixels — no
//! tool name. So an event cannot be judged on its own, and a stateless filter
//! would have to choose between passing screenshots or dropping every image.
//! [`ScreenshotFilter`] keeps the small amount of correlation needed: which
//! tool-call ids belong to the screen-control server, learned from the
//! `ToolCallStarted` that must precede any result for that id.
//!
//! [`ThreadEvent::ToolResultImages`]: oximux_agent_core::thread::ThreadEvent::ToolResultImages

use std::collections::VecDeque;

use oximux_agent_core::thread::ThreadEvent;
use serde_json::Value;

/// How many screen-control tool-call ids to remember.
///
/// Bounded because this lives for the length of a session and a chatty agent
/// makes thousands of calls. Generous relative to the distance between a call
/// and its result, which is one event in practice — the window only has to
/// outlive interleaving from parallel tool calls, not the whole conversation.
const REMEMBERED_CALLS: usize = 256;

/// Tracks which tool calls came from the screen-control server, so their images
/// can be dropped on the way out.
///
/// Deliberately not a `HashSet`: eviction order is what bounds it, and the ids
/// are consumed in roughly the order they arrive.
#[derive(Debug, Default)]
pub struct ScreenshotFilter {
    screen_calls: VecDeque<String>,
}

impl ScreenshotFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note what this event says, and rewrite it if it carries a screen capture.
    ///
    /// Returns `true` when something was removed, for a caller that wants to log
    /// it. The event is edited in place because every caller is about to send
    /// exactly this value onward.
    pub fn scrub(&mut self, event: &mut ThreadEvent) -> bool {
        match event {
            ThreadEvent::ToolCallStarted { id, name, .. } => {
                if crate::mcp::is_computer_use_tool(name) {
                    self.remember(id.clone());
                }
                false
            }
            ThreadEvent::ToolResultImages { tool_use_id, images } => {
                if !self.is_screen_call(tool_use_id) || images.is_empty() {
                    return false;
                }
                images.clear();
                true
            }
            _ => false,
        }
    }

    fn remember(&mut self, id: String) {
        if self.screen_calls.contains(&id) {
            return;
        }
        if self.screen_calls.len() == REMEMBERED_CALLS {
            self.screen_calls.pop_front();
        }
        self.screen_calls.push_back(id);
    }

    fn is_screen_call(&self, id: &str) -> bool {
        self.screen_calls.iter().any(|known| known == id)
    }
}

/// Drop screen-control images from a folded transcript, returning how many
/// entries were changed.
///
/// The fold has already done the correlation an event stream has not: a
/// `ToolCall` entry carries its `name` and its `images` together, so no state is
/// needed here.
///
/// # Why this walks the tree instead of reading the array elements
///
/// `ThreadEntry` is an ordinary externally-tagged enum, so a tool call reaches
/// the wire as `{"ToolCall": {"name": …, "images": […]}}` — the fields are one
/// level down, not on the array element. A first version of this read them off
/// the element and therefore matched nothing at all, while its unit tests passed
/// against a flat shape written by hand. The tests below now build real
/// [`ThreadEntry`] values for exactly that reason: the only shape worth
/// asserting against is the one the app actually serializes.
///
/// [`ThreadEntry`]: oximux_agent_core::thread::ThreadEntry
///
/// A transcript that will not parse is returned untouched and reported as zero.
/// That is the safe direction only because the caller's frame-budget step
/// behaves the same way and is what actually fails loudly — see the caller.
pub fn scrub_transcript(entries_json: &str) -> (String, usize) {
    let Ok(mut entries) = serde_json::from_str::<Value>(entries_json) else {
        return (entries_json.to_string(), 0);
    };
    let Some(array) = entries.as_array_mut() else {
        return (entries_json.to_string(), 0);
    };

    let mut scrubbed = 0usize;
    for entry in array.iter_mut() {
        scrub_entry(entry, &mut scrubbed);
    }

    if scrubbed == 0 {
        // Re-serializing an unchanged transcript would churn key order and byte
        // count for nothing, and the caller measures bytes.
        return (entries_json.to_string(), 0);
    }
    match serde_json::to_string(&entries) {
        Ok(json) => (json, scrubbed),
        // Cannot happen for a value that was just parsed, but returning the
        // original here would hand back the screenshots this exists to remove.
        Err(_) => ("[]".to_string(), scrubbed),
    }
}

/// Clear the images on every screen-control tool call reachable from `node`.
///
/// Recurses for the same reason the frame-budget pass downstream of it does: the
/// wire shape is the desktop's entry taxonomy, and a redactor that hardcodes
/// today's nesting stops working the day a variant moves — silently, and in the
/// direction that leaks. What it matches on is the *pair*, a tool `name` beside
/// an `images` array, which is a tool call wherever the fold decides to put it.
fn scrub_entry(node: &mut Value, scrubbed: &mut usize) {
    match node {
        Value::Object(map) => {
            let is_screen_call = map
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(crate::mcp::is_computer_use_tool);
            if is_screen_call
                && let Some(Value::Array(images)) = map.get_mut("images")
                && !images.is_empty()
            {
                images.clear();
                *scrubbed += 1;
                return;
            }
            for value in map.values_mut() {
                scrub_entry(value, scrubbed);
            }
        }
        Value::Array(items) => {
            for value in items.iter_mut() {
                scrub_entry(value, scrubbed);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agent_core::thread::{ChatImage, ThreadEntry, ToolCall};
    use serde_json::json;

    fn image() -> ChatImage {
        ChatImage {
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        }
    }

    fn started(id: &str, name: &str) -> ThreadEvent {
        ThreadEvent::ToolCallStarted {
            id: id.into(),
            name: name.into(),
            input: json!({}),
        }
    }

    fn images_for(id: &str) -> ThreadEvent {
        ThreadEvent::ToolResultImages {
            tool_use_id: id.into(),
            images: vec![image()],
        }
    }

    fn images_in(event: &ThreadEvent) -> usize {
        match event {
            ThreadEvent::ToolResultImages { images, .. } => images.len(),
            _ => 0,
        }
    }

    #[test]
    fn a_screenshot_is_dropped_on_the_way_out() {
        let mut filter = ScreenshotFilter::new();
        filter.scrub(&mut started("t1", "mcp__oximux-computer-use__screenshot"));

        let mut event = images_for("t1");
        assert!(filter.scrub(&mut event));
        assert_eq!(images_in(&event), 0);
    }

    #[test]
    fn an_image_the_user_asked_for_still_reaches_the_phone() {
        // The reason this is not "strip every image": a `Read` of a PNG is in
        // the conversation because the user put it there.
        let mut filter = ScreenshotFilter::new();
        filter.scrub(&mut started("t1", "Read"));

        let mut event = images_for("t1");
        assert!(!filter.scrub(&mut event));
        assert_eq!(images_in(&event), 1);
    }

    #[test]
    fn images_for_an_unknown_call_are_left_alone() {
        // A subscriber that joined mid-turn never saw the `ToolCallStarted`.
        // Passing the image is the wrong-but-chosen answer only because the
        // transcript path — which has the correlation — is what a phone
        // actually reads history from; this is the live edge.
        let mut filter = ScreenshotFilter::new();
        let mut event = images_for("never-seen");
        assert!(!filter.scrub(&mut event));
        assert_eq!(images_in(&event), 1);
    }

    #[test]
    fn interleaved_calls_do_not_confuse_each_other() {
        // Parallel tool calls are the normal case, so the correlation has to
        // survive a screenshot and an ordinary read being open at once.
        let mut filter = ScreenshotFilter::new();
        filter.scrub(&mut started("shot", "mcp__oximux-computer-use__zoom"));
        filter.scrub(&mut started("read", "Read"));

        let mut shot = images_for("shot");
        let mut read = images_for("read");
        assert!(filter.scrub(&mut shot));
        assert!(!filter.scrub(&mut read));
        assert_eq!(images_in(&shot), 0);
        assert_eq!(images_in(&read), 1);
    }

    #[test]
    fn the_remembered_set_stays_bounded() {
        // It lives as long as the session; a chatty agent makes thousands of
        // calls and this must not grow with them.
        let mut filter = ScreenshotFilter::new();
        for i in 0..REMEMBERED_CALLS * 3 {
            filter.scrub(&mut started(
                &format!("t{i}"),
                "mcp__oximux-computer-use__screenshot",
            ));
        }
        assert_eq!(filter.screen_calls.len(), REMEMBERED_CALLS);
        // And the most recent id — the one whose result is still in flight — is
        // the one that survived.
        let newest = format!("t{}", REMEMBERED_CALLS * 3 - 1);
        assert!(filter.is_screen_call(&newest));
    }

    #[test]
    fn a_repeated_id_does_not_consume_the_window_twice() {
        let mut filter = ScreenshotFilter::new();
        for _ in 0..10 {
            filter.scrub(&mut started("t1", "mcp__oximux-computer-use__screenshot"));
        }
        assert_eq!(filter.screen_calls.len(), 1);
    }

    /// A transcript in the shape the desktop actually publishes, built from the
    /// real types rather than written out by hand.
    ///
    /// This is not fussiness. The first version of [`scrub_transcript`] read
    /// `name` and `images` off the array element, which is where a hand-written
    /// fixture puts them and is *not* where `ThreadEntry` puts them — the enum
    /// is externally tagged, so they sit one level down under `"ToolCall"`. Every
    /// test passed and the redactor matched nothing on the only input that
    /// exists. A fixture that cannot drift from the serializer is the fix.
    fn folded(calls: &[(&str, &[&str])]) -> String {
        let entries: Vec<ThreadEntry> = calls
            .iter()
            .map(|(name, images)| {
                let mut tc = ToolCall::new("t", *name, json!({}));
                tc.images = images
                    .iter()
                    .map(|data| ChatImage {
                        media_type: "image/png".into(),
                        data: (*data).to_string(),
                    })
                    .collect();
                ThreadEntry::ToolCall(tc)
            })
            .collect();
        serde_json::to_string(&entries).expect("a folded transcript serializes")
    }

    #[test]
    fn a_folded_transcript_loses_only_the_screen_captures() {
        let transcript = folded(&[
            ("mcp__oximux-computer-use__get_window_state", &["AAAA"]),
            ("Read", &["BBBB"]),
        ]);

        let (scrubbed, count) = scrub_transcript(&transcript);
        assert_eq!(count, 1);
        assert!(!scrubbed.contains("AAAA"), "the screenshot must be gone");
        assert!(scrubbed.contains("BBBB"), "the user's own image must remain");
    }

    /// The regression that matters, stated on its own: the fields the redactor
    /// reads are nested inside the variant tag, and a version that misses that
    /// is indistinguishable from one that works until a screenshot reaches a
    /// phone.
    #[test]
    fn a_capture_is_found_through_the_entry_variant_tag() {
        let transcript = folded(&[("mcp__oximux-computer-use__zoom", &["AAAA"])]);
        assert!(
            transcript.contains("\"ToolCall\""),
            "the fold is externally tagged, which is the whole point: {transcript}"
        );
        let (scrubbed, count) = scrub_transcript(&transcript);
        assert_eq!(count, 1, "nested fields must still be reached");
        assert!(!scrubbed.contains("AAAA"));
    }

    #[test]
    fn a_transcript_with_nothing_to_remove_is_returned_byte_identical() {
        // The caller measures bytes against a frame budget, so re-serializing
        // would change the number it is about to check.
        let transcript = folded(&[("Bash", &[])]);
        let (out, count) = scrub_transcript(&transcript);
        assert_eq!(count, 0);
        assert_eq!(out, transcript);
    }

    /// An assistant message carries no tool name at all, and must survive the
    /// walk untouched rather than tripping the name/images pair check.
    #[test]
    fn entries_that_are_not_tool_calls_are_left_alone() {
        let entries = vec![
            ThreadEntry::ContextCompaction { summary: "earlier history".into() },
            ThreadEntry::User {
                text: "look at this".into(),
                images: vec![image()],
                checkpoint: None,
            },
        ];
        let transcript = serde_json::to_string(&entries).expect("serializes");
        let (out, count) = scrub_transcript(&transcript);
        assert_eq!(count, 0);
        assert_eq!(out, transcript, "the user's own attachment stays");
    }

    #[test]
    fn an_unparseable_transcript_is_not_silently_emptied() {
        let (out, count) = scrub_transcript("{ not json");
        assert_eq!(out, "{ not json");
        assert_eq!(count, 0);
    }
}
