//! Watching a device from the paired phone, at the RPC boundary: who may
//! subscribe, what the stream carries, and every way a live view ends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{
    AuthStore, Dispatcher, LocalScope, PairingSlot, SimPrincipal, SimViewFeed, SimViewOut, SimViewSource, SimViewer,
    SimulatorControl, registration_proof,
};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::simulator::{
    SimCmdWire, SimErrorWire, SimReplyWire, SimViewClassWire, SimViewKind, SimViewableWire, read_view_frame,
    read_view_token,
};
use oximux_remote_proto::testing::{DuplexTransport, duplex_pair};
use tokio::sync::mpsc;

const SECRET: [u8; 16] = [0x22; 16];
const NOW: u64 = 1_700_000_000;
const PHONE_SEED: [u8; 32] = [0x33; 32];

fn clock() -> u64 {
    NOW
}

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

/// A desktop with one device, whose frames the test feeds.
struct Screen {
    frames: Mutex<Option<mpsc::Receiver<SimViewOut>>>,
    /// The desktop still sharing (its setting on, nobody pressed Stop).
    sharing: Arc<AtomicBool>,
    /// The subscription's source was dropped: the phone no longer counts as
    /// a viewer.
    released: Arc<AtomicBool>,
    viewers: Mutex<Vec<SimViewer>>,
    /// The feed's rate (0: unpaced).
    bits_per_sec: std::sync::atomic::AtomicU64,
}

impl Screen {
    fn new() -> (Arc<Self>, mpsc::Sender<SimViewOut>) {
        let (tx, rx) = mpsc::channel(16);
        let screen = Self {
            frames: Mutex::new(Some(rx)),
            sharing: Arc::new(AtomicBool::new(true)),
            released: Arc::new(AtomicBool::new(false)),
            viewers: Mutex::new(Vec::new()),
            bits_per_sec: std::sync::atomic::AtomicU64::new(0),
        };
        (Arc::new(screen), tx)
    }
}

struct Source {
    frames: mpsc::Receiver<SimViewOut>,
    sharing: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SimViewSource for Source {
    async fn next(&mut self) -> Option<SimViewOut> {
        self.frames.recv().await
    }
    fn allowed(&self) -> bool {
        self.sharing.load(Ordering::SeqCst)
    }
}

fn row() -> SimViewableWire {
    SimViewableWire {
        device: "E0E0".into(),
        name: "iPhone 17".into(),
        worktree: "oximux · main".into(),
        class: SimViewClassWire::IosSimulator,
        streaming: true,
    }
}

#[async_trait::async_trait]
impl SimulatorControl for Screen {
    async fn run(&self, _: &str, _: SimPrincipal, _: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        Ok(SimReplyWire::Done)
    }
    fn supports_view(&self) -> bool {
        true
    }
    async fn view_list(&self, _: &SimViewer) -> Result<Vec<SimViewableWire>, SimErrorWire> {
        Ok(vec![row()])
    }
    async fn view_subscribe(&self, viewer: &SimViewer, device: &str) -> Result<SimViewFeed, SimErrorWire> {
        if device != "E0E0" {
            return Err(SimErrorWire::NotFound(device.into()));
        }
        self.viewers.lock().unwrap().push(viewer.clone());
        let frames = self.frames.lock().unwrap().take().ok_or(SimErrorWire::ConsentPending)?;
        Ok(SimViewFeed {
            width: 590,
            height: 1278,
            bits_per_sec: self.bits_per_sec.load(Ordering::SeqCst),
            source: Box::new(Source { frames, sharing: self.sharing.clone(), released: self.released.clone() }),
        })
    }
}

fn frame(kind: SimViewKind, pts_us: u64, payload: &[u8]) -> SimViewOut {
    SimViewOut { kind, pts_us, payload: payload.into() }
}

/// A host with `screen`, and a phone paired to it (narrowed to a session
/// when `session` names one).
fn host(screen: &Arc<Screen>, session: Option<&str>) -> (Arc<Dispatcher>, Arc<AuthStore>) {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, session.map(Into::into), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth.clone())
        .with_clock(clock)
        .with_simulator(screen.clone());
    (Arc::new(dispatcher), auth)
}

fn phone_key() -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&PHONE_SEED).verifying_key().to_bytes()
}

/// Serve one connection on its own task and pair the phone on it.
async fn paired(dispatcher: &Arc<Dispatcher>, session: Option<&str>) -> (DuplexTransport, tokio::task::JoinHandle<()>) {
    let (server, client) = duplex_pair();
    let dispatcher = dispatcher.clone();
    let serve = tokio::spawn(async move { dispatcher.serve(&server).await });
    let pubkey = phone_key();
    let register = RegisterReq {
        app_pubkey: pubkey,
        device_name: "Test phone".into(),
        proof: registration_proof(&SECRET, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: session.map(Into::into),
    };
    assert!(matches!(call(&client, Request::Register(register)).await, Response::Registered { .. }));
    (client, serve)
}

async fn subscribe(client: &DuplexTransport) -> [u8; 16] {
    match call(client, Request::SimSubscribe { device: "E0E0".into() }).await {
        Response::SimSubscribed(Ok(sub)) => {
            assert_eq!((sub.width, sub.height), (590, 1278));
            sub.token
        }
        other => panic!("not subscribed: {other:?}"),
    }
}

/// Wait (a bounded while) for `flag`.
async fn eventually(flag: &AtomicBool, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// The happy path: the phone lists the device, subscribes, and reads the
/// token then every frame, in order, off a stream of its own.
#[tokio::test]
async fn a_phone_watches_frames_on_their_own_stream() {
    let (screen, feed) = Screen::new();
    let (dispatcher, _auth) = host(&screen, None);
    let (client, _serve) = paired(&dispatcher, None).await;
    assert_eq!(call(&client, Request::SimViewables).await, Response::SimViewables(Ok(vec![row()])));
    let token = subscribe(&client).await;
    assert_eq!(screen.viewers.lock().unwrap()[0].phone_name, "Test phone");

    feed.send(frame(SimViewKind::Config, 0, &[1, 0x64, 0, 0x1f])).await.unwrap();
    feed.send(frame(SimViewKind::Key, 66_666, &[0, 0, 0, 1, 0x65])).await.unwrap();
    feed.send(frame(SimViewKind::Delta, 133_333, &[0, 0, 0, 1, 0x41])).await.unwrap();
    let mut stream = client.accept_uni().await.unwrap().expect("a view stream");
    assert_eq!(read_view_token(stream.as_mut()).await.unwrap(), Some(token));
    let kinds: Vec<_> = [
        read_view_frame(stream.as_mut()).await.unwrap().unwrap(),
        read_view_frame(stream.as_mut()).await.unwrap().unwrap(),
        read_view_frame(stream.as_mut()).await.unwrap().unwrap(),
    ]
    .into_iter()
    .map(|f| (f.kind, f.pts_us))
    .collect();
    assert_eq!(kinds, [(SimViewKind::Config, 0), (SimViewKind::Key, 66_666), (SimViewKind::Delta, 133_333)]);

    // RPCs still answer while the picture streams.
    assert_eq!(call(&client, Request::Ping).await, Response::Pong);
    // Unsubscribing ends the stream and the desktop's viewer.
    assert_eq!(call(&client, Request::SimUnsubscribe { token }).await, Response::Ack);
    assert!(read_view_frame(stream.as_mut()).await.unwrap().is_none(), "the stream ends");
    assert!(eventually(&screen.released, Duration::from_secs(2)).await);
}

/// **Revocation bites a live view** even on a still screen: no frame is
/// coming, and the stream still ends within the recheck interval.
#[tokio::test]
async fn revoking_the_phone_ends_a_still_view_within_a_second_or_so() {
    let (screen, _feed) = Screen::new();
    let (dispatcher, auth) = host(&screen, None);
    let (client, _serve) = paired(&dispatcher, None).await;
    subscribe(&client).await;
    let mut stream = client.accept_uni().await.unwrap().expect("a view stream");
    read_view_token(stream.as_mut()).await.unwrap().expect("the token");

    let revoked = Instant::now();
    auth.revoke(&phone_key());
    assert!(read_view_frame(stream.as_mut()).await.unwrap().is_none(), "the stream ends");
    assert!(revoked.elapsed() < Duration::from_secs(2), "{:?}", revoked.elapsed());
    assert!(eventually(&screen.released, Duration::from_secs(2)).await);
}

/// The desktop stopping sharing ends the view, and the frame waiting is not
/// sent.
#[tokio::test]
async fn stopping_sharing_on_the_desktop_ends_the_view() {
    let (screen, feed) = Screen::new();
    let (dispatcher, _auth) = host(&screen, None);
    let (client, _serve) = paired(&dispatcher, None).await;
    subscribe(&client).await;
    let mut stream = client.accept_uni().await.unwrap().expect("a view stream");
    read_view_token(stream.as_mut()).await.unwrap().expect("the token");
    feed.send(frame(SimViewKind::Key, 1, &[9])).await.unwrap();
    assert_eq!(read_view_frame(stream.as_mut()).await.unwrap().unwrap().kind, SimViewKind::Key);

    screen.sharing.store(false, Ordering::SeqCst);
    let _ = feed.send(frame(SimViewKind::Delta, 2, &[8])).await;
    assert!(read_view_frame(stream.as_mut()).await.unwrap().is_none(), "nothing more is sent");
    assert!(eventually(&screen.released, Duration::from_secs(2)).await);
}

/// **A large frame being paced out does not outlive the permission**: the
/// rest of it is not sent once sharing stops, though it would take seconds.
#[tokio::test]
async fn stopping_sharing_cuts_a_frame_being_paced_out() {
    let (screen, feed) = Screen::new();
    // 80 kbit/s: a 60 kB key frame takes six seconds to go out.
    screen.bits_per_sec.store(80_000, Ordering::SeqCst);
    let (dispatcher, _auth) = host(&screen, None);
    let (client, _serve) = paired(&dispatcher, None).await;
    subscribe(&client).await;
    let mut stream = client.accept_uni().await.unwrap().expect("a view stream");
    read_view_token(stream.as_mut()).await.unwrap().expect("the token");
    feed.send(frame(SimViewKind::Key, 1, &vec![7u8; 60_000])).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let stopped = Instant::now();
    screen.sharing.store(false, Ordering::SeqCst);
    let read = tokio::time::timeout(Duration::from_secs(10), read_view_frame(stream.as_mut())).await.expect("the stream ends");
    assert!(!matches!(read, Ok(Some(_))), "the frame was cut, not finished");
    assert!(stopped.elapsed() < Duration::from_secs(2), "{:?}", stopped.elapsed());
    assert!(eventually(&screen.released, Duration::from_secs(2)).await);
}

/// A phone that goes away (no unsubscribe: it lost signal) still releases
/// its viewer on the desktop.
#[tokio::test]
async fn a_phone_that_leaves_releases_its_viewer() {
    let (screen, _feed) = Screen::new();
    let (dispatcher, _auth) = host(&screen, None);
    let (client, serve) = paired(&dispatcher, None).await;
    subscribe(&client).await;
    drop(client);
    serve.await.unwrap();
    assert!(eventually(&screen.released, Duration::from_secs(2)).await);
}

/// The phone stopping reading (it dropped its end) ends the pump too.
#[tokio::test]
async fn a_phone_that_stops_reading_releases_its_viewer() {
    let (screen, feed) = Screen::new();
    let (dispatcher, _auth) = host(&screen, None);
    let (client, _serve) = paired(&dispatcher, None).await;
    subscribe(&client).await;
    drop(client.accept_uni().await.unwrap().expect("a view stream"));
    let _ = feed.send(frame(SimViewKind::Key, 1, &[9])).await;
    assert!(eventually(&screen.released, Duration::from_secs(2)).await);
}

/// Who may not: a phone narrowed to one session (as for terminals), and a
/// local caller (its socket has no second stream). The desktop's own
/// refusals pass through as they are.
#[tokio::test]
async fn the_gates_refuse_before_the_desktop_is_asked() {
    let (screen, _feed) = Screen::new();
    let (dispatcher, _auth) = host(&screen, Some("sess-1"));
    let (client, _serve) = paired(&dispatcher, Some("sess-1")).await;
    assert_eq!(call(&client, Request::SimViewables).await, Response::Error(RpcError::Unauthorized));
    let sub = call(&client, Request::SimSubscribe { device: "E0E0".into() }).await;
    assert_eq!(sub, Response::Error(RpcError::Unauthorized));
    assert!(screen.viewers.lock().unwrap().is_empty(), "the desktop was never asked");

    let (server, local) = duplex_pair();
    let serving = dispatcher.clone();
    tokio::spawn(async move { serving.serve_local(&server, LocalScope::Full).await });
    let sub = call(&local, Request::SimSubscribe { device: "E0E0".into() }).await;
    assert_eq!(sub, Response::Error(RpcError::Unsupported));

    let (screen, _feed) = Screen::new();
    let (dispatcher, _auth) = host(&screen, None);
    let (client, _serve) = paired(&dispatcher, None).await;
    let other = call(&client, Request::SimSubscribe { device: "nope".into() }).await;
    assert_eq!(other, Response::SimSubscribed(Err(SimErrorWire::NotFound("nope".into()))));
    subscribe(&client).await;
    let again = call(&client, Request::SimSubscribe { device: "E0E0".into() }).await;
    assert_eq!(again, Response::SimSubscribed(Err(SimErrorWire::ConsentPending)), "the desktop's answer, as it is");
}

/// A host whose desktop shows no devices (headless `oximux serve`) says so.
#[tokio::test]
async fn a_host_without_views_says_unsupported() {
    struct Plain;
    #[async_trait::async_trait]
    impl SimulatorControl for Plain {
        async fn run(&self, _: &str, _: SimPrincipal, _: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
            Ok(SimReplyWire::Done)
        }
    }
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(SessionRegistry::new()), auth).with_clock(clock).with_simulator(Arc::new(Plain)));
    let (client, _serve) = paired(&dispatcher, None).await;
    assert_eq!(call(&client, Request::SimViewables).await, Response::Error(RpcError::Unsupported));
}
