//! Receipts for the lines a host sends (`command_lifecycle`, a result's
//! `user_message_uuids`).

use serde_json::json;

use super::*;

/// Fixture lines from the CLI (2.1.310): a line sent under a uuid is
/// queued, started, and reported by the turn's result.
#[test]
fn a_sent_line_is_accepted_on_started_and_delivered_by_the_result() {
    let queued = json!({"type":"command_lifecycle","command_uuid":"00dad500-b85d-4a13-a756-c904e97ad49a",
        "state":"queued","uuid":"bda62ddd-3831-455a-9382-c86a16a9a9f0","session_id":"s"}).to_string();
    assert!(decode_line(&queued).is_empty(), "queued is not a receipt");
    let started = json!({"type":"command_lifecycle","command_uuid":"00dad500-b85d-4a13-a756-c904e97ad49a",
        "state":"started","uuid":"33909035-2347-46a7-b807-b7a49593b817","session_id":"s"}).to_string();
    assert_eq!(
        decode_line(&started),
        vec![ThreadEvent::UserMessageAccepted { uuid: "00dad500-b85d-4a13-a756-c904e97ad49a".into() }],
        "keyed by command_uuid, not the frame's own uuid"
    );
    let result = json!({"type":"result","subtype":"success","num_turns":1,
        "user_message_uuid":"00dad500-b85d-4a13-a756-c904e97ad49a",
        "user_message_uuids":["00dad500-b85d-4a13-a756-c904e97ad49a"],
        "queued_turn_count":0,"terminal_reason":"completed","stop_reason":"end_turn"}).to_string();
    let evs = decode_line(&result);
    assert_eq!(
        evs[0],
        ThreadEvent::UserMessagesDelivered { uuids: vec!["00dad500-b85d-4a13-a756-c904e97ad49a".into()] }
    );
    assert!(matches!(evs[1], ThreadEvent::TurnEnded { is_error: false, .. }), "just ahead of its turn end");
    assert!(evs[0].is_receipt() && !evs[1].is_receipt());
    let dropped = json!({"type":"command_lifecycle","command_uuid":"u-2","state":"cancelled","uuid":"f"}).to_string();
    assert_eq!(decode_line(&dropped), vec![ThreadEvent::UserMessageDropped { uuid: "u-2".into() }]);
    // A result with no uuids (an older CLI) carries no receipt.
    let plain = json!({"type":"result","subtype":"success"}).to_string();
    assert!(decode_line(&plain).iter().all(|e| !e.is_receipt()));
}
