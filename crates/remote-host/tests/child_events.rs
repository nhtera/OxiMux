//! A child's events reaching its parent: the watcher writes them to the
//! outbox, the drainer moves them into the parent's send gate as one batch,
//! and receipts settle them. Against a real host core with stand-in sessions.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oximux_agents::lineage::{
    LineageStore, NotifyParent, Reservation, SpawnCaps, SpawnOrigin, SpawnParent, SpawnSpec,
};
use oximux_agents::outbox::OutboxState;
use oximux_agents::send_gate::{QueuedSend, SendGate, SendSource};
use oximux_agents::session_registry::{SessionHandle, SessionMeta, SessionRegistry};
use oximux_agents::thread::launch_posture::SpawnPosture;
use oximux_agents::thread::{StubConnection, ThreadEvent};
use oximux_remote_host::{HostConfig, HostCore, HostCoreDeps, PriorOwner};

struct Host {
    core: Arc<HostCore>,
    registry: Arc<SessionRegistry>,
    _rt: tokio::runtime::Runtime,
}

fn host() -> Host {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let db = oximux_storage::db::open_memory().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    core.set_event_debounce(Duration::from_millis(150));
    core.start(PriorOwner::None);
    Host { core, registry, _rt: rt }
}

/// A parent session driven like the desktop drives one: its gate attached,
/// with a waker that counts.
struct Parent {
    gate: Arc<Mutex<SendGate>>,
    wakes: Arc<AtomicUsize>,
    handle: Arc<SessionHandle>,
}

fn open_parent(h: &Host, id: &str) -> Parent {
    let handle = h.registry.register(id.into(), Arc::new(StubConnection::default()));
    let gate = Arc::new(Mutex::new(SendGate::new()));
    let wakes = Arc::new(AtomicUsize::new(0));
    let seen = wakes.clone();
    gate.lock().unwrap().set_waker(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });
    handle.set_gate(Some(gate.clone()));
    Parent { gate, wakes, handle }
}

fn spawn(h: &Host, parent: &str, child: &str, notify: NotifyParent) {
    let spec = SpawnSpec {
        request_id: child.into(),
        origin: SpawnOrigin::Agent,
        posture: SpawnPosture::Default,
        notify_parent: notify,
        allow_parent_permits: false,
        preassigned_id: None,
        fingerprint_input: child.into(),
    };
    let r = h
        .core
        .lineage()
        .reserve(&SpawnParent::Session(parent.into()), &spec, SpawnCaps::default(), &|_: &str| true, chrono::Local::now())
        .unwrap();
    let Ok(Reservation::Fresh { row_id, .. }) = r else { panic!("{r:?}") };
    h.core.lineage().finalize(row_id, child, chrono::Local::now()).unwrap();
}

fn open_child(h: &Host, child: &str, title: &str) -> Arc<SessionHandle> {
    let handle = h.registry.register(child.into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: Some(title.into()), ..Default::default() });
    handle.set_working(true);
    handle
}

fn finish_turn(child: &SessionHandle, reply: &str) {
    child.ingest(ThreadEvent::UserMessage { text: "go".into(), images: vec![] });
    child.ingest(ThreadEvent::AssistantText(reply.into()));
    child.ingest(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
    child.set_working(false);
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn queued_events(p: &Parent) -> Vec<QueuedSend> {
    p.gate.lock().unwrap().queued().filter(|q| q.source == SendSource::ParentEvent).cloned().collect()
}

fn states(h: &Host, parent: &str) -> Vec<OutboxState> {
    h.core.outbox().rows_for(parent).unwrap().into_iter().map(|r| r.state).collect()
}

#[test]
fn a_finished_child_reaches_its_parent_and_receipts_settle_it() {
    let h = host();
    let parent = open_parent(&h, "parent");
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    let child = open_child(&h, "child-1", "Summarize the README");
    finish_turn(&child, "The README explains the build.");

    let batch = wait_for("the envelope in the parent's gate", || queued_events(&parent).into_iter().next());
    assert!(batch.text.contains("kind=\"finished\" child=\"child-1\""), "{}", batch.text);
    assert!(batch.text.contains("Child \"Summarize the README\" finished."));
    assert!(batch.text.contains("<result>The README explains the build.</result>"));
    assert!(parent.wakes.load(Ordering::SeqCst) >= 1, "the owner was woken");
    assert_eq!(states(&h, "parent"), [OutboxState::Claimed]);
    assert_eq!(h.core.pending_events("parent"), 1);

    // The owner sends it; the CLI's receipts settle it.
    let item = parent.gate.lock().unwrap().take_next_if_open().expect("sendable");
    parent.gate.lock().unwrap().report_sent(item.uuid, true);
    wait_for("sent", || (states(&h, "parent") == [OutboxState::Sent]).then_some(()));
    parent.handle.ingest(ThreadEvent::UserMessageAccepted { uuid: item.uuid.to_string() });
    wait_for("accepted", || (states(&h, "parent") == [OutboxState::Accepted]).then_some(()));
    parent.handle.ingest(ThreadEvent::UserMessagesDelivered { uuids: vec![item.uuid.to_string()] });
    wait_for("delivered", || (states(&h, "parent") == [OutboxState::Delivered]).then_some(()));
    assert_eq!(h.core.pending_events("parent"), 0);
}

#[test]
fn children_that_finish_together_arrive_as_one_message() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    let children: Vec<_> = (1..=3)
        .map(|n| {
            let id = format!("child-{n}");
            spawn(&h, "parent", &id, NotifyParent::Auto);
            open_child(&h, &id, &format!("task {n}"))
        })
        .collect();
    for child in &children {
        finish_turn(child, "done");
    }
    wait_for("all three rows claimed", || (states(&h, "parent") == [OutboxState::Claimed; 3]).then_some(()));
    let queued = queued_events(&parent);
    assert_eq!(queued.len(), 1, "one envelope");
    assert_eq!(queued[0].text.matches("<oximux-event ").count(), 3);
}

/// A batch the parent's gate is still holding (its turn is running) is taken
/// back and re-sent with a newer event: still one message.
#[test]
fn a_held_batch_is_merged_with_a_newer_event() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    spawn(&h, "parent", "child-2", NotifyParent::Auto);
    finish_turn(&open_child(&h, "child-1", "one"), "first");
    let first = wait_for("the first batch", || queued_events(&parent).into_iter().next());
    finish_turn(&open_child(&h, "child-2", "two"), "second");
    let merged = wait_for("the merged batch", || {
        queued_events(&parent).into_iter().find(|q| q.text.matches("<oximux-event ").count() == 2)
    });
    assert_ne!(merged.uuid, first.uuid, "a new key for a new set of rows");
    assert_eq!(queued_events(&parent).len(), 1);
}

/// A child that finished its whole turn before anything watched it (and a
/// second look at the same turn) still yields exactly one event.
#[test]
fn a_turn_finished_before_the_watcher_looked_is_reported_once() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    let child = h.registry.register("child-1".into(), Arc::new(StubConnection::default()));
    finish_turn(&child, "fast");
    child.set_watched(true); // as the watcher will, so the ring kept the turn
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    h.registry.get("child-1").unwrap().set_working(true); // a registry change: the watcher attaches
    wait_for("the event", || queued_events(&parent).into_iter().next());
    h.registry.get("child-1").unwrap().set_working(false); // another look
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(h.core.outbox().rows_for("parent").unwrap().len(), 1);
}

#[test]
fn a_child_set_to_stuck_reports_only_when_stuck() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    spawn(&h, "parent", "child-1", NotifyParent::Stuck);
    let child = open_child(&h, "child-1", "quiet");
    finish_turn(&child, "done");
    std::thread::sleep(Duration::from_millis(400));
    assert!(h.core.outbox().rows_for("parent").unwrap().is_empty(), "a plain finish is not reported");
    child.ingest(ThreadEvent::UserMessage { text: "again".into(), images: vec![] });
    child.ingest(ThreadEvent::TurnEnded { result: Some("boom".into()), usage: None, is_error: true, turn_diff: None });
    let batch = wait_for("the failure", || queued_events(&parent).into_iter().next());
    assert!(batch.text.contains("kind=\"failed\""), "{}", batch.text);
}

/// A parent whose tab is not open is never opened to deliver: its events wait
/// and go out once it binds.
#[test]
fn a_dormant_parent_gets_its_events_when_it_binds() {
    let h = host();
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    finish_turn(&open_child(&h, "child-1", "task"), "done");
    wait_for("the row", || (states(&h, "parent") == [OutboxState::Pending]).then_some(()));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(states(&h, "parent"), [OutboxState::Pending], "nothing to deliver to");
    let parent = open_parent(&h, "parent");
    parent.handle.set_working(false); // a registry change, as a bind makes
    wait_for("delivery on bind", || queued_events(&parent).into_iter().next());
}

/// The parent's session goes away with a batch still in its gate: the batch
/// returns to pending for the next bind.
#[test]
fn a_parent_that_closes_gets_its_held_batch_back() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    finish_turn(&open_child(&h, "child-1", "task"), "done");
    wait_for("claimed", || (states(&h, "parent") == [OutboxState::Claimed]).then_some(()));
    h.registry.unregister("parent");
    wait_for("released", || (states(&h, "parent") == [OutboxState::Pending]).then_some(()));
}

#[test]
fn a_cancelled_child_reports_cancelled_not_finished() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    let ended = h.core.lineage().mark_ended("child-1", false, chrono::Local::now()).unwrap();
    h.core.note_ended(&ended);
    let batch = wait_for("the cancel", || queued_events(&parent).into_iter().next());
    assert!(batch.text.contains("kind=\"cancelled\""), "{}", batch.text);
    assert!(!batch.text.contains("kind=\"finished\""));
}

#[test]
fn a_permission_prompt_on_a_child_is_reported_with_its_request() {
    let h = host();
    let parent = open_parent(&h, "parent");
    parent.gate.lock().unwrap().begin_turn(SendSource::User);
    spawn(&h, "parent", "child-1", NotifyParent::Auto);
    let child = open_child(&h, "child-1", "task");
    child.ingest(ThreadEvent::PermissionRequested {
        request_id: "req-9".into(),
        tool_use_id: Some("tu-1".into()),
        tool_name: "Read".into(),
        description: String::new(),
        input: serde_json::json!({ "file_path": "/repo/a.rs" }),
        kind: Default::default(),
        suggestions: Vec::new(),
    });
    let batch = wait_for("the permit", || queued_events(&parent).into_iter().next());
    assert!(batch.text.contains("kind=\"needs_permission\""), "{}", batch.text);
    assert!(batch.text.contains("<permit req=\"req-9\" answerable=\"false\" tool=\"Read\">"));
}
