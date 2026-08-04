//! `attach`'s two mid-stream gap-recovery branches, deterministically.
//!
//! The real dispatcher only produces a live-frame seq jump when a broadcast
//! ring laps under a slow subscriber — scheduling-dependent and unforceable
//! from a subprocess test. So these tests script the host side instead: a
//! minimal request/reply loop over the real local socket serves frames with
//! exactly the seq jumps each branch needs, and the compiled binary's stream
//! loop does the recovering. The contract under test (from `stream_session`):
//!
//! 1. a jump whose span `EventsSince` still covers is spliced in — **no
//!    marker, nothing lost**;
//! 2. a jump whose span aged out of the backlog resyncs from the transcript
//!    and prints the `resynced` marker **mid-stream**, then keeps streaming.

use std::path::Path;
use std::process::Command;

use oximux_agent_core::thread::ThreadEvent;
use oximux_remote_local::{LocalControlListener, generate_token};
use oximux_remote_proto::messages::{HelloAckWire, SessionStatusWire, TranscriptPageWire};
use oximux_remote_proto::proto::{MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Request, Response};
use oximux_remote_proto::{HostEvent, Transport};

fn bin(runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "10"]);
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

fn frame(seq: u64) -> HostEvent {
    HostEvent::new(
        "gap",
        seq,
        &ThreadEvent::AssistantText(format!("line {seq}")),
        SessionStatusWire { last_seq: seq, awaiting_permission: false },
    )
    .expect("encodable event")
}

async fn send(t: &dyn Transport, r: Response) {
    t.send(r.to_bytes().unwrap()).await.expect("host send");
}

/// Next request, or `None` once the client hung up.
async fn recv(t: &dyn Transport) -> Option<Request> {
    let f = t.recv().await.ok().flatten()?;
    Some(Request::from_bytes(&f).expect("decodable request"))
}

/// Answer the `Hello`/`Subscribe {after_seq: 0}` opening every attach makes,
/// replying with a contiguous seqs 1–3 backlog.
async fn serve_opening(t: &dyn Transport) {
    match recv(t).await {
        Some(Request::Hello(_)) => {
            send(
                t,
                Response::HelloAck(HelloAckWire {
                    protocol_version: PROTOCOL_VERSION,
                    min_compatible: MIN_COMPATIBLE_VERSION,
                }),
            )
            .await;
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    match recv(t).await {
        Some(Request::Subscribe { session_id, after_seq }) => {
            assert_eq!(session_id, "gap");
            assert_eq!(after_seq, Some(0));
            send(t, Response::Events(vec![frame(1), frame(2), frame(3)])).await;
        }
        other => panic!("expected Subscribe, got {other:?}"),
    }
}

/// One accepted connection, handed to the per-test script.
fn scripted_host<F, Fut>(rt: &tokio::runtime::Runtime, runtime_dir: &Path, script: F)
where
    F: FnOnce(std::sync::Arc<dyn Transport>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    rt.spawn(async move {
        let pending = listener.accept_pending().await.expect("one connection");
        let Ok((transport, _claim)) = pending.authenticate().await else {
            panic!("the CLI authenticates");
        };
        script(transport).await;
    });
}

/// Spawn `attach gap --from 0 --json` and collect its NDJSON stdout lines
/// until `until` says stop (or the deadline passes), then kill it.
fn attach_and_collect(
    runtime_dir: &Path,
    until: impl Fn(&serde_json::Value) -> bool,
) -> Vec<serde_json::Value> {
    use std::io::BufRead as _;

    let mut child = bin(runtime_dir)
        .args(["--json", "attach", "gap", "--from", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        let Ok(line) = rx.recv_timeout(std::time::Duration::from_millis(200)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let done = until(&v);
        lines.push(v);
        if done {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    lines
}

/// Branch 1: a live-frame jump whose span the backlog still covers is refilled
/// via `EventsSince` — every seq arrives, in order, with no resync marker.
#[test]
fn a_covered_gap_is_spliced_without_a_marker() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");

    scripted_host(&rt, &runtime_dir, |t| async move {
        let t = t.as_ref();
        serve_opening(t).await;
        // Live edge, then a jump 4 → 8.
        send(t, Response::Event(frame(4))).await;
        send(t, Response::Event(frame(8))).await;
        // The documented small-gap recovery: the backlog still covers 5–7.
        match recv(t).await {
            Some(Request::EventsSince { session_id, after_seq }) => {
                assert_eq!(session_id, "gap");
                assert_eq!(after_seq, 4, "refill resumes from the last seq seen");
                send(t, Response::Events(vec![frame(5), frame(6), frame(7)])).await;
            }
            other => panic!("expected EventsSince, got {other:?}"),
        }
        // Streaming continues past the recovered span.
        send(t, Response::Event(frame(9))).await;
        // Hold the connection until the test kills the client.
        while recv(t).await.is_some() {}
    });

    let lines = attach_and_collect(&runtime_dir, |v| v["seq"] == 9);
    let seqs: Vec<u64> = lines.iter().filter_map(|v| v["seq"].as_u64()).collect();
    assert_eq!(seqs, (1..=9).collect::<Vec<u64>>(), "nothing lost, nothing reordered");
    assert!(
        !lines.iter().any(|v| v["resynced"] == true),
        "a covered gap must recover silently — a marker would claim loss that did not happen"
    );
}

/// Branch 2: a live-frame jump whose span has aged out of the backlog resyncs
/// from the transcript, prints the marker MID-stream (after live events have
/// already flowed), and keeps streaming afterwards.
#[test]
fn an_aged_out_gap_resyncs_mid_stream_with_a_marker() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");

    scripted_host(&rt, &runtime_dir, |t| async move {
        let t = t.as_ref();
        serve_opening(t).await;
        // Live edge reached (seq 4 flows normally), then the ring laps: the
        // next frame to arrive is seq 20.
        send(t, Response::Event(frame(4))).await;
        send(t, Response::Event(frame(20))).await;
        // The refill attempt finds the span gone — the backlog now starts far
        // past seq 5, so the reply cannot open the gap.
        match recv(t).await {
            Some(Request::EventsSince { after_seq, .. }) => {
                assert_eq!(after_seq, 4);
                send(t, Response::Events(vec![frame(18), frame(19)])).await;
            }
            other => panic!("expected EventsSince, got {other:?}"),
        }
        // So the client resyncs the fold from the paged transcript.
        match recv(t).await {
            Some(Request::FetchTranscriptPage { session_id, cursor, .. }) => {
                assert_eq!(session_id, "gap");
                assert_eq!(cursor, 0);
                send(
                    t,
                    Response::TranscriptPage(TranscriptPageWire {
                        session_id: "gap".into(),
                        seq: 19,
                        entries_json: "[]".into(),
                        next_cursor: None,
                        total: 0,
                        model: None,
                    }),
                )
                .await;
            }
            other => panic!("expected FetchTranscriptPage, got {other:?}"),
        }
        // Streaming continues after the resync.
        send(t, Response::Event(frame(21))).await;
        while recv(t).await.is_some() {}
    });

    let lines = attach_and_collect(&runtime_dir, |v| v["seq"] == 21);
    let seqs: Vec<u64> = lines.iter().filter_map(|v| v["seq"].as_u64()).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 20, 21], "the aged-out span is not replayed");

    let marker_at = lines
        .iter()
        .position(|v| v["resynced"] == true)
        .expect("the resync marker must be printed — silent loss is a bug");
    assert_eq!(
        lines[marker_at]["events_elapsed"], 15,
        "the marker reports the lost span (seq 5–19)"
    );
    let last_before = lines[..marker_at].iter().rev().find_map(|v| v["seq"].as_u64());
    assert_eq!(
        last_before,
        Some(4),
        "mid-stream: live events flowed before the marker, so this is not the initial-subscribe path"
    );
    let first_after = lines[marker_at + 1..].iter().find_map(|v| v["seq"].as_u64());
    assert_eq!(first_after, Some(20), "the frame that revealed the gap streams right after");
}
