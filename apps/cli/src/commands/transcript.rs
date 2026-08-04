//! `oximux transcript` — the session's full folded transcript, reassembled
//! from `FetchTranscriptPage` pages so no transcript is too large to fetch
//! (the legacy single-frame verb dies at the transport's frame cap).

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// A whole transcript, reassembled. `--output-schema` reads the same pages the
/// `transcript` verb prints, so "what the agent finally said" means one thing.
pub struct Transcript {
    pub entries: Vec<Value>,
    /// The fold cursor the last page reported.
    pub seq: u64,
    pub model: Option<String>,
    pub pages: u64,
}

/// Page the whole folded transcript into one vector.
pub async fn fetch_entries(client: &Client, session: &str) -> Result<Transcript, Failure> {
    let mut entries: Vec<Value> = Vec::new();
    let mut cursor = 0u64;
    let mut seq;
    let mut model;
    let mut pages = 0u64;
    loop {
        let reply = client
            .call(Request::FetchTranscriptPage {
                session_id: session.into(),
                cursor,
                limit: 256,
            })
            .await?;
        let page = match reply {
            Response::TranscriptPage(page) => page,
            Response::Error(e) => return Err(rpc_failure(e)),
            other => return Err(unexpected_reply("FetchTranscriptPage", &other)),
        };
        let mut chunk: Vec<Value> = serde_json::from_str(&page.entries_json).map_err(|e| {
            Failure::new("decode", exit::ERROR, format!("undecodable transcript page: {e}"))
        })?;
        entries.append(&mut chunk);
        seq = page.seq;
        model = page.model;
        pages += 1;
        match page.next_cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }
    Ok(Transcript { entries, seq, model, pages })
}

pub async fn run(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    let transcript = fetch_entries(client, session).await?;
    let human = if transcript.entries.is_empty() {
        "empty transcript".to_string()
    } else {
        transcript
            .entries
            .iter()
            .filter_map(crate::render::render_entry)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let data = json!({
        "session_id": session,
        "seq": transcript.seq,
        "model": transcript.model,
        "pages": transcript.pages,
        "entries": transcript.entries,
    });
    Ok((data, human))
}
