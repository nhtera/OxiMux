//! The schedule RPCs (v10): create, list, delete, toggle, and read run history
//! for the desktop's scheduled agent runs.
//!
//! These name no session, so the interesting axis is device *tier*, not
//! `is_allowed_for`. Reads want full scope; writes want full-and-not-read-only.
//! Every refusal is asserted at the **store**, not on the wire — an
//! `Unauthorized` reply that still mutated the database would look identical from
//! the client's side.

use std::sync::Arc;

use oximux_agents::schedule::ScheduleStore;
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::messages::{RecurrenceWire, RegisterReq};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_proto::Transport;

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

/// An empty in-memory store with the app's schema applied, shared by `Arc` so a
/// test can inspect the same rows the dispatcher writes.
fn store() -> Arc<ScheduleStore> {
    let db = oximux_storage::open_memory().expect("open memory db");
    Arc::new(ScheduleStore::new(db.conn()))
}

fn daily(hour: u8, minute: u8) -> RecurrenceWire {
    RecurrenceWire::DailyAt { hour, minute }
}

fn create_req(name: &str, recurrence: RecurrenceWire) -> Request {
    Request::CreateSchedule {
        name: name.into(),
        cwd: "/work".into(),
        prompt: "run the nightly report".into(),
        agent_id: None,
        recurrence,
    }
}

/// The full lifecycle from a read-write device: create, list, toggle off,
/// delete — each observable in the shared store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_device_creates_lists_toggles_and_deletes() {
    let store = store();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_schedule_store(Arc::clone(&store));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        // Create returns the stored row, id and next-fire included, so the phone
        // never re-lists to learn what it just made.
        let created = call(&client, create_req("Nightly", daily(9, 0))).await;
        let Response::ScheduleCreated(sched) = created else {
            panic!("expected ScheduleCreated, got {created:?}");
        };
        assert_eq!(sched.name, "Nightly");
        assert!(sched.enabled);
        assert_eq!(sched.summary, "daily at 09:00", "carries the desktop's own phrasing");
        assert!(!sched.next_fire_at.is_empty(), "a concrete next fire, not a placeholder");
        let id = sched.id.clone();

        let listed = call(&client, Request::ListSchedules).await;
        let Response::Schedules(rows) = listed else {
            panic!("expected Schedules, got {listed:?}");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);

        // A freshly created schedule has never fired.
        let runs = call(&client, Request::GetScheduleRuns { schedule_id: id.clone(), limit: 20 }).await;
        assert_eq!(runs, Response::ScheduleRuns(vec![]), "empty history is normal, not an error");

        let toggled =
            call(&client, Request::SetScheduleEnabled { id: id.clone(), enabled: false }).await;
        assert_eq!(toggled, Response::Ack);

        let deleted = call(&client, Request::DeleteSchedule { id: id.clone() }).await;
        assert_eq!(deleted, Response::Ack);
        // Idempotent: deleting one already gone is still success.
        let again = call(&client, Request::DeleteSchedule { id }).await;
        assert_eq!(again, Response::Ack);
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert!(store.list().unwrap().is_empty(), "the delete really removed the row");
}

/// A read-only device may watch the schedule list but cannot arm one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_lists_but_cannot_create() {
    let store = store();
    // Seed one schedule directly so there is something to read.
    store
        .create(
            oximux_agents::schedule::NewSchedule {
                name: "Seeded".into(),
                cwd: "/work".into(),
                prompt: "hi".into(),
                agent_id: None,
                recurrence: oximux_agents::schedule::Recurrence::DailyAt { hour: 8, minute: 0 },
            },
            chrono::Local::now(),
        )
        .unwrap();

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_schedule_store(Arc::clone(&store));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        auth.set_read_only(&pubkey, true);

        // The read still works.
        let listed = call(&client, Request::ListSchedules).await;
        let Response::Schedules(rows) = listed else {
            panic!("expected Schedules, got {listed:?}");
        };
        assert_eq!(rows.len(), 1, "a read-only full device still sees the list");

        // The write does not.
        let created = call(&client, create_req("Nope", daily(9, 0))).await;
        assert_eq!(created, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert_eq!(store.list().unwrap().len(), 1, "the refused create wrote nothing");
}

/// A session-scoped device is refused schedules entirely — it has no session to
/// be narrowed to, and a schedule can target any project.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_scoped_device_is_refused_schedules() {
    let store = store();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, Some("sess-1".into()), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_schedule_store(Arc::clone(&store));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let mut req = register_req([0x33; 32]);
        req.session_id = Some("sess-1".into());
        let Response::Registered { .. } = call(&client, Request::Register(req)).await else {
            panic!("expected Registered");
        };
        // Even the read is refused: listing would leak cross-project prompts and
        // paths outside this device's one conversation.
        let listed = call(&client, Request::ListSchedules).await;
        assert_eq!(listed, Response::Error(RpcError::Unauthorized));
        let created = call(&client, create_req("Nope", daily(9, 0))).await;
        assert_eq!(created, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert!(store.list().unwrap().is_empty(), "nothing reached the store");
}

/// A host with no schedule store answers `Unauthorized`, not a distinct
/// "unsupported" — whether this desktop keeps schedules is not something an
/// unauthorized client should be able to probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_a_store_is_indistinguishable_from_one_that_refuses() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    // Note: no `.with_schedule_store(...)`.
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let listed = call(&client, Request::ListSchedules).await;
        assert_eq!(listed, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A recurrence the phone's pickers could never produce — an interval under the
/// floor — is refused with a client-safe reason and never stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recurrence_under_the_floor_is_refused_before_storing() {
    let store = store();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_schedule_store(Arc::clone(&store));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        // One minute is below the 5-minute floor the desktop enforces.
        let created =
            call(&client, create_req("TooFast", RecurrenceWire::EveryMinutes { minutes: 1 })).await;
        let Response::Error(RpcError::BadRequest(msg)) = created else {
            panic!("expected BadRequest, got {created:?}");
        };
        assert!(msg.contains("5 minutes"), "the reason names the rule, got {msg:?}");
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert!(store.list().unwrap().is_empty(), "the invalid create stored nothing");
}
