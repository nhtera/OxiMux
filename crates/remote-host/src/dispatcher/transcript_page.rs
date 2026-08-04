//! The paginated folded-transcript fetch (`FetchTranscriptPage`, v16).
//!
//! The v13 `FetchTranscript` returns the whole snapshot in one frame, which
//! works until a long session's fold outgrows the transport's 16 MB frame cap —
//! at which point the client loses the *entire* history, not the part that
//! overflowed. This handler serves the same snapshot in entry-indexed pages,
//! each bounded well under the cap, so any transcript is fetchable. The legacy
//! verb stays untouched (postcard payloads may never be reshaped) and v15
//! phones keep using it.

use oximux_remote_proto::messages::TranscriptPageWire;
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

/// Byte budget for one page's `entries_json`, well under the 16 MB transport
/// frame cap. The margin absorbs the postcard envelope and the page's other
/// fields; images inside a page are additionally trimmed by the same
/// [`fit_images`](crate::transcript_budget::fit_images) budget the legacy path
/// applies, so one attachment-heavy entry cannot blow past this on its own.
const PAGE_BUDGET: usize = 4 * 1024 * 1024;

/// The most entries one page may carry regardless of the client's ask, so a
/// hostile `limit` cannot make the host serialize an unbounded page.
const MAX_PAGE_ENTRIES: usize = 1024;

/// The hard per-entry ceiling. [`PAGE_BUDGET`] is a soft target — a single
/// entry between it and this ceiling still ships alone on its own page — but
/// an entry above this cannot cross the transport at all (the 16 MB frame cap
/// minus envelope margin), and `fit_images` only trims *image* payloads, never
/// text. Such an entry is substituted with a visible marker: losing one
/// pathological entry with a note beats losing the whole fetch to a frame
/// error, which is the exact failure pagination exists to remove.
const MAX_ENTRY_BYTES: usize = 12 * 1024 * 1024;

/// The stand-in for an entry too large to transfer. A `ContextCompaction` is
/// the fold's existing "history elided here" vocabulary, so every client
/// already renders it as a divider rather than choking on an unknown shape.
fn oversize_marker(bytes: usize) -> serde_json::Value {
    serde_json::json!({
        "ContextCompaction": {
            "summary": format!(
                "one transcript entry ({} MB) exceeded the transfer limit and was omitted",
                bytes / (1024 * 1024)
            )
        }
    })
}

impl Dispatcher {
    /// Serve one page of a session's folded transcript. Same source and same
    /// scrubbing as [`fetch_transcript`](Dispatcher::fetch_transcript) — a live
    /// session's published fold, else the catalog's persisted copy — sliced by
    /// entry index and bounded by [`PAGE_BUDGET`].
    pub(super) fn fetch_transcript_page(
        &self,
        peer: &Peer,
        session_id: &str,
        cursor: u64,
        limit: u32,
    ) -> Response {
        if !self.auth.is_allowed_for(peer, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let (seq, entries_json, model) = match self.registry.get(session_id) {
            Some(handle) => {
                let snap = handle.transcript_snapshot().unwrap_or_default();
                (snap.seq, snap.entries_json, snap.model)
            }
            None => match self.catalog.as_ref().and_then(|c| c.transcript(session_id)) {
                Some(t) => (0, t.entries_json, t.model),
                None => return Response::Error(RpcError::UnknownSession),
            },
        };
        let entries_json = if entries_json.is_empty() { "[]".to_string() } else { entries_json };
        // Scrub before slicing: the dormant branch reads straight off disk where
        // the desktop's own transcript keeps its screen captures, exactly as on
        // the legacy path.
        let (entries_json, captures) = oximux_agent_core::redact::scrub_transcript(&entries_json);
        if captures > 0 {
            tracing::debug!(session_id, captures, "dropped screen captures from stored history");
        }
        let entries: Vec<serde_json::Value> = match serde_json::from_str(&entries_json) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(session_id, %err, "stored transcript is not a JSON array");
                return Response::Error(RpcError::Internal("transcript decode failed".into()));
            }
        };
        let total = entries.len();
        // A cursor at/past the end answers an empty final page rather than an
        // error: the client's paging loop terminates on `next_cursor: None`
        // either way, and a fold that shrank under it (a rewind) is not misuse.
        let start = (cursor as usize).min(total);
        // `limit` is a client hint, clamped both ways: at least one entry per
        // page (or a 0 ask would loop forever on a stuck cursor), at most
        // [`MAX_PAGE_ENTRIES`].
        let limit = (limit as usize).clamp(1, MAX_PAGE_ENTRIES);

        // Owned rather than borrowed: substitution (below) mints a value that
        // is not in `entries`, and a page is bounded at [`PAGE_BUDGET`] so the
        // clone is cheap on a path that runs per fetch, not per event.
        let mut taken = 0usize;
        let mut bytes = 0usize;
        let mut page: Vec<serde_json::Value> = Vec::new();
        for entry in entries.iter().skip(start).take(limit) {
            // Estimate is exact: the page re-serializes these same values.
            let entry_bytes = serde_json::to_string(entry).map(|s| s.len()).unwrap_or(0);
            // Always take at least one entry, or an oversized entry would pin
            // the cursor in place and the client's loop would never advance.
            if taken > 0 && bytes + entry_bytes > PAGE_BUDGET {
                break;
            }
            // An entry no frame can carry is substituted, not shipped — see
            // [`MAX_ENTRY_BYTES`]. Substitution still advances the cursor.
            if entry_bytes > MAX_ENTRY_BYTES {
                tracing::warn!(
                    session_id,
                    entry_bytes,
                    "transcript entry exceeds the transfer limit; substituting a marker"
                );
                taken += 1;
                page.push(oversize_marker(entry_bytes));
                break;
            }
            bytes += entry_bytes;
            taken += 1;
            page.push(entry.clone());
        }
        let page_json = match serde_json::to_string(&page) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(session_id, %err, "transcript page encode failed");
                return Response::Error(RpcError::Internal("transcript encode failed".into()));
            }
        };
        // Trim image payloads exactly as the legacy path does — per page, since
        // each page is its own frame.
        let (page_json, stripped) = crate::transcript_budget::fit_images(
            &page_json,
            crate::transcript_budget::IMAGE_BUDGET,
        );
        if stripped > 0 {
            tracing::debug!(
                session_id,
                stripped,
                "dropped image data from older transcript entries to fit one page"
            );
        }
        let next_cursor = (start + taken < total).then(|| (start + taken) as u64);
        Response::TranscriptPage(TranscriptPageWire {
            session_id: session_id.to_string(),
            seq,
            entries_json: page_json,
            next_cursor,
            total: total as u64,
            model,
        })
    }
}
