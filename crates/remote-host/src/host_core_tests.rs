//! HostCore tests: the always-on subscriber, boot reconcile, spawn allowance.

use std::time::{Duration, Instant};

use oximux_agents::lineage::{
    NotifyParent, Reservation, SpawnCaps, SpawnOrigin, SpawnParent, SpawnSpec,
};
use oximux_agents::thread::connection::StubConnection;

use super::*;

struct Fixture {
    core: Arc<HostCore>,
    registry: Arc<SessionRegistry>,
    // Owns the runtime the core was handed; dropped last.
    _rt: tokio::runtime::Runtime,
}

fn fixture(config: HostConfig) -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let db = oximux_storage::db::open_memory().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    // Built from a plain thread with no runtime entered, as the desktop's GPUI
    // main thread does: the core must not need `Handle::try_current`.
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config,
    });
    Fixture { core, registry, _rt: rt }
}

fn spec(request: &str, posture: SpawnPosture) -> SpawnSpec {
    SpawnSpec {
        request_id: request.into(),
        origin: SpawnOrigin::Agent,
        posture,
        notify_parent: NotifyParent::Auto,
        allow_parent_permits: false,
        preassigned_id: None,
        fingerprint_input: request.into(),
    }
}

fn spawn_child(f: &Fixture, parent: &str, child: &str, posture: SpawnPosture) {
    let caps = SpawnCaps { depth: 2, ..SpawnCaps::default() };
    let r = f
        .core
        .lineage()
        .reserve(&SpawnParent::Session(parent.into()), &spec(child, posture), caps, &f.core.working(), settled_ago())
        .unwrap();
    let Ok(Reservation::Fresh { row_id, .. }) = r else { panic!("{r:?}") };
    f.core.lineage().finalize(row_id, child, settled_ago()).unwrap();
}

/// A start time past the grace a freshly active row gets, so these tests can
/// watch idle reports park a child.
fn settled_ago() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now() - chrono::Duration::seconds(oximux_agents::lineage::ACTIVE_GRACE_S + 5)
}

fn register(f: &Fixture, id: &str) -> Arc<oximux_agents::session_registry::SessionHandle> {
    f.registry.register(id.to_string(), Arc::new(StubConnection::default()))
}

fn wait_for_state(f: &Fixture, session: &str, want: LineageState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = f.core.lineage().row(session).unwrap().unwrap().state;
        if state == want {
            return;
        }
        assert!(Instant::now() < deadline, "{session} stuck at {state:?}, wanted {want:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_subscriber_parks_an_idle_child_and_revives_a_working_one() {
    let f = fixture(HostConfig::default());
    f.core.start(PriorOwner::None);
    register(&f, "parent");
    let child = register(&f, "child");
    child.set_working(true);
    spawn_child(&f, "parent", "child", SpawnPosture::Default);
    // The turn ends: the driver says idle, the row parks.
    child.set_working(false);
    wait_for_state(&f, "child", LineageState::Parked);
    // A new turn: the row revives without a cap check.
    child.set_working(true);
    wait_for_state(&f, "child", LineageState::Active);
}

#[test]
fn an_unregistered_child_parks_and_is_never_ended() {
    let f = fixture(HostConfig::default());
    f.core.start(PriorOwner::None);
    register(&f, "parent");
    let child = register(&f, "child");
    child.set_working(true);
    spawn_child(&f, "parent", "child", SpawnPosture::Default);
    f.registry.unregister("child");
    f.core.sync_lineage_states();
    let row = f.core.lineage().row("child").unwrap().unwrap();
    assert_eq!(row.state, LineageState::Parked, "a closed tab is not working");
    assert_eq!(row.ended_at, None, "and closing it never ends the row");
}

#[test]
fn a_guest_writes_nothing_until_it_owns_the_data_dir() {
    let f = fixture(HostConfig::default());
    register(&f, "parent");
    let child = register(&f, "child");
    child.set_working(true);
    spawn_child(&f, "parent", "child", SpawnPosture::Default);
    child.set_working(false);
    f.core.sync_lineage_states();
    assert_eq!(f.core.lineage().row("child").unwrap().unwrap().state, LineageState::Active, "a guest parks nothing");
    assert!(!f.core.spawn_allowance("parent").allowed, "a guest offers no spawning");
    f.core.start(PriorOwner::None);
    assert_eq!(f.core.lineage().row("child").unwrap().unwrap().state, LineageState::Parked);
    assert!(f.core.spawn_allowance("parent").allowed);
    // Giving the dir up turns writes off again; winning it back reconciles.
    f.core.release();
    assert!(!f.core.is_owner());
    child.set_working(true);
    f.core.sync_lineage_states();
    assert_eq!(f.core.lineage().row("child").unwrap().unwrap().state, LineageState::Parked);
    f.core.start(PriorOwner::None);
    assert_eq!(f.core.lineage().row("child").unwrap().unwrap().state, LineageState::Active, "re-won: corrected");
}

#[test]
fn an_open_permission_prompt_counts_as_working() {
    let f = fixture(HostConfig::default());
    assert!(!f.core.working().is_working("nobody"), "unregistered is idle");
    let h = register(&f, "s");
    assert!(!f.core.working().is_working("s"));
    h.set_working(true);
    assert!(f.core.working().is_working("s"));
}

#[test]
fn start_reconciles_once_and_records_the_prior_owner() {
    let f = fixture(HostConfig::default());
    register(&f, "parent");
    spawn_child(&f, "parent", "child", SpawnPosture::Default);
    assert_eq!(f.core.prior_owner(), None, "not the host until start");
    f.core.start(PriorOwner::None);
    assert_eq!(f.core.lineage().row("child").unwrap().unwrap().state, LineageState::Parked);
    assert_eq!(f.core.prior_owner(), Some(PriorOwner::None));
    // A second start while still the owner does not reconcile again.
    f.core.lineage().activate("child", chrono::Local::now()).unwrap();
    f.core.start(PriorOwner::None);
    assert_eq!(f.core.lineage().row("child").unwrap().unwrap().state, LineageState::Active);
}

#[test]
fn spawn_allowance_follows_delegation_registration_posture_and_depth() {
    let f = fixture(HostConfig::default());
    f.core.start(PriorOwner::None);
    register(&f, "root");
    let a = f.core.spawn_allowance("root");
    assert!(a.allowed, "{a:?}");
    assert_eq!((a.depth, a.max_depth), (0, 1));

    // A cockpit agent is not in the registry.
    let cockpit = f.core.spawn_allowance("cockpit-agent");
    assert!(!cockpit.allowed);
    assert!(cockpit.reason.unwrap().contains("chat agent"));

    // A depth-1 child at the default depth cap gets no spawn tool.
    register(&f, "child");
    spawn_child(&f, "root", "child", SpawnPosture::Default);
    let child = f.core.spawn_allowance("child");
    assert!(!child.allowed);
    assert_eq!(child.depth, 1);
    assert!(child.reason.unwrap().ends_with("Complete this task yourself."));

    // With depth 2 it may, unless it is a reviewer.
    f.core.set_config(HostConfig { caps: SpawnCaps { depth: 2, ..SpawnCaps::default() }, ..HostConfig::default() });
    assert!(f.core.spawn_allowance("child").allowed);
    register(&f, "reviewer");
    spawn_child(&f, "root", "reviewer", SpawnPosture::Review);
    let rev = f.core.spawn_allowance("reviewer");
    assert!(!rev.allowed);
    assert_eq!(rev.reason.as_deref(), Some("Review agents do not spawn; report findings instead."));

    f.core.set_config(HostConfig { delegation_enabled: false, ..HostConfig::default() });
    assert!(!f.core.spawn_allowance("root").allowed);
}

#[test]
fn a_stored_depth_above_the_ceiling_is_clamped_on_set() {
    let f = fixture(HostConfig { caps: SpawnCaps { depth: 9, ..SpawnCaps::default() }, ..HostConfig::default() });
    assert_eq!(f.core.config().caps.depth, 2);
}

#[test]
fn prior_owner_classifies_the_previous_record() {
    assert_eq!(PriorOwner::from_previous(None), PriorOwner::None);
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("host.lock");
    drop(oximux_single_instance::try_acquire_as(&lock, "serve").unwrap());
    let rec = oximux_single_instance::read_holder_record(&lock).unwrap();
    assert!(matches!(PriorOwner::from_previous(Some(&rec)), PriorOwner::Gone { same_boot: true, .. }));
    let mut rebooted = rec.clone();
    rebooted.boot_time_unix_s = rebooted.boot_time_unix_s.map(|b| b - 3600);
    assert!(matches!(PriorOwner::from_previous(Some(&rebooted)), PriorOwner::Gone { same_boot: false, .. }));
    let mut unknown = rec;
    unknown.boot_time_unix_s = None;
    assert!(matches!(PriorOwner::from_previous(Some(&unknown)), PriorOwner::Unknown { .. }));
}
