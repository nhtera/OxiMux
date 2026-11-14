//! The settings verbs: who may read, who may change, and what the answer says.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::Request;

use super::*;
use crate::auth::{AuthStore, LocalScope, PairingSlot, registration_proof};
use crate::host_config::{HostConfig, keys};
use crate::host_core::{HostCore, HostCoreDeps};

/// A dispatcher over a core whose settings live in an in-memory database, plus
/// the store so a test can see what was (or was not) written.
struct Fixture {
    dispatcher: Dispatcher,
    core: Arc<HostCore>,
    settings: oximux_storage::SettingsRepo,
    auth: Arc<AuthStore>,
    _rt: tokio::runtime::Runtime,
}

fn fixture() -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let db = oximux_storage::db::open_memory().unwrap();
    let settings = oximux_storage::SettingsRepo::new(db.clone());
    let registry = Arc::new(SessionRegistry::new());
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: oximux_agents::lineage::LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::load(|key| settings.get(key).ok().flatten()),
    });
    core.set_settings(settings.clone());
    let auth = Arc::new(AuthStore::new());
    let dispatcher = Dispatcher::new(registry, auth.clone()).with_host_core(core.clone());
    Fixture { dispatcher, core, settings, auth, _rt: rt }
}

fn operator() -> Peer {
    Peer::local(LocalScope::Full)
}

/// A fully paired, write-capable phone. Paired for real, because an unpaired
/// key is refused by the capability gate and would pass these tests without
/// the operator-only gate existing at all.
fn paired_phone(auth: &AuthStore) -> Peer {
    let secret = [0x22; 16];
    auth.set_pairing(PairingSlot::new(secret, None, false));
    let pubkey = SigningKey::from_bytes(&[0x33; 32]).verifying_key().to_bytes();
    let ts = 1_700_000_000;
    auth.register(
        &RegisterReq {
            app_pubkey: pubkey,
            device_name: "phone".into(),
            proof: registration_proof(&secret, &pubkey, ts),
            timestamp_secs: ts,
            session_id: None,
        },
        ts,
    )
    .expect("register");
    Peer::remote(pubkey)
}

fn entries(response: &Response) -> &[oximux_remote_proto::host_config::HostConfigEntryWire] {
    match response {
        Response::HostConfig(entries) => entries,
        other => panic!("not a settings reply: {other:?}"),
    }
}

/// A write is validated, persisted **and applied**: the point of the verb is
/// that the running host enforces the new number, which a client writing the
/// database behind its back could not achieve.
#[test]
fn a_setting_the_operator_changes_is_what_the_host_then_enforces() {
    let f = fixture();
    assert_eq!(f.core.config().caps.active_per_parent, HostConfig::default().caps.active_per_parent);

    let reply = f.dispatcher.host_config_write(&operator(), keys::CAP_ACTIVE_PER_PARENT, "2");
    let entries = entries(&reply);
    assert_eq!(entries.len(), 1, "one key asked about, one answered");
    assert_eq!(entries[0].key, keys::CAP_ACTIVE_PER_PARENT);
    assert_eq!(entries[0].value, "2");
    assert!(!entries[0].is_default, "it is stored now");
    assert_eq!(f.core.config().caps.active_per_parent, 2, "and the host enforces it");
    assert_eq!(f.settings.get(keys::CAP_ACTIVE_PER_PARENT).unwrap().as_deref(), Some("2"), "and it survives a restart");
}

/// The answer is the value in force, not the one asked for. A cap that
/// silently became something else is how a person ends up believing a limit
/// they do not have.
#[test]
fn a_cap_above_the_ceiling_answers_with_what_it_became() {
    let f = fixture();
    let reply = f.dispatcher.host_config_write(&operator(), keys::CAP_DEPTH, "9");
    assert_eq!(entries(&reply)[0].value, oximux_agents::lineage::MAX_DEPTH.to_string());
    assert_eq!(f.core.config().caps.depth, oximux_agents::lineage::MAX_DEPTH);
}

/// Reading says which values anybody chose, because "set to 8" and "8 because
/// nobody set it" are different answers to the only question being asked.
#[test]
fn a_whole_read_names_every_setting_and_which_are_untouched() {
    let f = fixture();
    f.dispatcher.host_config_write(&operator(), keys::STALL_AFTER_S, "90");

    let reply = f.dispatcher.host_config_read(&operator(), None);
    let entries = entries(&reply);
    assert_eq!(entries.len(), crate::host_config::SETTABLE.len(), "every setting this host has");
    let stall = entries.iter().find(|e| e.key == keys::STALL_AFTER_S).expect("the stall clock");
    assert_eq!((stall.value.as_str(), stall.is_default), ("90", false));
    let depth = entries.iter().find(|e| e.key == keys::CAP_DEPTH).expect("the depth cap");
    assert_eq!(
        (depth.value.as_str(), depth.is_default),
        (HostConfig::default().caps.depth.to_string().as_str(), true),
        "shipped, not chosen",
    );
}

/// A paired phone may see the caps it runs under and may not widen them:
/// turning confinement off would hand the next chat agent the operator's own
/// credential.
#[test]
fn a_paired_device_reads_the_settings_and_never_changes_them() {
    let f = fixture();
    let phone = paired_phone(&f.auth);

    let read = f.dispatcher.host_config_read(&phone, Some(keys::CONFINE_CHAT));
    assert_eq!(entries(&read)[0].key, keys::CONFINE_CHAT, "reading is allowed: {read:?}");

    let write = f.dispatcher.host_config_write(&phone, keys::CONFINE_CHAT, "false");
    assert!(matches!(write, Response::Error(RpcError::Unauthorized)), "{write:?}");
    assert!(f.core.config().confine_chat, "and confinement still holds");
    assert_eq!(f.settings.get(keys::CONFINE_CHAT).unwrap(), None, "the refusal landed before the write");
}

/// An unknown key is the caller's mistake, said as one — and nothing is
/// stored, so a typo cannot leave a row nothing will ever read.
#[test]
fn an_unknown_key_is_refused_and_not_stored() {
    let f = fixture();
    let reply = f.dispatcher.host_config_write(&operator(), "agents.delegation.cap.deptth", "2");
    assert!(matches!(reply, Response::Error(RpcError::BadRequest(_))), "{reply:?}");
    assert_eq!(f.settings.get("agents.delegation.cap.deptth").unwrap(), None);
    let read = f.dispatcher.host_config_read(&operator(), Some("nonsense"));
    assert!(matches!(read, Response::Error(RpcError::BadRequest(_))), "{read:?}");
}

/// A value the loader would ignore is refused instead of stored. Stored text
/// nothing reads comes back as the default, which reads as a setting that was
/// applied — the one failure mode a settings verb must not have.
#[test]
fn a_value_the_loader_would_ignore_is_refused() {
    let f = fixture();
    for (key, value) in [
        (keys::STALL_AFTER_S, "soon"),
        (keys::DELEGATION_ENABLED, "maybe"),
        (keys::VERIFY_ALLOW, "Bash(*)"),
    ] {
        let reply = f.dispatcher.host_config_write(&operator(), key, value);
        assert!(matches!(reply, Response::Error(RpcError::BadRequest(_))), "{key}={value}: {reply:?}");
        assert_eq!(f.settings.get(key).unwrap(), None, "{key} was not stored");
    }
    assert_eq!(f.core.config(), HostConfig::default(), "and nothing was applied");
}

/// A core with no settings store answers rather than pretending: there is
/// nothing to read back, and a write would be invisible.
#[test]
fn a_host_with_no_settings_store_says_so() {
    let f = fixture();
    let bare = HostCore::new(HostCoreDeps {
        rt: f._rt.handle().clone(),
        lineage: oximux_agents::lineage::LineageStore::new(oximux_storage::db::open_memory().unwrap().conn()),
        registry: Arc::new(SessionRegistry::new()),
        config: HostConfig::default(),
    });
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), f.auth.clone()).with_host_core(bare);
    assert!(matches!(
        dispatcher.host_config_read(&operator(), None),
        Response::Error(RpcError::Unsupported)
    ));
    assert!(matches!(
        dispatcher.host_config_write(&operator(), keys::CAP_DEPTH, "2"),
        Response::Error(RpcError::Unsupported)
    ));
}

/// The verbs are reachable through the dispatch table, not only as functions:
/// a handler nothing routes to is a handler that does not exist.
#[test]
fn the_verbs_are_routed() {
    let f = fixture();
    let reply = f.dispatcher.handle_session_rpc(
        &operator(),
        Request::HostConfigWrite { key: keys::MAX_RESUMES.into(), value: "0".into() },
        oximux_remote_proto::proto::PROTOCOL_VERSION,
    );
    assert_eq!(entries(&reply)[0].value, "0", "{reply:?}");
    let reply = f.dispatcher.handle_session_rpc(
        &operator(),
        Request::HostConfigRead { key: Some(keys::MAX_RESUMES.into()) },
        oximux_remote_proto::proto::PROTOCOL_VERSION,
    );
    assert_eq!(entries(&reply)[0].value, "0", "{reply:?}");
}
