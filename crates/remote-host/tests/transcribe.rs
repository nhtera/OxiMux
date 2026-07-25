//! The voice-transcription RPC (v11): decode one recorded clip to text.
//!
//! It names no session and mutates nothing, so — unlike the schedule RPCs — it
//! carries no tier gate: any paired device may dictate, including a read-only
//! one. The axes worth pinning are therefore the wire guards (valid base64, size
//! cap) and the error mapping (a bad clip is the client's to fix, a host-state
//! failure is not), plus that an absent engine is indistinguishable from a
//! refusal.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::transcribe::{AudioTranscriber, TranscribeError};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn register_req(pubkey: [u8; 32]) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&SECRET, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: None,
    }
}

fn transcribe_req(raw: &[u8], sample_rate: u32) -> Request {
    Request::TranscribeAudio { audio_base64: STANDARD.encode(raw), sample_rate }
}

/// What a fake transcriber does when called, plus a counter so a test can assert
/// the handler's guards ran *before* the (expensive) engine would.
enum Mode {
    /// Echo the decoded byte count + rate back, proving the exact bytes reached
    /// the seam after base64 decoding.
    Echo,
    /// Report a specific engine error, to pin the wire mapping.
    Err(fn() -> TranscribeError),
}

struct FakeTranscriber {
    mode: Mode,
    calls: Arc<AtomicUsize>,
}

impl FakeTranscriber {
    fn echo() -> (Arc<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Arc::new(Self { mode: Mode::Echo, calls: Arc::clone(&calls) }), calls)
    }
    fn erroring(make: fn() -> TranscribeError) -> Arc<Self> {
        Arc::new(Self { mode: Mode::Err(make), calls: Arc::new(AtomicUsize::new(0)) })
    }
}

#[async_trait::async_trait]
impl AudioTranscriber for FakeTranscriber {
    async fn transcribe(&self, wav: &[u8], sample_rate: u32) -> Result<String, TranscribeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.mode {
            Mode::Echo => Ok(format!("heard {} bytes at {sample_rate} Hz", wav.len())),
            Mode::Err(make) => Err(make()),
        }
    }
}

fn dispatcher_with(
    auth: Arc<AuthStore>,
    transcriber: Arc<dyn AudioTranscriber>,
) -> Dispatcher {
    Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_transcriber(transcriber)
}

fn open_auth() -> Arc<AuthStore> {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    auth
}

/// The happy path: a paired device sends a clip and gets the engine's text back,
/// with the decoded bytes reaching the seam intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paired_device_transcribes_a_clip() {
    let (transcriber, calls) = FakeTranscriber::echo();
    let dispatcher = dispatcher_with(open_auth(), transcriber);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        // 480 arbitrary bytes stand in for a WAV clip; the fake echoes the count.
        let clip = vec![0xABu8; 480];
        let reply = call(&client, transcribe_req(&clip, 16_000)).await;
        assert_eq!(reply, Response::Transcript("heard 480 bytes at 16000 Hz".into()));
        drop(client);
    };
    futures::future::join(serve, script).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the engine ran exactly once");
}

/// No write scope is required: a read-only device may still dictate, because the
/// call mutates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_may_still_dictate() {
    let auth = open_auth();
    let (transcriber, calls) = FakeTranscriber::echo();
    // `[0x33; 32]` is a valid ed25519 point (registration decompresses the key);
    // arbitrary byte fills like `0x44` are not, and the register would be refused.
    let pubkey = [0x33; 32];
    let dispatcher = dispatcher_with(Arc::clone(&auth), transcriber);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        auth.set_read_only(&pubkey, true);
        let reply = call(&client, transcribe_req(&[1, 2, 3, 4], 16_000)).await;
        assert_eq!(reply, Response::Transcript("heard 4 bytes at 16000 Hz".into()));
        drop(client);
    };
    futures::future::join(serve, script).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a read-only device reached the engine");
}

/// An oversized clip is refused with a clear reason *before* the engine is
/// invoked — the guard, not a bare transport error, is what the client sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_clip_is_refused_before_decoding() {
    let (transcriber, calls) = FakeTranscriber::echo();
    let dispatcher = dispatcher_with(open_auth(), transcriber);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        // Just over the 6 MiB decoded cap.
        let huge = vec![0u8; 6 * 1024 * 1024 + 1];
        let reply = call(&client, transcribe_req(&huge, 16_000)).await;
        let Response::Error(RpcError::BadRequest(msg)) = reply else {
            panic!("expected BadRequest, got {reply:?}");
        };
        assert!(msg.contains("too long"), "reason names the limit, got {msg:?}");
        drop(client);
    };
    futures::future::join(serve, script).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the guard fired before the engine");
}

/// A payload that is not valid base64 is a `BadRequest`, not an `Internal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_base64_payload_is_a_bad_request() {
    let (transcriber, calls) = FakeTranscriber::echo();
    let dispatcher = dispatcher_with(open_auth(), transcriber);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let reply = call(
            &client,
            Request::TranscribeAudio { audio_base64: "not base64!!!".into(), sample_rate: 16_000 },
        )
        .await;
        assert!(matches!(reply, Response::Error(RpcError::BadRequest(_))), "got {reply:?}");
        drop(client);
    };
    futures::future::join(serve, script).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a malformed payload never reached the engine");
}

/// The engine's error kind decides the wire code: a bad clip maps to
/// `BadRequest` (retrying the same bytes is pointless), a host-state failure to
/// `Internal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_error_kinds_map_to_the_right_code() {
    for (make, expect_bad_request) in [
        (( || TranscribeError::BadAudio) as fn() -> TranscribeError, true),
        (|| TranscribeError::NoModel, false),
        (|| TranscribeError::Failed, false),
    ] {
        let dispatcher = dispatcher_with(open_auth(), FakeTranscriber::erroring(make));
        let (client, server) = duplex_pair();
        let serve = dispatcher.serve(&server);
        let script = async move {
            let Response::Registered { .. } =
                call(&client, Request::Register(register_req([0x33; 32]))).await
            else {
                panic!("expected Registered");
            };
            let reply = call(&client, transcribe_req(&[1, 2, 3, 4], 16_000)).await;
            match reply {
                Response::Error(RpcError::BadRequest(_)) => assert!(expect_bad_request),
                Response::Error(RpcError::Internal(_)) => assert!(!expect_bad_request),
                other => panic!("expected a mapped error, got {other:?}"),
            }
            drop(client);
        };
        futures::future::join(serve, script).await;
    }
}

/// A host with no transcriber answers `Unauthorized`, indistinguishable from a
/// device that lacks the scope — capability is not something to leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_a_transcriber_refuses() {
    // No `.with_transcriber(...)`.
    let dispatcher =
        Dispatcher::new(Arc::new(SessionRegistry::new()), open_auth()).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let reply = call(&client, transcribe_req(&[1, 2, 3, 4], 16_000)).await;
        assert_eq!(reply, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
}
