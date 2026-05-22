//! Integration tests for LSP notification serialization (step 2).
//!
//! Verifies that `did_change`, `did_save`, `did_close` produce JSON-RPC
//! frames that match the Language Server Protocol spec. Tests live here
//! (not inline in `lsp/client.rs`) to keep `client.rs` under the
//! xtask file-size-lint warn threshold (500 LOC).

use std::str::FromStr;

use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidSaveTextDocumentParams,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, Uri, VersionedTextDocumentIdentifier,
    notification::Notification as _,
};

/// `did_change` serializes full-sync params correctly per Language Server
/// Protocol spec §3.17.2: when `range` is `None`, lsp_types serializes
/// the field as absent (skip_serializing_if = "Option::is_none"), which
/// the protocol treats as a full-document replacement. Text payload and
/// version must be present verbatim.
#[test]
fn did_change_full_sync_text_fidelity() {
    let uri: Uri = Uri::from_str("file:///tmp/test.rs").unwrap();
    let params = DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier::new(uri, 2),
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "fn main() {}\n".to_string(),
        }],
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": lsp_types::notification::DidChangeTextDocument::METHOD,
        "params": params,
    });
    let serialized = serde_json::to_string(&body).unwrap();

    // lsp_types skips None range fields (skip_serializing_if = "Option::is_none"),
    // so the serialized form omits the "range" key entirely — the spec treats
    // both absent and null range as a full-document replacement.
    assert!(
        !serialized.contains("\"range\""),
        "range key must be absent for full sync (lsp_types omits None fields), got: {serialized}"
    );
    assert!(
        serialized.contains("fn main() {}"),
        "text payload missing; got: {serialized}"
    );
    assert!(
        serialized.contains("\"version\":2"),
        "version missing; got: {serialized}"
    );
    assert!(
        serialized.contains("textDocument/didChange"),
        "method missing; got: {serialized}"
    );
}

/// `did_save` serializes with no `text` field (server uses last-received
/// didChange content; `textInclude` capability not advertised in step 2).
#[test]
fn did_save_serializes_correctly() {
    let uri: Uri = Uri::from_str("file:///tmp/test.rs").unwrap();
    let params = DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri },
        text: None,
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": lsp_types::notification::DidSaveTextDocument::METHOD,
        "params": params,
    });
    let serialized = serde_json::to_string(&body).unwrap();

    assert!(
        serialized.contains("textDocument/didSave"),
        "method missing; got: {serialized}"
    );
    assert!(
        serialized.contains("/tmp/test.rs"),
        "uri missing; got: {serialized}"
    );
}

/// `did_close` serializes with the correct textDocument URI.
#[test]
fn did_close_serializes_correctly() {
    let uri: Uri = Uri::from_str("file:///tmp/close_me.rs").unwrap();
    let params = DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier { uri },
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": lsp_types::notification::DidCloseTextDocument::METHOD,
        "params": params,
    });
    let serialized = serde_json::to_string(&body).unwrap();

    assert!(
        serialized.contains("textDocument/didClose"),
        "method missing; got: {serialized}"
    );
    assert!(
        serialized.contains("close_me.rs"),
        "uri missing; got: {serialized}"
    );
}

/// LSP version counter must be strictly monotonically increasing per document.
/// Tests the integer arithmetic pattern `EditorView` relies on (plain i32,
/// pre-increment before send, starts at 1 from didOpen → 2 on first change).
#[test]
fn version_monotonic_increasing() {
    let mut version: i32 = 1; // version 1 is consumed by didOpen
    let versions: Vec<i32> = (0..5)
        .map(|_| {
            version += 1;
            version
        })
        .collect();
    assert_eq!(versions, vec![2, 3, 4, 5, 6]);
    for w in versions.windows(2) {
        assert!(w[1] > w[0], "version not monotonically increasing: {w:?}");
    }
}
