//! Pinned transcripts for every committed Claude fixture.
//!
//! This is the baseline the assembler extraction (plan phase 4) is measured
//! against: the mappers are about to be rewritten, and the only thing that must
//! survive unchanged is what the user ends up seeing. Each fixture is decoded
//! with the real decoder, folded with the real fold, and compared against a
//! committed snapshot of the resulting transcript.
//!
//! A failure here means the rewrite changed rendered output. That is sometimes
//! correct — a round-9 fidelity fix, say — but it is never incidental, so the
//! snapshot must be regenerated deliberately and the change named in the phase
//! log:
//!
//! ```text
//! UPDATE_TRANSCRIPT_SNAPSHOTS=1 cargo test -p oximux-agent-core --features test-support
//! ```

use oximux_agent_core::thread::event::ThreadEvent;
use oximux_agent_core::thread::snapshot::assert_transcript_snapshot;
use oximux_agent_core::thread::stream_json::decode_line;

/// Every fixture in `src/thread/testdata/`. Listed rather than globbed so a new
/// fixture is a deliberate addition to this baseline — a glob would silently
/// pin whatever happened to be on disk, including a half-captured file.
const FIXTURES: &[&str] = &[
    "stream_json_error_api",
    "stream_json_error_stale_resume",
    "stream_json_exit_plan_mode",
    "stream_json_rate_limit",
    "stream_json_richtools",
    "stream_json_subagent",
    "stream_json_tool_input_delta",
];

fn decode_fixture(name: &str) -> Vec<ThreadEvent> {
    let path = format!("{}/src/thread/testdata/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    raw.lines().filter(|l| !l.trim().is_empty()).flat_map(decode_line).collect()
}

#[test]
fn every_claude_fixture_renders_the_pinned_transcript() {
    for name in FIXTURES {
        let events = decode_fixture(name);
        let path =
            format!("{}/tests/snapshots/{name}.transcript.json", env!("CARGO_MANIFEST_DIR"));
        assert_transcript_snapshot(path, &events);
    }
}

/// The baseline is only worth having if every fixture actually reaches the
/// decoder. A fixture that silently decodes to nothing would pin an empty
/// transcript and then "pass" forever, which is the shape of coverage that
/// isn't.
#[test]
fn every_fixture_decodes_to_at_least_one_event() {
    for name in FIXTURES {
        assert!(
            !decode_fixture(name).is_empty(),
            "{name} decoded to nothing — the fixture or the decoder is broken"
        );
    }
}
