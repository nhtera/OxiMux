//! `oximux attach` — and the shared session-streaming loop `run`, `send`, and
//! `wait` compose. One loop owns the whole contract: Subscribe + backlog
//! replay, seq-gap detection with EventsSince recovery, transcript resync with
//! a printed marker when the backlog can no longer fill the gap, Ctrl+C as
//! detach-not-stop, and NDJSON emission under `--json`.

use std::io::Write as _;

use oximux_agent_core::thread::ThreadEvent;
use oximux_remote_proto::messages::HostEvent;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// What ends a streaming loop (besides Ctrl+C and the transport closing).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Stream until detached (Ctrl+C) — `attach`.
    Never,
    /// Stop when a turn completes — `run`/`send` foreground, `wait --until done`.
    TurnEnded,
    /// Stop when a permission request or question is awaiting a decision.
    NeedsApproval,
}

/// How a streaming loop ended.
pub enum StreamEnd {
    /// A turn completed (`is_error` mirrors the backend's verdict).
    TurnEnded { is_error: bool },
    /// The condition [`Stop::NeedsApproval`] was met.
    NeedsApproval,
    /// The user detached with Ctrl+C; the agent keeps running.
    Detached,
    /// The caller's deadline passed before the condition was met.
    Deadline,
}

/// How long a stream may be silent before it says it is still there.
///
/// A blocking verb that prints nothing is indistinguishable from a wedged one,
/// and the callers most likely to be watching are supervisors that give up on
/// quiet children — Claude Code backgrounds a subprocess after roughly two
/// minutes of silence. Well under that, and long enough that an ordinary agent
/// turn produces no keepalives at all.
const KEEPALIVE_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

/// Say the stream is alive, on **stderr**.
///
/// Never stdout: `--json` promises stdout is the event stream and then one
/// result object, and a liveness line is neither. A caller that wants these
/// reads stderr; a caller that does not is unaffected by them existing.
///
/// Emitted regardless of `quiet`, which is the point — `wait` renders no events
/// at all, so it is the verb whose silence is least distinguishable from a hang.
fn emit_keepalive(json_mode: bool, session: &str, waited: std::time::Duration) {
    let secs = waited.as_secs();
    if json_mode {
        eprintln!("{}", json!({ "type": "keepalive", "session_id": session, "waited_secs": secs }));
    } else {
        eprintln!("· still waiting on {session} ({secs}s, no events)");
    }
}

/// Print one event — a compact line for humans, one NDJSON line under
/// `--json`. `quiet` suppresses output entirely (`wait` watches, not renders).
fn emit(json_mode: bool, quiet: bool, frame: &HostEvent) {
    if quiet {
        return;
    }
    match frame.event() {
        Ok(event) => {
            if json_mode {
                // ThreadEvent serializes losslessly, so the NDJSON line carries
                // the full event, not the compact human digest.
                let line = json!({
                    "seq": frame.seq,
                    "session_id": frame.session_id,
                    "awaiting_permission": frame.status.awaiting_permission,
                    "event": serde_json::to_value(&event).unwrap_or(Value::Null),
                });
                println!("{line}");
            } else if let Some(line) = crate::render::render_event(&event) {
                println!("{line}");
            }
        }
        // A newer host may stream an event vocabulary this build predates.
        // Mark it rather than dropping it silently or failing the stream.
        Err(_) => {
            if json_mode {
                println!(
                    "{}",
                    json!({ "seq": frame.seq, "session_id": frame.session_id, "event": null })
                );
            } else {
                println!("· (event this CLI cannot render)");
            }
        }
    }
    let _ = std::io::stdout().flush();
}

/// Does this event (or its piggybacked status) satisfy the stop condition?
fn stop_reached(stop: Stop, frame: &HostEvent, event: Option<&ThreadEvent>) -> Option<StreamEnd> {
    match stop {
        Stop::Never => None,
        Stop::TurnEnded => match event {
            Some(ThreadEvent::TurnEnded { is_error, .. }) => {
                Some(StreamEnd::TurnEnded { is_error: *is_error })
            }
            _ => None,
        },
        Stop::NeedsApproval => {
            let asked = matches!(
                event,
                Some(ThreadEvent::PermissionRequested { .. } | ThreadEvent::QuestionAsked { .. })
            );
            // `status` is current-at-forward-time, so it can satisfy the wait
            // even when the triggering event itself is something else.
            if asked || frame.status.awaiting_permission {
                Some(StreamEnd::NeedsApproval)
            } else {
                None
            }
        }
    }
}

/// Resync after a gap the backlog cannot fill: fetch the authoritative folded
/// transcript (paged — never the frame-capped legacy verb) and print the
/// marker. Returns the snapshot's fold cursor.
async fn resync_from_transcript(
    client: &Client,
    session: &str,
    json_mode: bool,
    quiet: bool,
    events_elapsed: u64,
    pending: &mut Vec<HostEvent>,
) -> Result<u64, Failure> {
    let mut cursor = 0u64;
    let mut seq;
    loop {
        let reply = client
            .call_streaming(
                Request::FetchTranscriptPage {
                    session_id: session.to_string(),
                    cursor,
                    limit: 256,
                },
                |push| {
                    if let Response::Event(frame) = push {
                        pending.push(frame);
                    }
                },
            )
            .await?;
        match reply {
            Response::TranscriptPage(page) => {
                seq = page.seq;
                match page.next_cursor {
                    Some(next) => cursor = next,
                    None => break,
                }
            }
            Response::Error(e) => return Err(rpc_failure(e)),
            other => return Err(unexpected_reply("FetchTranscriptPage", &other)),
        }
    }
    if !quiet {
        if json_mode {
            println!("{}", json!({ "resynced": true, "events_elapsed": events_elapsed }));
        } else {
            println!("— resynced, {events_elapsed} events elapsed —");
        }
        let _ = std::io::stdout().flush();
    }
    Ok(seq)
}

/// The shared streaming loop. Subscribes after `from` (or the live edge),
/// replays the backlog, then follows live events until `stop` is reached, the
/// deadline passes, or Ctrl+C detaches.
///
/// **Gap contract** (mirrors the protocol doc): a jump in `seq` between
/// consecutive frames first tries `EventsSince` (the documented small-gap
/// recovery); when the span has aged out of the bounded backlog too, the state
/// is resynced from the transcript and a `— resynced —` marker is printed.
/// Silent loss is not an acceptable outcome.
pub async fn stream_session(
    client: &Client,
    session: &str,
    from: Option<u64>,
    json_mode: bool,
    quiet: bool,
    stop: Stop,
    deadline: Option<tokio::time::Instant>,
) -> Result<StreamEnd, Failure> {
    // The resume point: an explicit cursor, else the live edge as the host
    // reports it now.
    let after_seq = match from {
        Some(seq) => seq,
        None => match client.call(Request::GetSessionInfo { session_id: session.into() }).await? {
            Response::SessionInfo(info) => info.summary.last_seq,
            Response::Error(e) => return Err(rpc_failure(e)),
            other => return Err(unexpected_reply("GetSessionInfo", &other)),
        },
    };

    // Pushes that interleave with a nested call are buffered, never dropped.
    let mut pending: Vec<HostEvent> = Vec::new();
    let reply = client
        .call_streaming(
            Request::Subscribe { session_id: session.into(), after_seq: Some(after_seq) },
            |push| {
                if let Response::Event(frame) = push {
                    pending.push(frame);
                }
            },
        )
        .await?;
    let backlog = match reply {
        Response::Events(frames) => frames,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("Subscribe", &other)),
    };

    let mut last_seen = after_seq;
    // The replayed backlog can itself open with a gap when `--from` reaches
    // past what the bounded backlog still holds.
    if let Some(first) = backlog.first()
        && first.seq > after_seq + 1
    {
        let elapsed = first.seq - after_seq - 1;
        resync_from_transcript(client, session, json_mode, quiet, elapsed, &mut pending).await?;
        last_seen = first.seq - 1;
    }
    let mut queue: std::collections::VecDeque<HostEvent> = backlog.into();
    queue.extend(pending.drain(..));

    // Wall-clock since the stream opened, reported by the keepalive so a reader
    // sees total elapsed rather than "15s" over and over.
    let started = std::time::Instant::now();

    loop {
        // Drain everything already in hand before blocking on the socket.
        while let Some(frame) = queue.pop_front() {
            if frame.seq <= last_seen {
                // A replayed duplicate (resync overlap); already shown.
                continue;
            }
            if frame.seq > last_seen + 1 {
                // Gap. Documented recovery first: replay from the backlog.
                let mut refill: Vec<HostEvent> = Vec::new();
                let reply = client
                    .call_streaming(
                        Request::EventsSince { session_id: session.into(), after_seq: last_seen },
                        |push| {
                            if let Response::Event(f) = push {
                                refill.push(f);
                            }
                        },
                    )
                    .await?;
                match reply {
                    Response::Events(frames)
                        if frames.first().is_some_and(|f| f.seq == last_seen + 1) =>
                    {
                        // The backlog still covers the span: splice it in ahead
                        // of the frame that revealed the gap. No marker — no
                        // events were lost.
                        let mut spliced: std::collections::VecDeque<HostEvent> = frames.into();
                        spliced.push_back(frame);
                        spliced.extend(refill);
                        spliced.extend(queue.drain(..));
                        queue = spliced;
                        continue;
                    }
                    Response::Events(_) => {
                        // Aged out: the span is unrecoverable from the ring.
                        // Resync the fold from the transcript and say so.
                        let elapsed = frame.seq - last_seen - 1;
                        resync_from_transcript(
                            client, session, json_mode, quiet, elapsed, &mut pending,
                        )
                        .await?;
                        queue.extend(refill);
                        queue.extend(pending.drain(..));
                        last_seen = frame.seq - 1;
                        queue.push_front(frame);
                        continue;
                    }
                    Response::Error(e) => return Err(rpc_failure(e)),
                    other => return Err(unexpected_reply("EventsSince", &other)),
                }
            }
            last_seen = frame.seq;
            emit(json_mode, quiet, &frame);
            let event = frame.event().ok();
            if let Some(end) = stop_reached(stop, &frame, event.as_ref()) {
                return Ok(end);
            }
        }

        // Block for the next live frame, the deadline, a keepalive tick, or a
        // detach. The keepalive `sleep` is created fresh each time round, so it
        // measures silence since the last frame rather than time since the
        // stream opened — a busy turn never trips it.
        let frame = tokio::select! {
            frame = client.recv_frame() => frame?,
            _ = tokio::signal::ctrl_c() => return Ok(StreamEnd::Detached),
            _ = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    // Held open forever when no deadline is set.
                    None => std::future::pending().await,
                }
            } => return Ok(StreamEnd::Deadline),
            _ = tokio::time::sleep(KEEPALIVE_AFTER) => {
                emit_keepalive(json_mode, session, started.elapsed());
                continue;
            }
        };
        match frame {
            Response::Event(event) if event.session_id == session => queue.push_back(event),
            // Another session's event or a list push — not ours to render.
            _ => {}
        }
    }
}

/// The `attach` verb: stream until Ctrl+C. Detaching never stops the agent.
pub async fn run(
    client: &Client,
    session: &str,
    from: Option<u64>,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    if !json_mode {
        println!("attached to {session} — Ctrl+C detaches (the agent keeps running)");
    }
    match stream_session(client, session, from, json_mode, false, Stop::Never, None).await? {
        StreamEnd::Detached => Ok((
            json!({ "session_id": session, "detached": true }),
            "detached — the agent keeps running".into(),
        )),
        // `Stop::Never` can only end by detach; anything else is a logic error
        // surfaced honestly rather than mapped to a fake success.
        _ => Err(Failure::new("protocol", exit::ERROR, "attach ended unexpectedly")),
    }
}
