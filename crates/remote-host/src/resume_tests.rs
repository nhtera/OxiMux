//! Recovery turns after a restart: who gets one, how many at once, and that a
//! second pass over the same restart sends nothing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use oximux_agents::health::HealthInputs;
use oximux_agents::lineage::{
    LineageStore, NotifyParent, Reservation, SpawnCaps, SpawnOrigin, SpawnParent, SpawnSpec,
};
use oximux_agents::send_gate::SendGate;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::connection::StubConnection;
use oximux_agents::thread::launch_posture::SpawnPosture;
use oximux_single_instance::HolderRecord;

use super::*;
use crate::catalog::{DormantChoices, DormantSession, DormantTranscript, SessionCatalog};
use crate::host_core::HostCoreDeps;
use crate::host_config::HostConfig;

/// Builds a child the way the desktop does: registers it with a stub agent.
struct Catalog {
    registry: Arc<SessionRegistry>,
    agents: Mutex<HashMap<String, Arc<StubConnection>>>,
    /// Each opened child's send gate, armed as the view that drives it arms
    /// it: what the host queues there is what the view would write.
    gates: Mutex<HashMap<String, Arc<Mutex<SendGate>>>>,
    opened: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SessionCatalog for Catalog {
    fn dormant(&self) -> Vec<DormantSession> {
        Vec::new()
    }
    fn transcript(&self, _: &str) -> Option<DormantTranscript> {
        None
    }
    fn choices(&self, _: &str) -> Option<DormantChoices> {
        None
    }
    async fn open(&self, session_id: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(session_id.to_string());
        if self.registry.get(session_id).is_none() {
            let agent = Arc::new(StubConnection::default());
            self.agents.lock().unwrap().insert(session_id.to_string(), agent.clone());
            // A restored chat cut short mid-turn owes its note from the start.
            let handle = self.registry.register(session_id.to_string(), agent);
            let gate = Arc::new(Mutex::new(SendGate::new()));
            gate.lock().unwrap().set_waker(|| {});
            handle.set_gate(Some(gate.clone()));
            self.gates.lock().unwrap().insert(session_id.to_string(), gate);
            handle.publish_health(HealthInputs { recovery_note_owed: true, ..HealthInputs::default() });
        }
        Ok(())
    }
}

impl Catalog {
    /// Recovery turns sent to `child`: queued for its view, or (never, for
    /// a recovery) written to its agent directly.
    fn recoveries(&self, child: &str) -> usize {
        let queued = self.gates.lock().unwrap().get(child).map_or(0, |g| {
            g.lock().unwrap().queued().filter(|q| q.source == SendSource::Recovery && q.text == CONTINUE_LINE).count()
        });
        let written = self.agents.lock().unwrap().get(child).map_or(0, |a| {
            a.sent().iter().filter(|m| m.to_string().contains(CONTINUE_LINE)).count()
        });
        queued + written
    }

    fn total(&self) -> usize {
        let children: Vec<String> = self.agents.lock().unwrap().keys().cloned().collect();
        children.iter().map(|c| self.recoveries(c)).sum()
    }
}

struct Fixture {
    core: Arc<HostCore>,
    registry: Arc<SessionRegistry>,
    catalog: Arc<Catalog>,
    _rt: tokio::runtime::Runtime,
}

fn fixture(max_resumes: u32) -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let db = oximux_storage::db::open_memory().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig { max_resumes, ..HostConfig::default() },
    });
    let catalog = Arc::new(Catalog { registry: registry.clone(), agents: Mutex::default(), gates: Mutex::default(), opened: Mutex::default() });
    core.set_catalog(catalog.clone());
    Fixture { core, registry, catalog, _rt: rt }
}

/// A child the previous host had working (its row `active`).
fn working_child(f: &Fixture, parent: &str, child: &str, origin: SpawnOrigin) {
    let spec = SpawnSpec {
        request_id: child.into(),
        origin,
        posture: SpawnPosture::default(),
        notify_parent: NotifyParent::Auto,
        allow_parent_permits: false,
        preassigned_id: None,
        fingerprint_input: child.into(),
    };
    let caps = SpawnCaps { depth: 2, active_per_parent: 16, active_per_root: 16, open_per_root: 32, ..SpawnCaps::default() };
    let r = f.core.lineage().reserve(&SpawnParent::Session(parent.into()), &spec, caps, &f.core.working(), chrono::Local::now()).unwrap();
    let Ok(Reservation::Fresh { row_id, .. }) = r else { panic!("{r:?}") };
    f.core.lineage().finalize(row_id, child, chrono::Local::now()).unwrap();
}

fn previous_host() -> PriorOwner {
    PriorOwner::Gone {
        record: HolderRecord { pid: 4242, started_at_unix_s: Some(EPOCH), boot_time_unix_s: None, role: None },
        same_boot: true,
    }
}

fn open_parent(f: &Fixture, parent: &str) {
    f.registry.register(parent.to_string(), Arc::new(StubConnection::default()));
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Long enough for a driver that would act to have acted.
fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

/// The child's agent took its recovery turn and finished it before the host
/// ever saw it working (the driver reads the receipt, not only the flag).
fn finish(f: &Fixture, child: &str) {
    let line = key_hash(KeyKind::Gate, &send_uuid(&continuation_id(child, EPOCH)).to_string());
    f.core
        .lineage()
        .receipts()
        .settle(child, &line, ReceiptState::Accepted, "view", chrono::Local::now())
        .unwrap();
    // Any registry change wakes the driver.
    f.registry.register(format!("nudge-{child}"), Arc::new(StubConnection::default()));
}

const EPOCH: u64 = 1_700_000_000;

#[test]
fn five_children_cut_short_three_resume_and_the_rest_wait_their_turn() {
    let f = fixture(3);
    open_parent(&f, "root");
    for c in ["c1", "c2", "c3", "c4", "c5"] {
        working_child(&f, "root", c, SpawnOrigin::Agent);
    }
    f.core.start(previous_host());
    wait_for("three recovery turns", || f.catalog.total() == 3);
    settle();
    assert_eq!(f.catalog.total(), 3, "the cap holds the other two back");
    let first = f.catalog.agents.lock().unwrap().keys().next().cloned().unwrap();
    finish(&f, &first);
    wait_for("a fourth once one finished", || f.catalog.total() == 4);
    for c in ["c1", "c2", "c3", "c4", "c5"] {
        assert!(f.catalog.recoveries(c) <= 1, "{c} was sent one recovery turn at most");
    }
}

#[test]
fn a_second_pass_over_the_same_restart_sends_nothing() {
    let f = fixture(3);
    open_parent(&f, "root");
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    f.core.start(previous_host());
    wait_for("the recovery turn", || f.catalog.recoveries("c1") == 1);
    // The lock is given up and won back over the same previous host while the
    // child still reads as cut short.
    f.core.release();
    f.core.lineage().activate("c1", chrono::Local::now()).unwrap();
    f.core.start(previous_host());
    settle();
    assert_eq!(f.catalog.recoveries("c1"), 1, "the restart's key was already used");
}

#[test]
fn with_no_previous_host_nothing_is_resumed() {
    let f = fixture(3);
    open_parent(&f, "root");
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    f.core.start(PriorOwner::None);
    settle();
    assert!(f.catalog.opened.lock().unwrap().is_empty(), "no restart, no recovery");
}

#[test]
fn a_child_waits_until_its_parent_is_open() {
    let f = fixture(3);
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    f.core.start(previous_host());
    settle();
    assert!(f.catalog.opened.lock().unwrap().is_empty(), "the parent's chat is not open yet");
    open_parent(&f, "root");
    wait_for("the recovery turn once the parent is open", || f.catalog.recoveries("c1") == 1);
}

#[test]
fn only_an_agent_spawned_child_without_a_retry_of_its_own_is_resumed() {
    let f = fixture(3);
    open_parent(&f, "root");
    working_child(&f, "root", "by-user", SpawnOrigin::User);
    working_child(&f, "root", "retrying", SpawnOrigin::Agent);
    working_child(&f, "root", "owed", SpawnOrigin::Agent);
    f.core
        .lineage()
        .retries()
        .arm("retrying", "turn", "window", "Usage limit reached", 1, "h", chrono::Local::now())
        .unwrap();
    f.core.start(previous_host());
    wait_for("the owed recovery turn", || f.catalog.recoveries("owed") == 1);
    settle();
    let opened = f.catalog.opened.lock().unwrap().clone();
    assert_eq!(opened, ["owed"], "the operator's child gets the note on its next message; the retry resumes its own");
}

#[test]
fn a_chat_that_owes_no_note_is_not_resumed() {
    // Its chat is open and owes nothing: local access given up and won back
    // over this same process cut nothing short, or someone already wrote to
    // it (and the note went with that message).
    let f = fixture(3);
    open_parent(&f, "root");
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    let agent = Arc::new(StubConnection::default());
    f.catalog.agents.lock().unwrap().insert("c1".into(), agent.clone());
    f.registry.register("c1".into(), agent);
    f.core.start(previous_host());
    wait_for("the open attempt", || !f.catalog.opened.lock().unwrap().is_empty());
    settle();
    assert_eq!(f.catalog.recoveries("c1"), 0);
    let key = key_hash(KeyKind::V2, &continuation_id("c1", EPOCH));
    assert_eq!(f.core.lineage().receipts().state("c1", &key).unwrap(), None, "nothing sent, nothing reserved");
}

#[test]
fn a_host_that_gave_the_directory_up_sends_nothing_more() {
    let f = fixture(3);
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    f.core.start(previous_host());
    // Given up and won back before the recovery woke: the old one must not
    // carry on beside whatever the new ownership decides (here nothing: the
    // child was parked by then, not working).
    f.core.release();
    f.core.start(previous_host());
    open_parent(&f, "root");
    settle();
    assert!(f.catalog.opened.lock().unwrap().is_empty(), "the recovery belonged to the ownership given up");
}

#[test]
fn a_child_whose_parent_ended_is_not_resumed() {
    let f = fixture(3);
    working_child(&f, "root", "p", SpawnOrigin::Agent);
    working_child(&f, "p", "c", SpawnOrigin::Agent);
    f.core.lineage().mark_ended("p", false, chrono::Local::now()).unwrap();
    open_parent(&f, "p");
    f.core.start(previous_host());
    settle();
    assert!(f.catalog.opened.lock().unwrap().is_empty());
}

/// A catalog that cannot build any session.
struct Unopenable {
    opened: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SessionCatalog for Unopenable {
    fn dormant(&self) -> Vec<DormantSession> {
        Vec::new()
    }
    fn transcript(&self, _: &str) -> Option<DormantTranscript> {
        None
    }
    fn choices(&self, _: &str) -> Option<DormantChoices> {
        None
    }
    async fn open(&self, session_id: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(session_id.to_string());
        Err("session files unreadable".into())
    }
}

#[test]
fn a_child_that_cannot_be_opened_is_sent_nothing_and_its_key_is_released() {
    let f = fixture(3);
    open_parent(&f, "root");
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    let unopenable = Arc::new(Unopenable { opened: Mutex::default() });
    f.core.set_catalog(unopenable.clone());
    f.core.start(previous_host());
    wait_for("the open attempt", || !unopenable.opened.lock().unwrap().is_empty());
    settle();
    assert_eq!(unopenable.opened.lock().unwrap().as_slice(), ["c1"], "tried once, not retried in a loop");
    let key = key_hash(KeyKind::V2, &continuation_id("c1", EPOCH));
    assert_eq!(
        f.core.lineage().receipts().state("c1", &key).unwrap(),
        None,
        "the pending reservation was forgotten, so a later pass may still send"
    );
}

#[test]
fn the_restart_epoch_is_the_start_time_else_the_pid_and_none_without_a_previous_host() {
    let record = |pid, started_at_unix_s| HolderRecord { pid, started_at_unix_s, boot_time_unix_s: None, role: None };
    assert_eq!(restart_epoch(&PriorOwner::None), None);
    assert_eq!(restart_epoch(&previous_host()), Some(EPOCH));
    let no_start = PriorOwner::Gone { record: record(4242, None), same_boot: true };
    assert_eq!(restart_epoch(&no_start), Some(4242), "a host that recorded no start falls back to its pid");
    let unknown = PriorOwner::Unknown { record: record(7, None) };
    assert_eq!(restart_epoch(&unknown), Some(7));
}

#[test]
fn with_recovery_turns_off_nothing_is_resumed() {
    let f = fixture(0);
    open_parent(&f, "root");
    working_child(&f, "root", "c1", SpawnOrigin::Agent);
    f.core.start(previous_host());
    settle();
    assert!(f.catalog.opened.lock().unwrap().is_empty(), "max_resumes 0 sends none; the note waits for the next message");
}
