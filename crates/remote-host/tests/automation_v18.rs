//! The v18 automation surface: heartbeats, team runs, and the coordination
//! blackboard — gates first, behaviour second.
//!
//! The gate matrix these pin, per the phase's ACL requirement:
//!
//! | verb              | operator | agent (own session) | agent (other session) | read-only device |
//! |-------------------|----------|---------------------|-----------------------|------------------|
//! | CreateHeartbeat   | yes      | yes                 | no                    | no               |
//! | DeleteHeartbeat   | yes      | yes                 | no                    | no               |
//! | TeamRunCreate     | yes      | no (full scope)     | no                    | no               |
//! | TeamReport        | yes      | own role only       | no                    | no               |
//! | StateSet          | yes      | yes (shared board)  | yes (shared board)    | no               |

use std::sync::Arc;

use futures::executor::block_on;
use futures::future::join;
use oximux_agents::coord::CoordStore;
use oximux_agents::schedule::{NewSchedule, Recurrence, ScheduleStore};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::team::{NewTeamRole, TeamRoleStatus, TeamStore};
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{
    CreateHeartbeatReq, HelloReq, RecurrenceWire, StateSetReq, TeamReportReq, TeamRoleStatusWire,
};
use oximux_remote_proto::proto::{PROTOCOL_VERSION, Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

/// One in-memory database shared by every store a host attaches, exactly as a
/// real host shares one file.
fn db() -> oximux_storage::Db {
    oximux_storage::db::open_memory().expect("open in-memory db")
}

/// A host with the automation stores attached and `sess-1`/`sess-2` live.
fn host(db: &oximux_storage::Db) -> (Arc<Dispatcher>, ScheduleStore, TeamStore) {
    let registry = Arc::new(SessionRegistry::new());
    for id in ["sess-1", "sess-2"] {
        registry.register(
            id.into(),
            Arc::new(oximux_agents::thread::StubConnection::default()),
        );
    }
    let schedules = ScheduleStore::new(db.conn());
    let teams = TeamStore::new(db.conn());
    let dispatcher = Arc::new(
        Dispatcher::new(registry, Arc::new(AuthStore::new()))
            .with_schedule_store(Arc::new(schedules.clone()))
            .with_team_store(Arc::new(teams.clone()))
            .with_coord_store(Arc::new(CoordStore::new(db.conn()))),
    );
    (dispatcher, schedules, teams)
}

/// Run one scripted exchange against a local connection at `scope`.
fn talk<F, Fut>(dispatcher: &Arc<Dispatcher>, scope: LocalScope, script: F)
where
    F: FnOnce(oximux_remote_proto::testing::DuplexTransport) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, scope);
    block_on(join(serve, script(client)));
}

fn every_15() -> RecurrenceWire {
    RecurrenceWire::EveryMinutes { minutes: 15 }
}

// ---- heartbeats ----

/// A confined agent arms a wake-up on itself without naming a session — the
/// host reads the target from the credential it proved, which is the only value
/// it could honestly supply.
#[test]
fn an_agent_arms_a_heartbeat_on_its_own_session() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Session("sess-1".into()), |client| async move {
        let reply = call(
            &client,
            Request::CreateHeartbeat(CreateHeartbeatReq {
                session_id: None,
                name: "check ci".into(),
                prompt: "has CI gone green?".into(),
                recurrence: every_15(),
            }),
        )
        .await;
        match reply {
            Response::HeartbeatCreated(h) => {
                assert_eq!(h.session_id, "sess-1", "resolved from the proven scope");
                assert!(h.enabled);
            }
            other => panic!("expected the armed heartbeat, got {other:?}"),
        }
        drop(client);
    });
}

/// The containment property: a confined agent cannot arm a wake-up in someone
/// else's session by naming it.
#[test]
fn an_agent_cannot_arm_a_heartbeat_on_another_session() {
    let db = db();
    let (dispatcher, sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Session("sess-1".into()), |client| async move {
        let reply = call(
            &client,
            Request::CreateHeartbeat(CreateHeartbeatReq {
                session_id: Some("sess-2".into()),
                name: "meddle".into(),
                prompt: "do something over there".into(),
                recurrence: every_15(),
            }),
        )
        .await;
        assert_eq!(reply, Response::Error(RpcError::Unauthorized));
        drop(client);
    });
    assert!(sched.for_session("sess-2").unwrap().is_empty(), "and nothing was written");
}

/// Deleting is authorized against the session the heartbeat actually targets,
/// read from the store — not one the caller names.
#[test]
fn an_agent_cannot_disarm_another_sessions_heartbeat() {
    let db = db();
    let (dispatcher, sched, _teams) = host(&db);
    let theirs = sched
        .create_heartbeat(
            NewSchedule {
                name: "theirs".into(),
                cwd: String::new(),
                prompt: "x".into(),
                agent_id: None,
                recurrence: Recurrence::every_minutes(15).unwrap(),
            },
            "sess-2",
            chrono::Local::now(),
        )
        .expect("create");

    let id = theirs.id.clone();
    talk(&dispatcher, LocalScope::Session("sess-1".into()), |client| async move {
        let reply = call(&client, Request::DeleteHeartbeat { id }).await;
        assert_eq!(reply, Response::Error(RpcError::Unauthorized));
        drop(client);
    });
    assert_eq!(sched.for_session("sess-2").unwrap().len(), 1, "it survived");
}

/// A heartbeat aimed at a session this host does not have is refused, rather
/// than armed to fail once per cadence forever.
#[test]
fn a_heartbeat_on_an_unknown_session_is_refused() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Full, |client| async move {
        let reply = call(
            &client,
            Request::CreateHeartbeat(CreateHeartbeatReq {
                session_id: Some("sess-nope".into()),
                name: "ghost".into(),
                prompt: "x".into(),
                recurrence: every_15(),
            }),
        )
        .await;
        assert_eq!(reply, Response::Error(RpcError::UnknownSession));
        drop(client);
    });
}

/// Heartbeats never appear in the schedule list: `ScheduleWire` has no target
/// field, so a client listing schedules would be shown rows it cannot read
/// correctly.
#[test]
fn heartbeats_stay_out_of_the_schedule_list() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Full, |client| async move {
        call(
            &client,
            Request::CreateHeartbeat(CreateHeartbeatReq {
                session_id: Some("sess-1".into()),
                name: "beat".into(),
                prompt: "x".into(),
                recurrence: every_15(),
            }),
        )
        .await;
        match call(&client, Request::ListSchedules).await {
            Response::Schedules(rows) => assert!(rows.is_empty(), "got {rows:?}"),
            other => panic!("expected the schedule list, got {other:?}"),
        }
        match call(&client, Request::ListHeartbeats { session_id: Some("sess-1".into()) }).await {
            Response::Heartbeats(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected the heartbeat list, got {other:?}"),
        }
        drop(client);
    });
}

// ---- team runs ----

/// Opening a run needs full scope: a run spans sessions, so serving a narrowed
/// caller would hand it the escape hatch the narrowing exists to close.
#[test]
fn a_confined_agent_cannot_open_a_team_run() {
    let db = db();
    let (dispatcher, _sched, teams) = host(&db);
    talk(&dispatcher, LocalScope::Session("sess-1".into()), |client| async move {
        let reply = call(
            &client,
            Request::TeamRunCreate(oximux_remote_proto::messages::TeamRunCreateReq {
                name: "ship".into(),
                cwd: "/work".into(),
                agent_id: None,
                worktree_each: false,
                roles: vec![oximux_remote_proto::messages::TeamRoleSpecWire {
                    name: "backend".into(),
                    prompt: "go".into(),
                }],
            }),
        )
        .await;
        assert_eq!(reply, Response::Error(RpcError::Unauthorized));
        drop(client);
    });
    assert!(teams.list(10).unwrap().is_empty(), "and no run was recorded");
}

/// A role settles its own row from inside its session, and the reply is the
/// board — so the caller learns whether the run converged in one round trip.
#[test]
fn a_role_reports_from_its_own_session_and_sees_the_board() {
    let db = db();
    let (dispatcher, _sched, teams) = host(&db);
    let run = teams
        .create(
            "ship",
            "/work",
            &[
                NewTeamRole {
                    name: "backend".into(),
                    session_id: Some("sess-1".into()),
                    status: TeamRoleStatus::Running,
                    summary: None,
                },
                NewTeamRole {
                    name: "frontend".into(),
                    session_id: Some("sess-2".into()),
                    status: TeamRoleStatus::Running,
                    summary: None,
                },
            ],
            chrono::Local::now(),
        )
        .expect("create");

    let run_id = run.id.clone();
    talk(&dispatcher, LocalScope::Session("sess-1".into()), |client| async move {
        let reply = call(
            &client,
            Request::TeamReport(TeamReportReq {
                run_id: run_id.clone(),
                role: "backend".into(),
                ok: true,
                summary: Some("ported".into()),
            }),
        )
        .await;
        match reply {
            Response::TeamRun(board) => {
                assert!(!board.closed, "frontend is still running");
                let backend = board.roles.iter().find(|r| r.name == "backend").expect("role");
                assert_eq!(backend.status, TeamRoleStatusWire::Done);
                assert_eq!(backend.summary.as_deref(), Some("ported"));
            }
            other => panic!("expected the board, got {other:?}"),
        }
        // A teammate's row is not this agent's to settle.
        let refused = call(
            &client,
            Request::TeamReport(TeamReportReq {
                run_id,
                role: "frontend".into(),
                ok: false,
                summary: None,
            }),
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        drop(client);
    });

    let read = teams.get(&run.id).unwrap().unwrap();
    assert_eq!(read.roles[1].status, TeamRoleStatus::Running, "the teammate is untouched");
}

/// A confined caller reads the run it is IN, and nothing else.
#[test]
fn a_role_reads_its_own_run_but_not_another() {
    let db = db();
    let (dispatcher, _sched, teams) = host(&db);
    let mine = teams
        .create(
            "mine",
            "/work",
            &[NewTeamRole {
                name: "backend".into(),
                session_id: Some("sess-1".into()),
                status: TeamRoleStatus::Running,
                summary: None,
            }],
            chrono::Local::now(),
        )
        .expect("create");
    let theirs = teams
        .create(
            "theirs",
            "/work",
            &[NewTeamRole {
                name: "backend".into(),
                session_id: Some("sess-2".into()),
                status: TeamRoleStatus::Running,
                summary: None,
            }],
            chrono::Local::now(),
        )
        .expect("create");

    let (mine_id, theirs_id) = (mine.id.clone(), theirs.id.clone());
    talk(&dispatcher, LocalScope::Session("sess-1".into()), |client| async move {
        match call(&client, Request::TeamStatus { run_id: mine_id }).await {
            Response::TeamRun(board) => assert_eq!(board.name, "mine"),
            other => panic!("expected its own board, got {other:?}"),
        }
        let refused = call(&client, Request::TeamStatus { run_id: theirs_id }).await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        // And the host-wide listing is full-scope only.
        assert_eq!(
            call(&client, Request::TeamList).await,
            Response::Error(RpcError::Unauthorized)
        );
        drop(client);
    });
}

// ---- coordination state ----

/// The blackboard's whole point: a losing conditional write is told it lost,
/// and handed the value it lost to.
#[test]
fn a_stale_conditional_write_answers_with_the_current_entry() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Full, |client| async move {
        let first = call(
            &client,
            Request::StateSet(StateSetReq {
                key: "claim".into(),
                value_json: "\"backend\"".into(),
                if_version: Some(0),
            }),
        )
        .await;
        match first {
            Response::StateValue(Some(entry)) => assert_eq!(entry.version, 1),
            other => panic!("expected the written entry, got {other:?}"),
        }
        // A second create-only write must lose, and say what it lost to.
        let conflict = call(
            &client,
            Request::StateSet(StateSetReq {
                key: "claim".into(),
                value_json: "\"frontend\"".into(),
                if_version: Some(0),
            }),
        )
        .await;
        match conflict {
            Response::StateConflict(Some(current)) => {
                assert_eq!(current.version, 1);
                assert_eq!(current.value_json, "\"backend\"", "the winner is handed back");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        drop(client);
    });
}

/// A conflict is distinguishable from a success even when the losing write was
/// trying to store exactly what the winner stored — the case a shared
/// `StateValue` reply could not express.
#[test]
fn a_conflict_is_distinguishable_even_when_the_values_match() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Full, |client| async move {
        call(
            &client,
            Request::StateSet(StateSetReq {
                key: "k".into(),
                value_json: "1".into(),
                if_version: Some(0),
            }),
        )
        .await;
        let reply = call(
            &client,
            Request::StateSet(StateSetReq {
                key: "k".into(),
                value_json: "1".into(),
                if_version: Some(0),
            }),
        )
        .await;
        assert!(
            matches!(reply, Response::StateConflict(_)),
            "same value, still a refusal: {reply:?}"
        );
        drop(client);
    });
}

/// A value that is not JSON is refused before it reaches the store, so a
/// watcher decoding the board never has to defend against prose.
#[test]
fn a_non_json_value_is_refused() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Full, |client| async move {
        let reply = call(
            &client,
            Request::StateSet(StateSetReq {
                key: "k".into(),
                value_json: "not json".into(),
                if_version: None,
            }),
        )
        .await;
        assert!(matches!(reply, Response::Error(RpcError::BadRequest(_))), "got {reply:?}");
        drop(client);
    });
}

/// A watcher gets the baseline, then every later change, narrowed to its
/// prefix. The v18 gate on the push is exercised implicitly: this peer declares
/// the current version.
#[test]
fn a_watcher_gets_the_baseline_then_prefixed_changes() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    // Seed a key so the baseline is non-empty.
    CoordStore::new(db.conn())
        .set("team/a", "1", None, chrono::Local::now())
        .unwrap()
        .unwrap();

    talk(&dispatcher, LocalScope::Full, |client| async move {
        call(&client, Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION })).await;
        match call(&client, Request::StateWatch { prefix: Some("team/".into()) }).await {
            Response::StateSnapshot(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].key, "team/a");
            }
            other => panic!("expected the baseline, got {other:?}"),
        }
        // A write outside the prefix must not reach this watcher; one inside it
        // must. Both are issued, then frames are read until the first push —
        // the assertion is that it names `team/b`, so a leaked `other/x` fails
        // here rather than being skipped over.
        for (key, value) in [("other/x", "1"), ("team/b", "2")] {
            client
                .send(
                    Request::StateSet(StateSetReq {
                        key: key.into(),
                        value_json: value.into(),
                        if_version: None,
                    })
                    .to_bytes()
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        // Replies and pushes share the transport and their interleaving is not
        // ordered, so skip the two write replies rather than assuming they come
        // first. Bounded so a missing push fails the test instead of hanging.
        let mut pushed = None;
        for _ in 0..4 {
            let frame = client.recv().await.unwrap().expect("a frame");
            if let Response::StateChanged { key, entry } = Response::from_bytes(&frame).unwrap() {
                pushed = Some((key, entry));
                break;
            }
        }
        let (key, entry) = pushed.expect("a pushed change");
        assert_eq!(key, "team/b", "the out-of-prefix write was filtered out");
        assert_eq!(entry.expect("an entry").value_json, "2");
        drop(client);
    });
}

/// The read-only tier, across every v18 write. A device that may only watch
/// must not arm a wake-up, open a run, settle a role, or edit the board — the
/// last of these especially, since editing what agents coordinate on is a way
/// to steer them without ever sending a prompt.
#[test]
fn a_read_only_device_is_refused_every_v18_write() {
    use oximux_remote_host::{PairingSlot, registration_proof};
    use oximux_remote_proto::messages::{RegisterReq, TeamRoleSpecWire, TeamRunCreateReq};

    const NOW: u64 = 1_754_000_000;
    const SECRET: [u8; 16] = [0x22; 16];
    fn clock() -> u64 {
        NOW
    }

    let db = db();
    let registry = Arc::new(SessionRegistry::new());
    registry.register(
        "sess-1".into(),
        Arc::new(oximux_agents::thread::StubConnection::default()),
    );
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(
        Dispatcher::new(registry, auth.clone())
            .with_clock(clock)
            .with_schedule_store(Arc::new(ScheduleStore::new(db.conn())))
            .with_team_store(Arc::new(TeamStore::new(db.conn())))
            .with_coord_store(Arc::new(CoordStore::new(db.conn()))),
    );
    // A byte pattern that is a structurally valid Ed25519 point — `register`
    // rejects anything else outright, and not every fill value decodes.
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let registered = call(
            &client,
            Request::Register(RegisterReq {
                app_pubkey: pubkey,
                device_name: "phone".into(),
                proof: registration_proof(&SECRET, &pubkey, NOW),
                timestamp_secs: NOW,
                session_id: None,
            }),
        )
        .await;
        let Response::Registered { .. } = registered else {
            panic!("expected Registered, got {registered:?}");
        };
        auth.set_read_only(&pubkey, true);

        for write in [
            Request::CreateHeartbeat(CreateHeartbeatReq {
                session_id: Some("sess-1".into()),
                name: "beat".into(),
                prompt: "x".into(),
                recurrence: every_15(),
            }),
            Request::DeleteHeartbeat { id: "sch-whatever".into() },
            Request::TeamRunCreate(TeamRunCreateReq {
                name: "ship".into(),
                cwd: "/work".into(),
                agent_id: None,
                worktree_each: false,
                roles: vec![TeamRoleSpecWire { name: "backend".into(), prompt: "go".into() }],
            }),
            Request::TeamReport(TeamReportReq {
                run_id: "run-x".into(),
                role: "backend".into(),
                ok: true,
                summary: None,
            }),
            Request::StateSet(StateSetReq {
                key: "k".into(),
                value_json: "1".into(),
                if_version: None,
            }),
            Request::StateDelete { key: "k".into() },
        ] {
            let label = format!("{write:?}");
            assert_eq!(
                call(&client, write).await,
                Response::Error(RpcError::Unauthorized),
                "a read-only device must not perform {label}",
            );
        }

        // …while reading the board still works: watching is what the tier is for.
        assert!(matches!(
            call(&client, Request::StateGet { key: "k".into() }).await,
            Response::StateValue(_)
        ));
        drop(client);
    };
    block_on(join(serve, script));
}

/// A pre-v18 peer never receives the push, even though nothing else stops one
/// reaching it: an older decoder meeting the unknown ordinal drops the whole
/// connection.
#[test]
fn an_older_peer_is_never_pushed_a_state_change() {
    let db = db();
    let (dispatcher, _sched, _teams) = host(&db);
    talk(&dispatcher, LocalScope::Full, |client| async move {
        call(&client, Request::Hello(HelloReq { protocol_version: 17 })).await;
        assert!(matches!(
            call(&client, Request::StateWatch { prefix: None }).await,
            Response::StateSnapshot(_)
        ));
        call(
            &client,
            Request::StateSet(StateSetReq {
                key: "k".into(),
                value_json: "1".into(),
                if_version: None,
            }),
        )
        .await;
        // The reply to the write is the LAST frame: a push would have arrived
        // before this `StateDelete`'s reply if the gate were missing.
        let after = call(&client, Request::StateDelete { key: "k".into() }).await;
        assert_eq!(after, Response::Ack, "no push interleaved");
        drop(client);
    });
}
