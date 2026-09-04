//! The session-control RPCs (v7): listing a backend's model/mode catalog, and
//! switching between them.

use std::sync::Arc;

use futures::StreamExt;
use oximux_agents::session_registry::{ChoiceKind, SessionMeta, SessionRegistry};
use oximux_agents::thread::{ModeChoice, ModelChoice, StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::messages::RegisterReq;
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

fn model(wire: &str, label: &str) -> ModelChoice {
    ModelChoice { wire: wire.into(), label: label.into(), description: None }
}

/// A switchable (ACP-like) backend: offers a catalog and accepts changes live.
fn switchable_registry() -> (Arc<SessionRegistry>, Arc<StubConnection>) {
    let conn = Arc::new(StubConnection::default().with_switchable(
        vec![model("opus-5", "Opus 5"), model("sonnet-5", "Sonnet 5")],
        vec![ModeChoice { wire: "plan".into(), label: "Plan".into() }],
    ));
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), conn.clone());
    handle.set_meta(SessionMeta {
        model: Some("opus-5".into()),
        permission_mode: Some("plan".into()),
        ..Default::default()
    });
    (registry, conn)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_lists_the_catalog_and_switches_model() {
    let (registry, conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let choices = call(&client, Request::ListChoices { session_id: "sess-1".into() }).await;
        let Response::Choices(choices) = choices else {
            panic!("expected Choices, got {choices:?}");
        };
        assert_eq!(
            choices.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["opus-5", "sonnet-5"],
        );
        assert_eq!(choices.models[0].label, "Opus 5", "the label is what a person reads");
        assert_eq!(choices.modes.len(), 1);
        assert_eq!(
            choices.current_model.as_deref(),
            Some("opus-5"),
            "the picker marks the active model without a second round trip",
        );
        assert_eq!(
            choices.current_mode.as_deref(),
            Some("plan"),
            "and the mode it is running, or its chip can only ever read 'Mode'",
        );

        let set = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        assert_eq!(set, Response::Ack);

        drop(client);
    };
    futures::future::join(serve, script).await;

    // Asserted at the backend, not on the wire: a reply that acknowledged while
    // dropping the change would look identical from the client's side.
    let sent = conn.sent();
    assert!(
        sent.iter().any(|v| v["type"] == "set_model" && v["model"] == "sonnet-5"),
        "the switch reached the backend, saw {sent:?}",
    );
}

/// An id the session does not offer is refused, and never reaches the backend.
///
/// Backends accept an unrecognised pick silently — the setter returns `Ok`,
/// nothing changes — so this used to answer `Ack`:
/// `oximux mode set <session> nonsense` printed "mode set to nonsense" and
/// exited 0 while the session stayed on its default. Worse than a plain error
/// for a scripted run, because the next turn then behaves as the OLD value
/// dictates — parking on a permission request nobody is there to answer.
///
/// The backend assertion is the load-bearing half: a reply that refused on the
/// wire while still forwarding the change would look identical from the client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_id_the_session_does_not_offer_is_refused_and_never_forwarded() {
    let (registry, conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        // The catalog is opus-5/sonnet-5 and the single mode `plan`.
        for (what, req, offered) in [
            (
                "model",
                Request::SetModel { session_id: "sess-1".into(), model: "gpt-9".into() },
                "opus-5, sonnet-5",
            ),
            (
                "permission mode",
                Request::SetPermissionMode {
                    session_id: "sess-1".into(),
                    mode: "acceptEdit".into(), // the real id is `plan`
                },
                "plan",
            ),
        ] {
            let Response::Error(RpcError::BadRequest(msg)) = call(&client, req).await else {
                panic!("an unoffered {what} must be refused, not acknowledged");
            };
            assert!(
                msg.contains(offered),
                "the refusal names what the session does offer, got: {msg}",
            );
        }

        drop(client);
    };
    futures::future::join(serve, script).await;

    let sent = conn.sent();
    assert!(
        !sent.iter().any(|v| v["type"] == "set_model" || v["type"] == "set_mode"),
        "a refused pick must not reach the backend, saw {sent:?}",
    );
}

/// A backend that has not advertised its catalog yet still accepts a switch.
///
/// The guard above validates against the offered list, and an EMPTY list is not
/// evidence of a bad id — a dynamic-catalog backend advertises only once its
/// handshake completes, and a session can be switched before then. Rejecting on
/// an empty list would refuse correct calls on timing alone, which is the
/// obvious wrong way to implement that check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_with_no_catalog_yet_is_not_second_guessed() {
    // Switchable, but advertising nothing — the pre-handshake state.
    let conn = Arc::new(StubConnection::default().with_switchable(vec![], vec![]));
    let registry = Arc::new(SessionRegistry::new());
    registry.register("sess-1".into(), conn.clone());
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(
                &client,
                Request::SetModel { session_id: "sess-1".into(), model: "anything".into() },
            )
            .await,
            Response::Ack,
            "an empty catalog means nothing to check against, not a bad id",
        );
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert!(
        conn.sent().iter().any(|v| v["type"] == "set_model"),
        "and the change still reaches the backend",
    );
}

/// A fix-at-spawn backend with no desktop view attached must fail loudly.
///
/// The recovery for such a backend is a respawn, which only a bound view can
/// perform. With none attached there is nothing that could carry the change out,
/// so the host says so — a picker that silently did nothing would be worse than
/// one that explains itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fix_at_spawn_backend_with_no_view_refuses_with_an_explanation() {
    let registry = Arc::new(SessionRegistry::new());
    // A default stub is deliberately NOT switchable — it matches the trait
    // default and the real fix-at-spawn backends.
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        // An empty catalog is a legitimate answer, not an error.
        let choices = call(&client, Request::ListChoices { session_id: "sess-1".into() }).await;
        let Response::Choices(choices) = choices else {
            panic!("expected Choices, got {choices:?}");
        };
        assert!(choices.models.is_empty(), "no catalog is an answer, not a failure");

        let set = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        match set {
            Response::Error(RpcError::BadRequest(msg)) => {
                assert!(msg.contains("model"), "the message names what failed: {msg}");
                // The underlying error text is logged host-side only. Forwarding
                // it would repeat the leak the git handlers had to fix, where raw
                // tool output carried host paths to the client.
                assert!(
                    !msg.contains("does not support"),
                    "the backend's own wording is not forwarded: {msg}",
                );
            }
            other => panic!("expected a BadRequest explaining the refusal, got {other:?}"),
        }

        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A fix-at-spawn backend switches anyway when a desktop view is attached.
///
/// This is the case that matters: Claude and Codex take `--model` at spawn and
/// refuse the in-session setter, and the desktop's own picker has always
/// recovered by respawning the child resumed on the new pick. Without this the
/// phone offered a catalog it could never apply against the two most common
/// backends, while the identical desktop control worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fix_at_spawn_backend_switches_through_the_desktop_view() {
    let registry = Arc::new(SessionRegistry::new());
    // Not switchable — the in-session setter fails, as on the real backends.
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));

    // Stand in for the bound desktop view: take the relayed change and confirm it.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    handle.set_remote_choice_sink(tx);
    let view = tokio::spawn(async move {
        let change = rx.next().await.expect("the change reaches the view");
        assert_eq!(change.kind, ChoiceKind::Model);
        assert_eq!(change.value, "sonnet-5", "the view is told which model to respawn on");
        let _ = change.reply.send(true);
    });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let set = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        assert_eq!(set, Response::Ack, "the view completed the change, so it succeeded");

        drop(client);
    };
    futures::future::join(serve, script).await;
    view.await.expect("the view task saw the change");
}

/// A mode change goes through the view even on a backend that could take it
/// directly.
///
/// Claude switches permission mode in place, so setting the connection here would
/// "work" — the agent would obey — while the desktop tab went on holding the old
/// mode: the value it shows in its own picker, and the one it would respawn with.
/// The phone would then re-read that stale mode and redraw the chip it started
/// with, which reads as a control that does nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switchable_backend_still_changes_mode_through_the_desktop_view() {
    let (registry, conn) = switchable_registry();
    let handle = registry.get("sess-1").expect("the session is registered");

    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    handle.set_remote_choice_sink(tx);
    let view = tokio::spawn(async move {
        let change = rx.next().await.expect("the change reaches the view");
        assert_eq!(change.kind, ChoiceKind::PermissionMode);
        assert_eq!(change.value, "plan");
        let _ = change.reply.send(true);
    });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let set = call(
            &client,
            Request::SetPermissionMode { session_id: "sess-1".into(), mode: "plan".into() },
        )
        .await;
        assert_eq!(set, Response::Ack);

        drop(client);
    };
    futures::future::join(serve, script).await;
    view.await.expect("the view task saw the change");

    assert!(
        conn.sent().is_empty(),
        "the view owns the switch; setting the backend behind it desyncs the two, saw {:?}",
        conn.sent(),
    );
}

/// The view being attached is not the same as the change landing.
///
/// A respawn can fail (the tab closed mid-change, the agent would not start).
/// Reporting success then would leave the phone showing a model the session is
/// not running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_view_that_cannot_apply_the_change_is_reported_as_a_failure() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));

    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    handle.set_remote_choice_sink(tx);
    let view = tokio::spawn(async move {
        let change = rx.next().await.expect("the change reaches the view");
        // Dropped rather than answered — the view went away mid-change. This must
        // resolve as a failure rather than hanging the RPC forever.
        drop(change.reply);
    });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let set = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        assert!(
            matches!(set, Response::Error(RpcError::BadRequest(_))),
            "an unapplied change is a failure, got {set:?}",
        );

        drop(client);
    };
    futures::future::join(serve, script).await;
    view.await.expect("the view task saw the change");
}

/// Reading the catalog is a read; changing it is a write.
///
/// A read-only device should still see which model is running — it simply cannot
/// change it. Gating the list on `may_write` too would hide information the tier
/// was never meant to withhold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_sees_the_catalog_but_cannot_switch() {
    let (registry, conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(registry, Arc::clone(&auth)).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        // Down-scoped after pairing, mirroring the real flow.
        auth.set_read_only(&pubkey, true);

        let choices = call(&client, Request::ListChoices { session_id: "sess-1".into() }).await;
        assert!(matches!(choices, Response::Choices(_)), "reads stay allowed: {choices:?}");

        let set_model = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        assert_eq!(set_model, Response::Error(RpcError::Unauthorized));

        let set_mode = call(
            &client,
            Request::SetPermissionMode { session_id: "sess-1".into(), mode: "plan".into() },
        )
        .await;
        assert_eq!(set_mode, Response::Error(RpcError::Unauthorized));

        drop(client);
    };
    futures::future::join(serve, script).await;

    // The refusal is checked at the backend, not just on the wire — an
    // Unauthorized reply that still applied the change would look identical.
    assert!(
        conn.sent().is_empty(),
        "nothing reached the backend, saw {:?}",
        conn.sent(),
    );
}

/// An unknown session is distinguishable from an unauthorized one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_is_reported_as_such() {
    let (registry, _conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let choices = call(&client, Request::ListChoices { session_id: "nope".into() }).await;
        assert_eq!(choices, Response::Error(RpcError::UnknownSession));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A launcher whose behaviour the test scripts, recording what it was asked for.
struct ScriptedLauncher {
    calls: std::sync::Mutex<Vec<(String, Option<String>)>>,
    result: Result<String, ()>,
}

#[async_trait::async_trait]
impl oximux_remote_host::SessionLauncher for ScriptedLauncher {
    async fn create(
        &self,
        cwd: &str,
        agent_id: Option<&str>,
        _model: Option<&str>,
    ) -> Result<String, oximux_remote_host::LaunchError> {
        self.calls.lock().unwrap().push((cwd.to_string(), agent_id.map(str::to_string)));
        self.result
            .clone()
            .map_err(|_| oximux_remote_host::LaunchError::BadWorkingDirectory)
    }
}

fn launcher(result: Result<String, ()>) -> Arc<ScriptedLauncher> {
    Arc::new(ScriptedLauncher { calls: std::sync::Mutex::new(Vec::new()), result })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_access_device_starts_a_session_and_gets_its_id() {
    let launcher = launcher(Ok("sess-new".into()));
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_launcher(launcher.clone());

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let created = call(
            &client,
            Request::CreateSession { cwd: "/work/proj".into(), agent_id: Some("claude".into()) },
        )
        .await;
        // The id comes back so the client can subscribe immediately, rather than
        // re-listing and guessing which row is new.
        assert_eq!(created, Response::SessionCreated { session_id: "sess-new".into() });
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert_eq!(
        launcher.calls.lock().unwrap().as_slice(),
        &[("/work/proj".to_string(), Some("claude".to_string()))],
        "cwd and agent reached the launcher unchanged",
    );
}

/// A session-scoped device must not be able to create its way out of its scope.
///
/// This is the gate that does not follow from `may_write`: creating a session has
/// no session id to narrow against, so the ordinary write check would wave a
/// narrowed device straight through — and the session it created would be
/// outside the confinement the desktop user chose, making the narrowing
/// decorative.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_scoped_device_cannot_create_a_session() {
    let launcher = launcher(Ok("sess-new".into()));
    let auth = Arc::new(AuthStore::new());
    // Paired against one session only — the narrowed tier.
    auth.set_pairing(PairingSlot::new(SECRET, Some("sess-1".into()), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_launcher(launcher.clone());

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let mut req = register_req([0x33; 32]);
        req.session_id = Some("sess-1".into());
        let Response::Registered { .. } = call(&client, Request::Register(req)).await else {
            panic!("expected Registered");
        };
        let created = call(
            &client,
            Request::CreateSession { cwd: "/work".into(), agent_id: None },
        )
        .await;
        assert_eq!(created, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;

    // Checked at the launcher, not on the wire: an Unauthorized reply that still
    // spawned a process would look identical from the client's side.
    assert!(
        launcher.calls.lock().unwrap().is_empty(),
        "nothing reached the launcher, saw {:?}",
        launcher.calls.lock().unwrap(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_cannot_create_a_session() {
    let launcher = launcher(Ok("sess-new".into()));
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_launcher(launcher.clone());

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        auth.set_read_only(&pubkey, true);
        let created =
            call(&client, Request::CreateSession { cwd: "/work".into(), agent_id: None }).await;
        assert_eq!(created, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
    assert!(launcher.calls.lock().unwrap().is_empty(), "nothing spawned");
}

/// A project provider whose list the test controls.
struct ScriptedProjects(Vec<oximux_remote_proto::ProjectSummaryWire>);

#[async_trait::async_trait]
impl oximux_remote_host::ProjectProvider for ScriptedProjects {
    async fn projects(&self) -> Vec<oximux_remote_proto::ProjectSummaryWire> {
        self.0.clone()
    }
}

fn projects(
    rows: Vec<oximux_remote_proto::ProjectSummaryWire>,
) -> Arc<ScriptedProjects> {
    Arc::new(ScriptedProjects(rows))
}

/// A device that may create sessions gets the provider's project list verbatim —
/// the quick-start targets the phone offers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_access_device_lists_the_projects() {
    let rows = vec![
        oximux_remote_proto::ProjectSummaryWire {
            name: "OxiMux".into(),
            path: "/Users/me/Code/OxiMux".into(),
        },
        oximux_remote_proto::ProjectSummaryWire { name: "work".into(), path: "/Users/me/work".into() },
    ];
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_projects(projects(rows.clone()));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(call(&client, Request::ListProjects).await, Response::Projects(rows));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// The project list is gated exactly like creating: a session-scoped device may
/// not enumerate the host's projects (it would leak paths it cannot use).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_scoped_device_cannot_list_projects() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, Some("sess-1".into()), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_projects(projects(vec![oximux_remote_proto::ProjectSummaryWire {
            name: "OxiMux".into(),
            path: "/secret".into(),
        }]));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let mut req = register_req([0x33; 32]);
        req.session_id = Some("sess-1".into());
        let Response::Registered { .. } = call(&client, Request::Register(req)).await else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(&client, Request::ListProjects).await,
            Response::Error(RpcError::Unauthorized),
        );
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A host with no project provider answers an empty list — an authorized client
/// sees no quick-start projects rather than an error, since "this desktop exposes
/// none" is not worth hiding (the create path stays gated regardless).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_a_project_provider_lists_nothing() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth).with_clock(clock); // no provider

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(call(&client, Request::ListProjects).await, Response::Projects(vec![]));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A host with no launcher answers `Unauthorized`, not a distinct "unsupported".
///
/// Whether this desktop can start sessions is not something an unauthorized
/// client should be able to probe — the same reasoning the terminal RPCs use for
/// a missing `TerminalSource`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_a_launcher_is_indistinguishable_from_one_that_refuses() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth).with_clock(clock); // no launcher

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let created =
            call(&client, Request::CreateSession { cwd: "/work".into(), agent_id: None }).await;
        assert_eq!(created, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A launch failure reports a category, never the desktop's own error text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_launch_reports_a_category_not_host_detail() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_launcher(launcher(Err(())));

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let created = call(
            &client,
            Request::CreateSession { cwd: "/nope/missing".into(), agent_id: None },
        )
        .await;
        match created {
            Response::Error(RpcError::BadRequest(msg)) => {
                // A launch failure routinely embeds absolute host paths (a
                // missing directory, a binary off PATH). The category crosses;
                // the detail is logged host-side.
                assert!(!msg.contains("/nope/missing"), "no host path leaked: {msg}");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
        drop(client);
    };
    futures::future::join(serve, script).await;
}
