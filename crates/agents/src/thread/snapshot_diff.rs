//! Snapshot → delta helpers shared by the Pi-family mappers.
//!
//! pi's `message_update` (and omp's — same fork lineage, re-verified live on
//! omp 18.0.4) re-sends the WHOLE accumulated message on every tick rather
//! than a thin delta, measured at 137.8× amplification. The mappers diff those
//! snapshots down to the suffix the UI has not seen yet; these are the pure,
//! untyped pieces of that diffing, extracted so the two mappers cannot drift.
//!
//! Scope is deliberately small (red-team F9): the helpers are map-level and
//! untyped; each adapter keeps its own event dispatch and state around them.

use serde_json::Value;

/// The not-yet-emitted suffix of `full` relative to `seen`, advancing `seen`.
///
/// `None` when there is nothing new to emit: `full` no longer extends `seen`
/// (a rewrite — the caller's authoritative `*_end` reconciliation covers it,
/// and `seen` is left alone so that reconciliation still sees what was
/// actually emitted), or the suffix is empty.
pub(crate) fn suffix_delta(seen: &mut String, full: String) -> Option<String> {
    let suffix = full.strip_prefix(seen.as_str())?;
    if suffix.is_empty() {
        return None;
    }
    let suffix = suffix.to_string();
    *seen = full;
    Some(suffix)
}

/// The text of `content[ci]` in a partial message snapshot — the Pi family
/// names it `text` on a text block and `thinking` on a thinking block.
pub(crate) fn block_text(partial: Option<&Value>, ci: u64) -> Option<String> {
    let block = partial?.get("content")?.as_array()?.get(ci as usize)?;
    let s = block
        .get("text")
        .or_else(|| block.get("thinking"))
        .and_then(Value::as_str)?;
    Some(s.to_string())
}

/// Flatten a result/partialResult's `content[]` to its text.
pub(crate) fn content_text(v: Option<&Value>) -> Option<String> {
    let arr = v?.get("content")?.as_array()?;
    Some(
        arr.iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn suffix_delta_emits_each_new_suffix_exactly_once() {
        let mut seen = String::new();
        assert_eq!(suffix_delta(&mut seen, "Hel".into()).as_deref(), Some("Hel"));
        assert_eq!(suffix_delta(&mut seen, "Hello".into()).as_deref(), Some("lo"));
        // An identical snapshot has nothing new.
        assert_eq!(suffix_delta(&mut seen, "Hello".into()), None);
        assert_eq!(seen, "Hello");
    }

    #[test]
    fn a_rewrite_emits_nothing_and_keeps_the_emitted_record() {
        // Text is verified append-only, so a non-extension is a rewrite the
        // `*_end` reconciliation handles; `seen` must keep recording what WAS
        // emitted so that reconciliation can tell agreement from disagreement.
        let mut seen = String::from("Hello");
        assert_eq!(suffix_delta(&mut seen, "Goodbye".into()), None);
        assert_eq!(seen, "Hello");
    }

    #[test]
    fn block_text_reads_text_and_thinking_blocks() {
        let partial = json!({"content": [
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "hi"}
        ]});
        assert_eq!(block_text(Some(&partial), 0).as_deref(), Some("hmm"));
        assert_eq!(block_text(Some(&partial), 1).as_deref(), Some("hi"));
        assert_eq!(block_text(Some(&partial), 2), None);
        assert_eq!(block_text(None, 0), None);
    }

    #[test]
    fn content_text_joins_only_text_blocks() {
        let v = json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "image", "data": "…"},
            {"type": "text", "text": "b"}
        ]});
        assert_eq!(content_text(Some(&v)).as_deref(), Some("ab"));
        assert_eq!(content_text(None), None);
    }
}
