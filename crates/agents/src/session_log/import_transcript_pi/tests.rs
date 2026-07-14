//! Fixture-driven tests: happy-path transcript shape plus corrupt/missing-file
//! degrade paths.

use super::*;

/// The scrubbed real-shaped rollout in `testdata/pi_import_fixture.jsonl`
/// (session header + model/thinking-level plumbing + two user turns and one
/// assistant turn with a thinking block; placeholder text throughout).
const FIXTURE_JSONL: &str = include_str!("../testdata/pi_import_fixture.jsonl");

#[test]
fn maps_user_and_assistant_turns_skipping_plumbing_lines() {
    let entries = transcript_from_pi_str(FIXTURE_JSONL);
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert_eq!(
        entries[0],
        ThreadEntry::User {
            text: "placeholder prompt one".into(),
            images: Vec::new(),
            checkpoint: None,
        }
    );
    match &entries[1] {
        ThreadEntry::Assistant(m) => {
            assert_eq!(m.text, "placeholder reply one");
            assert_eq!(m.thinking, "placeholder reasoning");
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
    // A bare-string content body (no block array) still maps.
    assert_eq!(
        entries[2],
        ThreadEntry::User {
            text: "placeholder prompt two".into(),
            images: Vec::new(),
            checkpoint: None,
        }
    );
}

#[test]
fn missing_file_yields_empty_transcript_not_a_panic() {
    let entries = pi_transcript(Path::new("/nonexistent/pi/session.jsonl"));
    assert!(entries.is_empty());
}

#[test]
fn corrupt_and_foreign_lines_are_skipped_not_fatal() {
    let raw = concat!(
        "not json at all\n",
        r#"{"type":"session","id":"x"}"#,
        "\n",
        r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"still readable"}]}}"#,
        "\n",
        "{ broken json\n",
    );
    let entries = transcript_from_pi_str(raw);
    assert_eq!(
        entries,
        vec![ThreadEntry::User { text: "still readable".into(), images: Vec::new(), checkpoint: None }]
    );
}

#[test]
fn empty_file_yields_empty_transcript() {
    assert!(transcript_from_pi_str("").is_empty());
}

#[test]
fn reads_a_real_file_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(&path, FIXTURE_JSONL).unwrap();
    let entries = pi_transcript(&path);
    assert_eq!(entries.len(), 3);
}
