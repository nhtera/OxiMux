//! The per-session pump: what it publishes, what it writes, and what a
//! restart finds where it left it.
//!
//! A sibling file rather than an inline `mod tests`, like the host
//! config's and the retry driver's: the pump is already one of the
//! larger files here, and its tests are the half that keeps growing.

use super::*;
use oximux_agents::send_gate::{DeliveryMode, SendWait};
use oximux_agents::thread::StubConnection;

/// The real driver, over the test's own database.
///
/// Not a stand-in: the pump calls it on every failure and every line it
/// writes, and a stub that armed nothing would let the pump's side of
/// those calls go untested in every one of these.
fn retry_driver(
    registry: Arc<SessionRegistry>,
    settings: &SettingsRepo,
    db: &oximux_storage::db::Db,
) -> Arc<super::super::retry::ServeRetry> {
    let (tx, rx) = tokio::sync::watch::channel(oximux_remote_host::HostConfig::default());
    // The sender outlives the test: a receiver whose sender is gone still
    // reads the last value, but leaking it keeps that from being the
    // thing under test.
    std::mem::forget(tx);
    super::super::retry::ServeRetry::new(
        registry,
        oximux_agents::retry::store::RetryStore::new(db.conn()),
        settings.clone(),
        rx,
        "test-host".into(),
    )
}

fn wait_until(deadline_ms: u64, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
    while std::time::Instant::now() < deadline {
        if probe() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// A queued send is written when nothing holds the gate.
///
/// Attaching a gate takes the send path over: `send_prompt_tagged` queues
/// instead of writing, for every sender. So the flush is not an extra —
/// without it this host sends nothing at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_send_is_written_once_the_gate_is_open() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (_tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    // `has_waker` and not merely `is_some`: that is the exact test the
    // outbox drainer makes before it will hand a parent its children's
    // events (`outbox_drainer.rs`: `if !gate.has_waker() { continue }`).
    assert!(
        wait_until(5_000, || handle
            .gate()
            .is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attaches a gate the drainer will deliver into",
    );

    handle.send_prompt("ship it", &[]).unwrap();
    assert!(
        wait_until(5_000, || !conn.sent().is_empty()),
        "the send the gate swallowed is the send the pump writes",
    );
}

/// A turn held for a provider limit holds the session's queue with it,
/// and the queue goes out when the hold lifts.
///
/// Both halves are the pump's, and the second is the one with a trap in
/// it: what lifts this hold is the retry driver, outside the pump, and
/// the gate's waker only fires on a *push*. Republished on the wake, the
/// next line goes; left as it was, it waits for an unrelated event that
/// a session waiting out a limit does not have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_holds_the_queue_and_lifting_it_lets_the_queue_go() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let retry = retry_driver(registry.clone(), &settings, &db);
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry.clone(),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    // A usage window closes the turn: the provider's reset is hours out.
    tx.send(ThreadEvent::RateLimitUpdated(oximux_agent_core::thread::event::RateLimitInfo {
        status: "rejected".into(),
        limit_type: Some("five_hour".into()),
        resets_at_ms: Some(oximux_agents::session_health::now_ms() + 3 * 60 * 60 * 1000),
        utilization: None,
    }))
    .unwrap();
    tx.send(ThreadEvent::TurnEnded {
        result: Some("usage limit reached".into()),
        usage: None,
        is_error: true,
        turn_diff: None,
    })
    .unwrap();

    assert!(
        wait_until(5_000, || handle.health().status
            == oximux_agents::health::HealthStatus::Limited),
        "the session reports the limit it is waiting out: {:?}",
        handle.health(),
    );
    assert_eq!(
        handle.health().limited_until,
        retry.armed("s-1").map(|a| a.wake_at_ms),
        "and says when it will try again — the driver's answer, not a second one",
    );

    // A line sent meanwhile waits: letting it out would start a second
    // turn into the same closed window.
    handle.send_prompt("and this too", &[]).unwrap();
    assert!(
        !wait_until(300, || !conn.sent().is_empty()),
        "nothing goes out while the window is closed",
    );

    // The hold lifts from outside the pump, and the next push is the
    // only thing that wakes it.
    retry.on_turn_succeeded("s-1");
    handle.send_prompt("and now", &[]).unwrap();
    assert!(
        wait_until(5_000, || !conn.sent().is_empty()),
        "the line held for the limit went out once the hold lifted",
    );
    // One, not both: taking the first opened a turn, and the second
    // waits for it like any mid-turn send.
    assert_eq!(conn.sent().len(), 1, "a lifted limit does not empty the queue at once");
    assert!(
        conn.sent()[0].to_string().contains("and this too"),
        "and the one that went is the one that had been waiting longest",
    );
}

/// A turn that failed is still failed after the host restarts.
///
/// The fold keeps no durable trace of a failure, so before this the
/// session came back reading idle — and the queue the failure was holding
/// drained into an agent nobody had fixed. The second pump here is the
/// restart: same blob, same store, a new process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_turn_is_still_failed_after_a_restart() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    tx.send(ThreadEvent::TurnEnded {
        result: Some("the provider refused".into()),
        usage: None,
        is_error: true,
        turn_diff: None,
    })
    .unwrap();
    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .and_then(|b| b.last_failure)
            .is_some()),
        "the failure is written where a restart will find it",
    );

    // The restart. The agent's stream is new and silent, which is the
    // case that matters: a resumed backend replays nothing, so nothing
    // but the blob can say this session failed.
    drop(tx);
    registry.unregister("s-1");
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let (_tx, rx) = std::sync::mpsc::channel();
    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: blob::load(&settings, "s-1").expect("the stored blob"),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );

    assert!(
        wait_until(5_000, || handle.health().status
            == oximux_agents::health::HealthStatus::Failed),
        "the restored session reports the failure, not idle: {:?}",
        handle.health(),
    );
    assert_eq!(
        handle.health().reason.as_deref(),
        Some("the provider refused"),
        "and reports the words it reported before the restart",
    );
    // And the failure still holds the queue: another agent's message
    // waits for a person rather than feeding a session nobody has fixed.
    // No turn is running here, so `Busy` can only be the failure.
    let gate = handle.gate().expect("the pump attached its gate");
    let mut gate = gate.lock().unwrap();
    gate.push("a child finished".into(), Vec::new(), SendSource::Agent, DeliveryMode::Queue, None);
    let queued = gate.queued().next().expect("the item is queued").clone();
    assert_eq!(gate.wait_of(&queued), SendWait::Busy);
}

/// The stored failure is cleared by the next send, not only written.
///
/// A blob that could gain the field and never lose it would report a
/// failure the session has since moved past — for the life of the
/// session, since nothing else writes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_clears_the_failure_the_blob_carries() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let mut seed = ChatBlob::new("s-1".into());
    seed.last_failure = Some("the provider refused".into());
    seed.entries = vec![oximux_agents::thread::ThreadEntry::User {
        text: "ship it".into(),
        images: Vec::new(),
        checkpoint: None,
    }];
    let (_tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    handle.send_prompt("try again", &[]).unwrap();
    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .is_some_and(|b| b.last_failure.is_none())),
        "the send that supersedes the failure clears what a restart would read",
    );
}

/// A Stop from a phone or the CLI interrupts the agent.
///
/// `SessionHandle::cancel` hands a Stop to the gate whenever one is
/// attached and listening, because only the gate's driver can take back
/// the lines it wrote into the turn. Headless that driver is this pump, so
/// without it reading the request the gate holds a Stop nobody acts on:
/// the caller is told `Ok` and the turn runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_from_elsewhere_interrupts_the_agent() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![ThreadEvent::UserMessage { text: "hi".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );
    tx.send(ThreadEvent::AssistantText("working…".into())).unwrap();
    assert!(wait_until(5_000, || handle.status_snapshot().working), "a turn is running");

    // The route a phone's Cancel takes.
    handle.cancel().unwrap();
    assert!(wait_until(5_000, || conn.cancels() == 1), "the Stop reached the agent");
}

/// A send that could not be written leaves the gate as it found it.
///
/// Taking an item opens a turn for it. A write that failed started none,
/// and a turn left open closes the gate to every later send until
/// something publishes the holds again — which a backend that says
/// nothing never does, so the queue would stall for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_could_not_be_written_does_not_wedge_the_queue() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (_tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    conn.fail_sends(true);
    handle.send_prompt("this one cannot be written", &[]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(conn.sent().is_empty(), "nothing reached the agent");

    // No backend event in between: the only thing that can reopen the gate
    // is the failed write putting back what it took.
    conn.fail_sends(false);
    handle.send_prompt("and this one can", &[]).unwrap();
    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the next send still goes out");
}

/// A send that arrives mid-turn waits for the turn, then goes.
///
/// The gate's waker fires when something is pushed and never when a hold
/// lifts, so a flush wired only to the waker leaves a child's events
/// sitting there until something unrelated happens to arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_arrives_mid_turn_goes_out_when_the_turn_ends() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![ThreadEvent::UserMessage { text: "hi".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    tx.send(ThreadEvent::AssistantText("working…".into())).unwrap();
    assert!(wait_until(5_000, || handle.status_snapshot().working), "a turn is running");
    let during = conn.sent().len();

    handle.send_prompt("and this too", &[]).unwrap();
    // Nothing may be written into a running turn: this host is queue-only.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(conn.sent().len(), during, "held while the turn runs");

    tx.send(ThreadEvent::TurnEnded {
        is_error: false,
        result: None,
        usage: None,
        turn_diff: None,
    })
    .unwrap();
    assert!(
        wait_until(5_000, || conn.sent().len() > during),
        "and written when the turn ends — the hold lifting is what opens the gate",
    );
}

/// A reopened session says so before its backend says anything.
///
/// Both reviewers caught this independently, and the catalog's own comment
/// had already promised it: a resumed backend replays nothing, so the
/// buffer is empty, and `health_push` sends a client no frame at all
/// until a session has published once. Every device would have read a
/// session someone just reopened as a default-idle one.
///
/// And the first answer is `RestoredUnconfirmed`, not `Idle`: nothing has
/// yet confirmed that the conversation on disk is the one this process now
/// holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reopened_session_publishes_before_its_backend_speaks() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s-1".into(), Arc::new(StubConnection::default()));
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (_tx, rx) = std::sync::mpsc::channel();
    // A session with history: what `HeadlessCatalog::resume` hands over,
    // including its empty buffer.
    let mut seed = ChatBlob::new("s-1".into());
    seed.entries = vec![oximux_agents::thread::ThreadEntry::User {
        text: "ship it".into(),
        images: vec![],
        checkpoint: None,
    }];

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );

    assert!(
        wait_until(5_000, || handle.health_published()),
        "a silent backend is no reason for a client to hear nothing",
    );
    assert_eq!(
        handle.health_snapshot().health.liveness,
        oximux_agents::health::Liveness::RestoredUnconfirmed,
        "reopened, and nothing has confirmed it yet",
    );
}

/// A batch of child events reaches its parent as the events it is.
///
/// The gate keeps the host's own envelope intact at the push and escapes
/// everyone else's lookalike text. Writing the item back through the
/// direct path applied that escaping a second time, so a real update
/// arrived at its parent in the very form that marks a forged one as
/// ordinary text — on the one host whose whole job in this change is to
/// run children.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_childs_events_reach_their_parent_with_the_envelope_intact() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (_tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the gate is attached",
    );
    // Pushed as the outbox drainer pushes: the host's own batch, under
    // its own source.
    let events = oximux_agents::envelope::render(&[oximux_agents::envelope::ChildEvent {
        kind: oximux_agents::envelope::ChildEventKind::Finished,
        child: "c-1".into(),
        depth: 1,
        title: "a child".into(),
        status: Some("exit 0".into()),
        result: Some("done".into()),
        permit: None,
    }]);
    assert!(events.contains("<oximux-event"), "the envelope carries its tags");
    handle.gate().unwrap().lock().unwrap().push(
        events,
        Vec::new(),
        SendSource::ParentEvent,
        oximux_agents::send_gate::DeliveryMode::Queue,
        None,
    );
    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the batch is written");
    let written = conn.sent()[0].to_string();
    assert!(written.contains("<oximux-event"), "written as events: {written}");
    assert!(!written.contains("&lt;oximux-event"), "and not escaped: {written}");
}

/// A line an agent cannot acknowledge is confirmed by its next frame.
///
/// Only Claude reports which tagged line it started on; every other
/// backend drops the id, so nothing will ever name the host's write. The
/// outbox drainer settles its rows from receipts alone and gives up after
/// 30 s, so without a stand-in a parent's updates read `unconfirmed`
/// although they were delivered and acted on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_an_agent_cannot_acknowledge_is_confirmed_by_its_next_frame() {
    let registry = Arc::new(SessionRegistry::new());
    // The default stub reports no receipts, like Codex and pi.
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    assert!(!handle.reports_receipts(), "the backend this test is about");
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut receipts = handle.subscribe_receipts();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the gate is attached",
    );
    handle.send_prompt("ship it", &[]).unwrap();
    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the line is written");
    assert!(receipts.try_recv().is_err(), "no agent said anything about it yet");

    tx.send(ThreadEvent::AssistantText("on it".into())).unwrap();
    assert!(
        wait_until(5_000, || matches!(receipts.try_recv(), Ok(ThreadEvent::UserMessageAccepted { .. }))),
        "the frame stands in for the receipt the agent cannot send",
    );
    tx.send(ThreadEvent::TurnEnded { is_error: false, result: None, usage: None, turn_diff: None }).unwrap();
    assert!(
        wait_until(5_000, || matches!(receipts.try_recv(), Ok(ThreadEvent::UserMessagesDelivered { .. }))),
        "and the turn ending says it was carried",
    );
}

/// A turn that ends releases the session, rather than leaving it working
/// for ever.
///
/// The registry's flag is what the host core reads to park a child and to
/// count the fan-out cap, and this host never wrote it: a spawned child
/// was set working once by the spawn and stayed `active` for the life of
/// the process, holding its slot and giving no reason why its parent could
/// not spawn again. The private turn channel said the right thing the
/// whole time, which is why it went unnoticed — two places saying one
/// thing, only one of them read by the thing that matters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_turn_releases_the_session_it_was_holding() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s-1".into(), Arc::new(StubConnection::default()));
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let pumps = PumpSet::new();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            // A turn begins with the user's message: the sibling test
            // relies on the same thing, and an assistant frame on its own
            // folds into no turn at all.
            buffered: vec![ThreadEvent::UserMessage { text: "hi".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        pumps.clone(),
    );

    tx.send(ThreadEvent::AssistantText("working…".into())).unwrap();
    assert!(
        wait_until(5_000, || handle.status_snapshot().working),
        "a running turn reads working in the registry, not only in the pump",
    );
    assert!(handle.health_published(), "and it is the published facts that say so");

    tx.send(ThreadEvent::TurnEnded { is_error: false, result: None, usage: None, turn_diff: None }).unwrap();
    assert!(
        wait_until(5_000, || !handle.status_snapshot().working),
        "and the end of that turn gives the slot back",
    );
    // The pump's own channel agrees — they are published together.
    assert_eq!(pumps.active_turns(), 0);
}

/// The bug a review caught before it shipped: an agent whose stream closes
/// must return its session to the dormant set — final state persisted,
/// registry entry gone (so `open()` can resume it fresh), and the shared
/// index still listing it. Without the unregister, one agent crash
/// stranded the session as live-looking-but-dead until a host restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_agent_returns_its_session_to_dormant() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s-1".into(), Arc::new(StubConnection::default()));
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let index = Arc::new(SessionIndex::default());
    let pumps = PumpSet::new();
    let (tx, rx) = std::sync::mpsc::channel();
    // The credential-revoking hook the launcher installs, stubbed: what
    // matters here is that a dead agent runs it at all, since a credential
    // that outlives its process is the leak this hook exists to close.
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ended_flag = ended.clone();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![ThreadEvent::UserMessage { text: "hi".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: index.clone(),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: Some(Box::new(move || {
                ended_flag.store(true, std::sync::atomic::Ordering::SeqCst)
            })),
        },
        pumps.clone(),
    );

    // A turn starts, then the agent dies mid-turn.
    tx.send(ThreadEvent::AssistantText("working…".into())).unwrap();
    assert!(
        wait_until(5_000, || pumps.active_turns() == 1),
        "the pump tracks the in-flight turn"
    );
    drop(tx);

    assert!(
        wait_until(5_000, || registry.get("s-1").is_none()),
        "a dead agent's session leaves the registry"
    );
    assert!(wait_until(5_000, || pumps.live_pumps() == 0), "the pump ends");
    assert!(
        wait_until(5_000, || ended.load(std::sync::atomic::Ordering::SeqCst)),
        "the end hook runs, so the agent's credential is revoked with it"
    );
    // The final state reached disk, interruption marked (turn no longer
    // active in the persisted fold), and the index still knows the row.
    let blob = blob::load(&settings, "s-1").expect("final state persisted");
    assert!(!blob.entries.is_empty(), "the fold reached disk");
    assert_eq!(
        index.title_of("s-1"),
        Some(Some("hi".into())),
        "the index lists the now-dormant session"
    );
}

/// A LIVE session lists by its prompt, not by its own UUID.
///
/// The registry row is what `oximux ls` reads while a session is running,
/// and it used to publish `fold.title` alone. These backends never send
/// `TitleUpdated`, so that stayed `None` for the session's whole life and
/// the wire fell back to the session id — every row reading
/// `<project> · <uuid>`, with the uuid already in the id column. Measured
/// against a real `claude`: three live sessions were three indistinguishable
/// rows, and *restarting the host* fixed them, because only the persistence
/// path applied the derived-title fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_session_lists_by_its_prompt_rather_than_its_own_id() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s-2".into(), Arc::new(StubConnection::default()));
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let index = Arc::new(SessionIndex::default());
    let pumps = PumpSet::new();
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-2".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![ThreadEvent::UserMessage {
                text: "Summarise what a.txt contains".into(),
                images: vec![],
            }],
            seed: ChatBlob::new("s-2".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index,
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        pumps.clone(),
    );

    // Still live — this is the registry row, not the dormant index.
    assert!(
        wait_until(5_000, || registry
            .get("s-2")
            .and_then(|h| h.meta_snapshot().title)
            .is_some_and(|t| t == "Summarise what a.txt contains")),
        "a live session's list row carries the derived title, got {:?}",
        registry.get("s-2").and_then(|h| h.meta_snapshot().title),
    );
    drop(tx);
}

/// The order the buffered-event test cannot see: the backend announces
/// BEFORE the first prompt lands, and the prompt arrives over the protocol
/// (`send_prompt` → relay sink), never as a `ThreadEvent` — no backend
/// echoes the user's half. The `SessionInit` trigger then fires on an empty
/// fold, and if the prompt arms skip the meta refresh the row shows the
/// session's own UUID for the session's whole life. A resumed session hits
/// the same hole unconditionally, having no `SessionInit` at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_injected_over_the_protocol_still_titles_the_live_row() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s-3".into(), Arc::new(StubConnection::default()));
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let index = Arc::new(SessionIndex::default());
    let pumps = PumpSet::new();
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-3".into(),
            handle: handle.clone(),
            events: rx,
            // The backend has already announced; the fold holds no user
            // entry when the SessionInit trigger fires.
            buffered: vec![ThreadEvent::SessionInit {
                session_id: "s-3".into(),
                model: "fake".into(),
                permission_mode: "default".into(),
                slash_commands: vec![],
                meta: Default::default(),
            }],
            seed: ChatBlob::new("s-3".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index,
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        pumps.clone(),
    );
    assert!(
        wait_until(5_000, || registry
            .get("s-3")
            .map(|h| h.meta_snapshot().model.as_deref() == Some("fake"))
            .unwrap_or(false)),
        "the pump applied the buffered SessionInit",
    );

    handle.send_prompt("Fix the flaky test", &[]).unwrap();

    assert!(
        wait_until(5_000, || registry
            .get("s-3")
            .and_then(|h| h.meta_snapshot().title)
            .is_some_and(|t| t == "Fix the flaky test")),
        "a protocol-injected prompt titles the live row, got {:?}",
        registry.get("s-3").and_then(|h| h.meta_snapshot().title),
    );
    drop(tx);
}

/// A send that overtakes a fired retry takes the retry's own resend out of
/// the queue with it.
///
/// A fired retry lifts its own hold and pushes its resend, so everything the
/// hold was keeping back is *ahead* of that resend. The first of those to go
/// out supersedes the retry — and the resend, left queued, would then re-send
/// the superseded turn on top of the message that took over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_supersedes_a_resend_takes_it_out_of_the_queue() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let retry = retry_driver(registry.clone(), &settings, &db);
    let store = oximux_agents::retry::store::RetryStore::new(db.conn());
    let (tx, rx) = std::sync::mpsc::channel();

    // The transcript a fired retry reads to know what to send again.
    let mut seed = ChatBlob::new("s-1".into());
    seed.entries = vec![oximux_agents::thread::ThreadEntry::User {
        text: "ship it".into(),
        images: Vec::new(),
        checkpoint: None,
    }];
    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: vec![],
            seed,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry.clone(),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    // A window this host is already waiting out. Its wake is an hour away and
    // the test fires it by hand: a wake time short enough to wait out here is
    // one that a loaded machine can reach while this is still setting up, and
    // the resend would then race the line queued below. A real window's grace
    // and jitter put its wake minutes out anyway, which is why this goes
    // through the restore path rather than a rate-limit event.
    store
        .arm(
            "s-1",
            "key-1",
            "window",
            "Usage limit reached",
            oximux_agents::session_health::now_ms() + 60 * 60 * 1000,
            "host-0",
            chrono::Local::now(),
        )
        .expect("arm");
    retry.restore();
    assert!(
        wait_until(5_000, || handle.health().limited_until.is_some()),
        "the pump is holding the queue for the limit: {:?}",
        handle.health(),
    );

    // The person's own message, queued behind the hold.
    handle.send_prompt("and this too", &[]).unwrap();
    assert!(!wait_until(300, || !conn.sent().is_empty()), "it waits out the window");

    // The window reopens.
    retry.fire_now("s-1", "key-1").await;
    assert!(
        wait_until(10_000, || !conn.sent().is_empty()),
        "the hold lifted and the queue moved",
    );
    assert_eq!(conn.sent().len(), 1, "one line opens the turn");
    assert!(
        conn.sent()[0].to_string().contains("and this too"),
        "and it is the person's, which had been waiting longest: {:?}",
        conn.sent(),
    );

    // The turn that line opened ends. Nothing of the retry's is left to go
    // out behind it.
    tx.send(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None })
        .unwrap();
    assert!(
        !wait_until(1_000, || conn.sent().len() > 1),
        "the resend went with the retry the person's message superseded: {:?}",
        conn.sent(),
    );
}

/// A message queued when the host stopped comes back, and comes back held.
///
/// Before this the queue went with the process: a line a phone sent while a
/// turn was running was accepted, held, and then lost. Restoring it *sent*
/// would be worse — a restart is the one moment nobody has reviewed what was
/// waiting — so it comes back visible and waiting for a person.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_message_survives_a_restart_and_comes_back_held() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            // A turn is running, so what follows queues behind it.
            buffered: vec![ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    handle.send_prompt("and this too", &[]).unwrap();
    assert!(!wait_until(300, || conn.sent().len() > 1), "it waits behind the running turn");

    // The host stops: the agent's stream closing is what the pump sees.
    drop(tx);
    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .is_some_and(|b| !b.queued.is_empty())),
        "the queue reached the blob: {:?}",
        blob::load(&settings, "s-1").map(|b| b.queued),
    );
    let stored = blob::load(&settings, "s-1").expect("the stored blob");
    assert_eq!(stored.queued.len(), 1, "one line was waiting: {:?}", stored.queued);
    assert_eq!(stored.queued[0].text, "and this too");
    assert_eq!(stored.queued[0].source, "remote", "and it keeps who sent it");

    // The restart.
    registry.unregister("s-1");
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let (_tx, rx) = std::sync::mpsc::channel();
    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: stored,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );

    assert!(
        wait_until(5_000, || handle.health().held_restored == 1),
        "the restored session says it is holding one message: {:?}",
        handle.health(),
    );
    // Nothing runs here, so the gate is open — and it still does not take it.
    assert!(
        !wait_until(500, || !conn.sent().is_empty()),
        "a restart does not send what nobody has reviewed: {:?}",
        conn.sent(),
    );
    let gate = handle.gate().expect("the pump attached its gate");
    let gate = gate.lock().unwrap();
    assert_eq!(
        gate.wait_of(gate.queued().next().expect("the line is in the queue")),
        oximux_agents::send_gate::SendWait::HeldRestored,
        "and it says why it is waiting",
    );
}

/// A restart that cut a turn short owes the agent a note, and the next
/// message carries it.
///
/// The agent's own process is gone but its session is not: resumed, it would
/// carry on from a state that no longer exists. The note says so once, ahead
/// of the next message rather than as a turn of its own — a turn nobody asked
/// for is a turn the person is billed for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_mid_turn_owes_a_note_the_next_message_carries() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let pumps = PumpSet::new();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            // A turn is running when the host is told to stop.
            buffered: vec![ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] }],
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        pumps.clone(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    // The host drains.
    pumps.finalize_all();
    assert!(wait_until(5_000, || pumps.live_pumps() == 0), "the pump finalized");
    let stored = blob::load(&settings, "s-1").expect("the stored blob");
    assert!(stored.mid_turn, "the blob says a turn was cut short");
    drop(tx);

    // The restart.
    registry.unregister("s-1");
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let (_tx, rx) = std::sync::mpsc::channel();
    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: stored,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.health_inputs().recovery_note_owed),
        "the restored session says it owes the agent a note: {:?}",
        handle.health(),
    );
    // And the health a client reads says it, not just the input the host acts
    // on. Without this the next message silently grew a paragraph nobody
    // typed, and no surface could warn about it.
    assert!(
        handle.health_snapshot().health.recovery_note_owed,
        "the published health carries it: {:?}",
        handle.health_snapshot().health,
    );
    assert!(conn.sent().is_empty(), "and the note is not a turn of its own");

    // A second restart before anything is sent keeps the *same* note: the
    // agent has to hear about the restart that interrupted it, and by now the
    // blob is the only thing that still says there was one — this host was not
    // mid-turn and would build nothing of its own.
    let carried = blob::load(&settings, "s-1").expect("the stored blob");
    assert!(!carried.mid_turn, "nothing was running under this host");
    let note = carried.recovery_note.clone().expect("the note is still owed, durably");
    assert!(note.contains("Your previous turn was interrupted"), "{note}");
    registry.unregister("s-1");
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let (_tx2, rx) = std::sync::mpsc::channel();
    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: carried,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.health_inputs().recovery_note_owed),
        "the note outlived a restart that had nothing of its own to report",
    );

    // The next message takes it.
    handle.send_prompt("carry on", &[]).unwrap();
    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the message went out");
    let line = conn.sent()[0].to_string();
    assert!(line.contains("Your previous turn was interrupted"), "the note went with it: {line}");
    assert!(line.contains("carry on"), "and so did the message: {line}");

    assert!(
        wait_until(5_000, || !handle.health_inputs().recovery_note_owed),
        "once sent it is not owed again: {:?}",
        handle.health(),
    );
    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .is_some_and(|b| b.recovery_note.is_none())),
        "nor after another restart: {:?}",
        blob::load(&settings, "s-1").map(|b| b.recovery_note),
    );
}

/// A session answers for itself by the time `start` says it has published.
///
/// `resume_one` reads `recovery_note_owed` the moment `open` returns, to
/// decide whether a child whose turn a restart cut short gets its recovery
/// turn. The pump publishes from its own task, so a caller that looks before
/// that reads the default — `false` — and gives up on the child instead of
/// retrying it. The ack is what the caller waits for, rather than a duration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_answers_for_itself_once_its_pump_says_it_published() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (_tx, rx) = std::sync::mpsc::channel();

    // A session the last host stopped mid-turn: it owes the agent a note.
    let mut seed = ChatBlob::new("s-1".into());
    seed.entries = vec![oximux_agents::thread::ThreadEntry::User {
        text: "ship it".into(),
        images: Vec::new(),
        checkpoint: None,
    }];
    seed.mid_turn = true;

    let published = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    published.await.expect("the pump published before it ended");

    // No waiting and no polling here on purpose: the ack is the claim under
    // test, so reading straight after it is what proves the claim.
    assert!(
        handle.health_inputs().recovery_note_owed,
        "the session says it owes a note as soon as it is open: {:?}",
        handle.health(),
    );
}

/// A provider's death is still this session's state after a host restart.
///
/// The frame that says a process died is the only live word on the subject:
/// it reaches whoever is subscribed at that instant, and then the registry
/// entry goes. So without recording it, a host restart left a session that
/// died reading *live* on its next message, with nothing anywhere to say what
/// had happened — measured on a real serve on 2026-11-17, which is why this
/// test exists.
///
/// Two halves, because the write and the read are in different places: the
/// exit lands in the blob with its code, and the next host reads it back as
/// the session's state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_providers_death_is_still_the_sessions_state_after_a_restart() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    // Killed, not asked to stop: `128 + SIGKILL`, the code a reader needs to
    // tell "something killed it" from "it closed its stream".
    conn.set_exit_code(Some(137));
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    // The agent's stream closing is the child exiting.
    drop(tx);

    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .is_some_and(|b| b.last_failure.is_some())),
        "the exit has to reach the disk: it is the only place it can outlive the host",
    );
    let persisted = blob::load(&settings, "s-1").expect("a blob");
    assert_eq!(
        persisted.last_failure.as_deref(),
        Some("agent process exited (code 137)"),
        "with the code, not a bare 'the connection closed': {persisted:?}",
    );
    // Waited for, not assumed: `save_now` commits the blob and returns, and
    // the unregister is a later statement — so the probe above can win the
    // race and this would fail against a pump doing exactly the right thing.
    assert!(
        wait_until(5_000, || registry.get("s-1").is_none()),
        "and the session went back to dormant",
    );

    // The next host, over the same blob. A fresh registration, as `resume`
    // makes: the point is what it says before anything runs.
    let conn = Arc::new(StubConnection::default());
    let handle = registry.register("s-1".into(), conn);
    let (_tx2, rx2) = std::sync::mpsc::channel();
    let published = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx2,
            buffered: Vec::new(),
            seed: persisted,
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    published.await.expect("the pump publishes before it acks");
    let health = handle.health_snapshot();
    assert_eq!(
        health.health.status,
        oximux_agents::health::HealthStatus::Failed,
        "a session whose provider died does not come back idle: {health:?}",
    );
    assert_eq!(
        health.health.reason.as_deref(),
        Some("agent process exited (code 137)"),
        "and it still says why",
    );
}

/// A turn that failed and a process that then exited leave **both** facts.
///
/// The exit is usually the failure's consequence — a provider reports "API
/// error 529" and dies — so a record that kept only the exit would replace
/// the explanation with its epitaph, and whoever reopened the session would
/// read that the process is gone with no idea why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_turn_keeps_its_explanation_when_the_process_then_exits() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = Arc::new(StubConnection::default());
    conn.set_exit_code(Some(7));
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            events: rx,
            buffered: Vec::new(),
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    tx.send(ThreadEvent::TurnEnded {
        result: Some("API error 529: overloaded".into()),
        usage: None,
        is_error: true,
        turn_diff: None,
    })
    .unwrap();
    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .is_some_and(|b| b.last_failure.is_some())),
        "the failed turn is the session's state first",
    );

    // And then the process goes.
    drop(tx);
    assert!(
        wait_until(5_000, || blob::load(&settings, "s-1")
            .and_then(|b| b.last_failure)
            .is_some_and(|f| f.contains("exited"))),
        "the exit reaches the blob",
    );
    let failure = blob::load(&settings, "s-1").and_then(|b| b.last_failure).expect("a failure");
    assert_eq!(
        failure, "API error 529: overloaded — agent process exited (code 7)",
        "the explanation, then how it ended — not one in place of the other",
    );
}

/// The dormant verdict and the pump's own first publish are the same answer
/// for the same blob, apart from the liveness a live process makes available.
///
/// This is the promise `serve::seed` makes, and the only way to hold it is to
/// ask both. A review read the code and concluded they diverge for a session
/// cut short mid-turn — that the pump would report a reason the dormant read
/// drops. It does not: the "restored, not confirmed" wording belongs to the
/// arm of `classify` where the process is *also* gone, and a resumed session
/// has one. Both cases are here because an argument about which arm runs is
/// exactly what a test should replace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dormant_verdict_agrees_with_what_the_pump_publishes_for_the_same_blob() {
    for (what, failure, was_mid_turn) in [
        ("a session a restart cut short", None, true),
        ("a session that died failing", Some("the provider refused".to_string()), false),
    ] {
        let registry = Arc::new(SessionRegistry::new());
        let conn = Arc::new(StubConnection::default());
        let handle = registry.register("s-1".into(), conn.clone());
        let db = oximux_storage::open_memory().unwrap();
        let settings = SettingsRepo::new(db.clone());
        let (_tx, rx) = std::sync::mpsc::channel();

        let mut seed = ChatBlob::new("s-1".into());
        seed.entries = vec![oximux_agents::thread::ThreadEntry::User {
            text: "ship it".into(),
            images: Vec::new(),
            checkpoint: None,
        }];
        seed.last_failure = failure;
        seed.mid_turn = was_mid_turn;

        // Read off disk, with nothing running it.
        let dormant = super::super::seed::verdict("s-1", seed.clone());

        let _pump = start(
            PumpSpec {
                session_id: "s-1".into(),
                handle: handle.clone(),
                events: rx,
                buffered: Vec::new(),
                seed,
                settings: settings.clone(),
                registry: registry.clone(),
                index: Arc::new(SessionIndex::default()),
                retry: retry_driver(registry.clone(), &settings, &db),
                on_end: None,
            },
            PumpSet::new(),
        );
        assert!(
            wait_until(5_000, || handle.health_published()),
            "{what}: the pump published",
        );
        let live = handle.health();

        assert_eq!(
            (live.status, live.reason.clone()),
            (dormant.status, dormant.reason.clone()),
            "{what}: what the session *is* must not change when something opens it — \
             otherwise reading it without opening it is worse than not reading it",
        );
        assert_eq!(
            dormant.liveness,
            oximux_agents::health::Liveness::Unverifiable,
            "{what}: nothing rebuilt this session, so nothing is unconfirmed — \
             there is simply nothing that can tell",
        );
        assert_eq!(
            live.liveness,
            oximux_agents::health::Liveness::RestoredUnconfirmed,
            "{what}: and the resumed one has a process whose session is not confirmed yet",
        );
    }
}

/// A Claude-shaped backend: it takes a line mid-turn and reports, per line,
/// that it started on it. Both halves are required for steering — without the
/// receipt a Stop could not take the line back.
fn steerable_agent() -> Arc<StubConnection> {
    Arc::new(StubConnection::default().with_receipts().with_capabilities(
        oximux_agents::thread::AgentCapabilities { supports_steer: true, ..Default::default() },
    ))
}

/// Serve's own steer arm, and the row it exists for: a child's event reaching a
/// busy Claude parent goes **into** the running turn.
///
/// The failure this guards is not "it waits": it is that the gate marks a
/// steerable line sendable, and a host with no steer arm writes it as an
/// ordinary prompt — a second turn on work the first is already doing. That is
/// what serve did while `steer_supported` was hardcoded false and nothing
/// answered `Ready::Steer`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_event_is_steered_into_the_running_turn_rather_than_starting_a_second_one() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = steerable_agent();
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            // A turn that is running *and* has said something: a steer goes
            // into a turn that has started, not one still opening.
            buffered: vec![
                ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] },
                ThreadEvent::AssistantText("on it".into()),
            ],
            events: rx,
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    // What the outbox drainer does with a child's events.
    handle.gate().unwrap().lock().unwrap().push(
        "a child finished".into(),
        Vec::new(),
        SendSource::ParentEvent,
        DeliveryMode::Steer,
        None,
    );

    assert!(
        wait_until(5_000, || conn.sent().iter().any(|s| s["type"] == "steer")),
        "the child's event went into the running turn: {:?}",
        conn.sent(),
    );
    let sent = conn.sent();
    assert_eq!(sent.len(), 1, "once, and nothing else: {sent:?}");
    assert!(
        sent[0].to_string().contains("a child finished"),
        "and it carried the child's text: {sent:?}",
    );
    assert!(
        sent[0]["uuid"].is_string(),
        "under an id the agent's receipt can name, or the outbox row it carries never settles: {sent:?}",
    );
    let _ = tx;
}

/// And it waits while a prompt is open, because the person has not answered
/// yet — then goes once nothing is waiting on them.
///
/// The matrix row. A line folded into a turn that is itself blocked on a
/// permission would be answered by nobody and acted on at an unpredictable
/// moment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steerable_line_waits_while_a_prompt_is_open_and_goes_once_it_is_gone() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = steerable_agent();
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            buffered: vec![
                ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] },
                ThreadEvent::AssistantText("on it".into()),
                ThreadEvent::PermissionRequested {
                    request_id: "req-1".into(),
                    tool_use_id: Some("tu-1".into()),
                    tool_name: "Bash".into(),
                    input: serde_json::json!({ "command": "ls" }),
                    description: "run ls".into(),
                    suggestions: vec![],
                    kind: oximux_agent_core::thread::tool_call::PermissionKind::Tool,
                    args_digest: None,
                },
            ],
            events: rx,
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.health().status
            == oximux_agents::health::HealthStatus::NeedsPermission),
        "the session is waiting on a person: {:?}",
        handle.health(),
    );

    handle.gate().unwrap().lock().unwrap().push(
        "a child finished".into(),
        Vec::new(),
        SendSource::ParentEvent,
        DeliveryMode::Steer,
        None,
    );
    assert!(
        !wait_until(300, || !conn.sent().is_empty()),
        "nothing goes into a turn that is itself waiting on a person: {:?}",
        conn.sent(),
    );
    {
        let gate = handle.gate().unwrap();
        let gate = gate.lock().unwrap();
        let item = gate.queued().next().expect("the line is queued").clone();
        assert_eq!(
            gate.wait_of(&item),
            oximux_agents::send_gate::SendWait::Interaction,
            "and it says why, so a client can answer \"why isn't this sending?\"",
        );
    }

    // The prompt is answered and the tool runs: nothing waits on the person.
    tx.send(ThreadEvent::ToolResult {
        tool_use_id: "tu-1".into(),
        content: "a.txt".into(),
        is_error: false,
        structured: None,
    })
    .unwrap();
    assert!(
        wait_until(5_000, || conn.sent().iter().any(|s| s["type"] == "steer")),
        "and then it goes, into the same turn: {:?}",
        conn.sent(),
    );
}

/// A steerable line waits for the turn to say it has started.
///
/// `turn_started` is the field serve used to hardcode `false`, which left a
/// steerable line in `Starting` for the whole turn — the shape the gate uses
/// to say "a turn is opening and has not spoken yet". A line written then
/// would race the turn's own first prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steerable_line_waits_for_the_turn_to_actually_start() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = steerable_agent();
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            // A turn is open, and the agent has said nothing yet.
            buffered: vec![ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] }],
            events: rx,
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );

    handle.gate().unwrap().lock().unwrap().push(
        "a child finished".into(),
        Vec::new(),
        SendSource::ParentEvent,
        DeliveryMode::Steer,
        None,
    );
    assert!(
        !wait_until(300, || !conn.sent().is_empty()),
        "nothing is written into a turn that has not spoken: {:?}",
        conn.sent(),
    );

    // The turn speaks.
    tx.send(ThreadEvent::AssistantText("on it".into())).unwrap();
    assert!(
        wait_until(5_000, || conn.sent().iter().any(|s| s["type"] == "steer")),
        "and now the line goes in: {:?}",
        conn.sent(),
    );
}

/// A steered line the agent has not started on keeps the turn open.
///
/// The third field this arm needs, and the subtlest: a line written into a
/// turn may still *begin* a turn of its own when that turn ends before the
/// agent gets to it. Until its receipt says it started, the turn is not over —
/// so the next queued line waits rather than racing it. Without this a child's
/// event and a person's message could open two turns on the same work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steered_line_the_agent_has_not_started_keeps_the_turn_open() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = steerable_agent();
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            buffered: vec![
                ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] },
                ThreadEvent::AssistantText("on it".into()),
            ],
            events: rx,
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );
    handle.gate().unwrap().lock().unwrap().push(
        "a child finished".into(),
        Vec::new(),
        SendSource::ParentEvent,
        DeliveryMode::Steer,
        None,
    );
    assert!(
        wait_until(5_000, || conn.sent().iter().any(|s| s["type"] == "steer")),
        "the line went into the turn: {:?}",
        conn.sent(),
    );

    // The turn ends with no receipt for that line: the agent never said it
    // started on it, so it may yet run as a turn of its own.
    tx.send(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None })
        .unwrap();
    assert!(
        wait_until(5_000, || handle
            .gate()
            .is_some_and(|g| g.lock().unwrap().awaiting_steer())),
        "the gate is still waiting on the steered line",
    );

    // So the next line waits, rather than opening a second turn beside it.
    handle.send_prompt("and this too", &[]).unwrap();
    assert!(
        !wait_until(400, || conn.sent().len() > 1),
        "a queued line must not start a turn while a steered one may still begin one: {:?}",
        conn.sent(),
    );
}

/// The receipt releases the line, and the gate opens again.
///
/// The bug this exists for: serve had no receipt handler at all, so the first
/// steer held `unstarted` for good — and because an unstarted steer keeps the
/// turn open on purpose, every later line read `Busy` until a Stop or a
/// restart. One steered child event deadlocked the session's queue. Two
/// reviewers found it; no test did, because none sent a receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_receipt_releases_the_steered_line_and_the_queue_moves_again() {
    let registry = Arc::new(SessionRegistry::new());
    let conn = steerable_agent();
    let handle = registry.register("s-1".into(), conn.clone());
    let db = oximux_storage::open_memory().unwrap();
    let settings = SettingsRepo::new(db.clone());
    let (tx, rx) = std::sync::mpsc::channel();

    let _pump = start(
        PumpSpec {
            session_id: "s-1".into(),
            handle: handle.clone(),
            buffered: vec![
                ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] },
                ThreadEvent::AssistantText("on it".into()),
            ],
            events: rx,
            seed: ChatBlob::new("s-1".into()),
            settings: settings.clone(),
            registry: registry.clone(),
            index: Arc::new(SessionIndex::default()),
            retry: retry_driver(registry.clone(), &settings, &db),
            on_end: None,
        },
        PumpSet::new(),
    );
    assert!(
        wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
        "the pump attached its gate",
    );
    handle.gate().unwrap().lock().unwrap().push(
        "a child finished".into(),
        Vec::new(),
        SendSource::ParentEvent,
        DeliveryMode::Steer,
        None,
    );
    assert!(
        wait_until(5_000, || conn.sent().iter().any(|s| s["type"] == "steer")),
        "the line went in: {:?}",
        conn.sent(),
    );
    let uuid = conn.sent()[0]["uuid"].as_str().expect("tagged").to_string();
    // A subscriber sees what caused the turn to change course. No backend
    // echoes the user's own half, so without the ingest the stream shows the
    // agent reacting to nothing.
    assert!(
        handle
            .events_since(0)
            .iter()
            .any(|(_, event)| {
                matches!(event, ThreadEvent::UserMessage { text, .. } if text == "a child finished")
            }),
        "the steered line reached the event stream",
    );

    // The agent says it started on it, and the turn then ends.
    tx.send(ThreadEvent::UserMessageAccepted { uuid: uuid.clone() }).unwrap();
    assert!(
        wait_until(5_000, || handle
            .gate()
            .is_some_and(|g| !g.lock().unwrap().awaiting_steer())),
        "the receipt took it out of the gate's unstarted list",
    );
    tx.send(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None })
        .unwrap();

    // So an ordinary line can open the next turn.
    handle.send_prompt("and now this", &[]).unwrap();
    assert!(
        wait_until(5_000, || conn.sent().len() > 1),
        "the queue moves again once the steered line was answered for: {:?}",
        conn.sent(),
    );
}

/// A Stop asks the **backend** to take the steered lines back, and settles
/// only the ones it confirms.
///
/// The second bug: `stop_the_turn` forgot the lines locally and called a plain
/// `cancel`, so Claude could still run a line it had already queued while the
/// outbox handed the same child events out again — the event delivered twice,
/// and a turn starting after the Stop. The desktop passes the ids to
/// `stop_turn`; serve now does too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_asks_the_backend_to_take_the_steered_line_back() {
    for confirms in [true, false] {
        let registry = Arc::new(SessionRegistry::new());
        let conn = Arc::new(
            StubConnection::default()
                .with_receipts()
                .with_capabilities(oximux_agents::thread::AgentCapabilities {
                    supports_steer: true,
                    ..Default::default()
                }),
        );
        let conn = if confirms { Arc::new((*conn).clone().with_take_back()) } else { conn };
        let handle = registry.register("s-1".into(), conn.clone());
        let db = oximux_storage::open_memory().unwrap();
        let settings = SettingsRepo::new(db.clone());
        let (_tx, rx) = std::sync::mpsc::channel();

        let _pump = start(
            PumpSpec {
                session_id: "s-1".into(),
                handle: handle.clone(),
                buffered: vec![
                    ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] },
                    ThreadEvent::AssistantText("on it".into()),
                ],
                events: rx,
                seed: ChatBlob::new("s-1".into()),
                settings: settings.clone(),
                registry: registry.clone(),
                index: Arc::new(SessionIndex::default()),
                retry: retry_driver(registry.clone(), &settings, &db),
                on_end: None,
            },
            PumpSet::new(),
        );
        assert!(
            wait_until(5_000, || handle.gate().is_some_and(|g| g.lock().unwrap().has_waker())),
            "the pump attached its gate",
        );
        handle.gate().unwrap().lock().unwrap().push(
            "a child finished".into(),
            Vec::new(),
            SendSource::ParentEvent,
            DeliveryMode::Steer,
            None,
        );
        assert!(
            wait_until(5_000, || conn.sent().iter().any(|s| s["type"] == "steer")),
            "the line went in: {:?}",
            conn.sent(),
        );
        let uuid = conn.sent()[0]["uuid"].as_str().expect("tagged").to_string();

        handle.cancel().unwrap();
        assert!(
            wait_until(5_000, || !conn.stopped().is_empty()),
            "the Stop reached the backend as a take-back, not a bare cancel",
        );
        assert_eq!(
            conn.stopped(),
            vec![uuid.clone()],
            "naming the line it must dequeue, or the agent runs it after the Stop",
        );
        assert!(
            wait_until(5_000, || handle.gate().is_some_and(|g| {
                // Confirmed: settled, and the gate waits on nothing. Not
                // confirmed: still tracked, because it may yet run.
                g.lock().unwrap().awaiting_steer() != confirms
            })),
            "confirms={confirms}: a line the backend did not confirm stays tracked",
        );
    }
}
