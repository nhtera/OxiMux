//! Keeps a transcript reply inside what one transport frame can carry.
//!
//! A folded transcript inlines its attachments as base64, so a session grows by
//! however many megabytes of photos have ever been sent to it — and unlike the
//! text, that growth has no natural ceiling. Past the frame cap the reply cannot
//! be assembled at all, which costs the client its whole history rather than the
//! part that overflowed.
//!
//! So the budget is spent on the images, newest first: those are the ones a client
//! is about to draw, and an older one is far more likely to have scrolled out of
//! reach than to be missed. An image beyond the budget keeps its entry and its
//! media type and loses only `data`, which is what lets the client say an image was
//! here instead of leaving a gap that reads as a rendering fault.
//!
//! Deliberately generic over the entry shape: the registry hands this layer the
//! fold as opaque JSON precisely so the wire never has to track the desktop's entry
//! taxonomy, and re-introducing that dependency here to save one tree walk would
//! trade a structural property for a micro-optimization on a cold path.

use serde_json::Value;

/// How many bytes of inlined image data one transcript reply may carry.
///
/// Half of the 16 MB frame the iroh transport will assemble, leaving the text half
/// of a transcript — which this cannot shed — the same room again before anything
/// is at risk. Written as a literal rather than derived from `MAX_FRAME` because
/// this crate is transport-agnostic on purpose: it is driven entirely through the
/// `remote-proto` seam so a WebSocket carrier stays a drop-in, and depending on a
/// concrete transport to read one number would give that up. A carrier with a
/// tighter frame would want this lowered, which is a deliberate edit, not a
/// silent recompile.
pub const IMAGE_BUDGET: usize = 8 * 1024 * 1024;

/// Drop inlined image data from the oldest entries until the rest fits `budget`,
/// returning the trimmed JSON and how many images were stripped.
///
/// The input is returned untouched when it already fits, when it is under the
/// budget outright, or when it is not the array this expects — a transcript that
/// cannot be parsed is still better delivered than replaced with nothing.
pub fn fit_images(entries_json: &str, budget: usize) -> (String, usize) {
    // The common case by far: no attachments at all, or few enough that the whole
    // fold is smaller than the budget. Skip the parse entirely.
    if entries_json.len() <= budget {
        return (entries_json.to_string(), 0);
    }
    let Ok(mut entries) = serde_json::from_str::<Value>(entries_json) else {
        return (entries_json.to_string(), 0);
    };
    let Some(array) = entries.as_array_mut() else {
        return (entries_json.to_string(), 0);
    };

    // Newest first: spend the budget on what the client is about to render.
    let mut spent = 0usize;
    let mut stripped = 0usize;
    for entry in array.iter_mut().rev() {
        strip_images(entry, budget, &mut spent, &mut stripped);
    }
    if stripped == 0 {
        return (entries_json.to_string(), 0);
    }
    match serde_json::to_string(&entries) {
        Ok(json) => (json, stripped),
        Err(_) => (entries_json.to_string(), 0),
    }
}

/// Walk one entry, clearing `data` on every image past the budget.
///
/// Recurses rather than matching a known entry shape: images ride user messages
/// today and tool results too, and a new carrier should be covered the day it is
/// added rather than the day someone notices it was missed.
fn strip_images(node: &mut Value, budget: usize, spent: &mut usize, stripped: &mut usize) {
    match node {
        Value::Object(map) => {
            // An image is an object carrying base64 `data` beside its `media_type`
            // — matching on the pair rather than on `data` alone, so an unrelated
            // field of that name is never mistaken for an attachment.
            let is_image = map.contains_key("media_type") && map.contains_key("data");
            if is_image {
                if let Some(Value::String(data)) = map.get("data") {
                    let len = data.len();
                    if *spent + len <= budget {
                        *spent += len;
                    } else {
                        map.insert("data".into(), Value::String(String::new()));
                        *stripped += 1;
                    }
                }
                return;
            }
            for value in map.values_mut() {
                strip_images(value, budget, spent, stripped);
            }
        }
        Value::Array(items) => {
            for value in items.iter_mut() {
                strip_images(value, budget, spent, stripped);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry carrying `n` images of `size` bytes of base64 each.
    fn user_entry(n: usize, size: usize) -> Value {
        let images: Vec<Value> = (0..n)
            .map(|_| serde_json::json!({ "media_type": "image/jpeg", "data": "A".repeat(size) }))
            .collect();
        serde_json::json!({ "User": { "text": "look", "images": images } })
    }

    fn transcript(entries: Vec<Value>) -> String {
        serde_json::to_string(&Value::Array(entries)).unwrap()
    }

    /// Count the images that still carry data, and those that were emptied.
    fn tally(json: &str) -> (usize, usize) {
        let v: Value = serde_json::from_str(json).unwrap();
        let (mut kept, mut empty) = (0, 0);
        fn walk(v: &Value, kept: &mut usize, empty: &mut usize) {
            match v {
                Value::Object(m) => {
                    if let (true, Some(Value::String(d))) =
                        (m.contains_key("media_type"), m.get("data"))
                    {
                        if d.is_empty() { *empty += 1 } else { *kept += 1 }
                        return;
                    }
                    for x in m.values() {
                        walk(x, kept, empty)
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        walk(x, kept, empty)
                    }
                }
                _ => {}
            }
        }
        walk(&v, &mut kept, &mut empty);
        (kept, empty)
    }

    /// A transcript already under budget is returned byte-for-byte — no reparse,
    /// no reserialization, no key reordering for a client diffing against it.
    #[test]
    fn a_small_transcript_is_untouched() {
        let json = transcript(vec![user_entry(1, 100)]);
        let (out, stripped) = fit_images(&json, 1024 * 1024);
        assert_eq!(out, json);
        assert_eq!(stripped, 0);
    }

    /// The bug this guards: five full-resolution photos in one session pushed the
    /// reply past the frame cap, and the client could then fetch no history at all.
    #[test]
    fn the_newest_images_are_kept_and_the_oldest_dropped() {
        // Five 1 KB images across five entries, with room for three.
        let entries: Vec<Value> = (0..5).map(|_| user_entry(1, 1000)).collect();
        let json = transcript(entries);

        let (out, stripped) = fit_images(&json, 3000);
        assert_eq!(stripped, 2, "the two oldest lost their data");
        assert_eq!(tally(&out), (3, 2));

        // And the result is now small enough to send.
        assert!(out.len() < json.len());
    }

    /// A stripped image keeps its entry and media type — the client has to be able
    /// to say "an image was here", which a removed element could not express.
    #[test]
    fn a_stripped_image_keeps_its_place_and_type() {
        let json = transcript(vec![user_entry(1, 5000), user_entry(1, 100)]);
        let (out, _) = fit_images(&json, 200);

        let v: Value = serde_json::from_str(&out).unwrap();
        let first = &v[0]["User"]["images"][0];
        assert_eq!(first["media_type"], "image/jpeg", "type survives");
        assert_eq!(first["data"], "", "only the payload is gone");
        assert_eq!(v[0]["User"]["text"], "look", "the message text survives");
        assert_eq!(v.as_array().unwrap().len(), 2, "no entry was dropped");
    }

    /// Several images in one entry are budgeted individually, not all-or-nothing.
    #[test]
    fn images_within_one_entry_are_budgeted_separately() {
        let json = transcript(vec![user_entry(4, 1000)]);
        let (out, stripped) = fit_images(&json, 2000);
        assert_eq!(stripped, 2);
        assert_eq!(tally(&out), (2, 2));
    }

    /// A budget too small for even one image strips them all rather than looping or
    /// emitting something oversize anyway.
    #[test]
    fn a_budget_below_one_image_strips_everything() {
        let json = transcript(vec![user_entry(2, 1000)]);
        let (out, stripped) = fit_images(&json, 0);
        assert_eq!(stripped, 2);
        assert_eq!(tally(&out), (0, 2));
    }

    /// A transcript that is large but carries no images has nothing to shed; it
    /// must come back intact rather than mangled by a failed trim.
    #[test]
    fn a_large_text_only_transcript_is_returned_intact() {
        let json = transcript(vec![serde_json::json!({ "Assistant": { "text": "x".repeat(5000) } })]);
        let (out, stripped) = fit_images(&json, 100);
        assert_eq!(stripped, 0);
        assert_eq!(out, json);
    }

    /// Unparseable or unexpected JSON is passed through: a transcript we cannot
    /// trim is still worth more to the client than an empty one.
    #[test]
    fn malformed_input_is_passed_through() {
        for input in ["not json at all", "{\"not\":\"an array\"}"] {
            let (out, stripped) = fit_images(input, 0);
            assert_eq!(out, input);
            assert_eq!(stripped, 0);
        }
    }

    /// A field named `data` that is not part of an image must not be emptied.
    #[test]
    fn a_non_image_data_field_is_left_alone() {
        let json = transcript(vec![serde_json::json!({
            "Tool": { "name": "read", "data": "x".repeat(5000) }
        })]);
        let (out, stripped) = fit_images(&json, 10);
        assert_eq!(stripped, 0);
        assert_eq!(out, json);
    }
}
