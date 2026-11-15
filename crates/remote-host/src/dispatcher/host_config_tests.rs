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

/// How long a test waits for a parked writer to announce it got to the gate.
/// Generous: it is a deadlock guard, not a timing assumption — the signal
/// arrives as soon as the writer is scheduled, and the test does not sleep.
const GATE: std::time::Duration = std::time::Duration::from_secs(10);

/// A dispatcher over a core whose settings live in an in-memory database, plus
/// the store so a test can see what was (or was not) written.
struct Fixture {
    dispatcher: Dispatcher,
    core: Arc<HostCore>,
    db: oximux_storage::db::Db,
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
    Fixture { dispatcher, core, db, settings, auth, _rt: rt }
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

/// A second writer waits for the first, and the store and the host agree
/// afterwards.
///
/// Each connection serves on its own task of a multi-threaded runtime, so two
/// writers are two operators (or one, twice over). The sequence is write →
/// read every key → apply, and unserialized it interleaves: A writes, B writes
/// and applies, A applies the config it read before B's write. The store then
/// says one thing and the host enforces another until the next write.
///
/// Staged rather than stressed: a loop of concurrent writers passes against
/// the unserialized code, because the store's own lock makes the bad
/// interleaving rare. So the first writer is parked in the one window that
/// matters and the second is watched not entering it.
#[test]
fn a_second_writer_waits_for_the_first() {
    let f = fixture();
    let (release, parked) = std::sync::mpsc::channel();
    let (arrived, at_gate) = std::sync::mpsc::channel();
    f.core.set_config_gate(arrived, parked);

    let first = {
        let core = f.core.clone();
        std::thread::spawn(move || core.set_config_value(keys::STALL_AFTER_S, "900"))
    };
    // The gate says so: the first writer is past its store write, inside the
    // window and holding the lock. Waited for rather than slept past, so the
    // second writer cannot be the one that gets there first.
    at_gate.recv_timeout(GATE).expect("the first writer reached the window");
    assert_eq!(f.settings.get(keys::STALL_AFTER_S).unwrap().as_deref(), Some("900"), "its value is stored");

    let second = {
        let core = f.core.clone();
        std::thread::spawn(move || core.set_config_value(keys::CAP_ACTIVE_PER_PARENT, "7"))
    };
    assert!(!first.is_finished(), "the first writer is still in the window");
    assert!(
        !second.is_finished(),
        "and the second waited for it instead of applying a config read around it",
    );

    drop(release);
    first.join().expect("first writer").expect("set");
    second.join().expect("second writer").expect("set");

    let stored = HostConfig::load(|key| f.settings.get(key).ok().flatten());
    assert_eq!(f.core.config(), stored, "what the host enforces is what the store holds");
    assert_eq!(f.core.config().caps.active_per_parent, 7, "and the later write was not undone");
}

/// The whole-config write the desktop's Settings pane uses: every key stored,
/// the sanitized result applied, under the same lock one key takes.
#[test]
fn a_whole_config_is_stored_and_applied_together() {
    let f = fixture();

    let change = f
        .core
        .edit_config(|config| {
            config.confine_chat = false;
            config.stall_after_s = 45;
            config.caps.depth = 9;
        })
        .expect("edit");

    assert_eq!(change.after, f.core.config(), "the answer is what is in force");
    assert_eq!(f.core.config().stall_after_s, 45);
    assert!(!f.core.config().confine_chat);
    assert_eq!(f.core.config().caps.depth, oximux_agents::lineage::MAX_DEPTH, "sanitized on the way in");
    for (key, value) in f.core.config().entries() {
        assert_eq!(f.settings.get(key).unwrap().as_deref(), Some(value.as_str()), "{key} was stored");
    }
}

/// The pane's edit runs on the config **in force**, not on one it read first.
///
/// The lock serialises the two writes, but it cannot protect a snapshot taken
/// before it: a pane that built a whole `HostConfig` from a config it read,
/// then waited for the lock, would store that stale copy and undo whatever
/// `oximux config set` changed while it waited. Staged with the write gate, so
/// the interleaving is the one that matters and not a timing accident.
#[test]
fn an_edit_that_waits_for_the_lock_keeps_what_landed_meanwhile() {
    let f = fixture();
    assert_ne!(f.core.config().stall_after_s, 90, "the fixture must differ");

    // A single-key writer parked inside its window, holding the lock. The
    // gate's acknowledgement is what makes the order certain: started after a
    // sleep, the edit could win the lock and the staging would prove nothing.
    let (release, parked) = std::sync::mpsc::channel();
    let (arrived, at_gate) = std::sync::mpsc::channel();
    f.core.set_config_gate(arrived, parked);
    let single = {
        let core = std::sync::Arc::clone(&f.core);
        std::thread::spawn(move || core.set_config_value(keys::STALL_AFTER_S, "90"))
    };
    at_gate.recv_timeout(GATE).expect("the single writer reached the window");

    // The pane's edit, which has to wait for that lock.
    let edit = {
        let core = std::sync::Arc::clone(&f.core);
        std::thread::spawn(move || core.edit_config(|config| config.confine_chat = true))
    };
    assert!(!edit.is_finished(), "the edit is waiting for the lock");

    drop(release);
    single.join().expect("single writer").expect("set");
    let change = edit.join().expect("edit").expect("edit");

    assert!(change.after.confine_chat, "the edit applied");
    assert_eq!(change.before.stall_after_s, 90, "it read the config the other writer left");
    assert_eq!(f.core.config().stall_after_s, 90, "which it did not undo");
    let stored = HostConfig::load(|key| f.settings.get(key).ok().flatten());
    assert_eq!(f.core.config(), stored, "what the host enforces is what the store holds");
}

/// A whole-config write that cannot be stored applies nothing and reports it.
///
/// `set_many` is one transaction for this reason: a loop that failed part way
/// through would leave the rows holding a mixture of the old config and the
/// new one, and the next host start would read that mixture back as the
/// posture to enforce — including a confinement change nobody completed.
#[test]
fn a_whole_config_that_cannot_be_stored_leaves_both_the_store_and_the_host_alone() {
    let f = fixture();
    f.dispatcher.host_config_write(&operator(), keys::STALL_AFTER_S, "90");
    let before = f.core.config();
    let stored_before: Vec<(&str, Option<String>)> =
        before.entries().iter().map(|(key, _)| (*key, f.settings.get(key).unwrap())).collect();

    // `entries()` writes the rules key ninth, so eight keys have already gone
    // into the transaction when it is refused: exactly the partial write.
    f.db.with_conn(|c| {
        c.execute_batch(&format!(
            "CREATE TRIGGER refuse_one BEFORE INSERT ON settings \
             WHEN NEW.key = '{}' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            keys::VERIFY_ALLOW
        ))
    })
    .expect("trigger");

    let err = f
        .core
        .edit_config(|config| {
            config.confine_chat = !config.confine_chat;
            config.stall_after_s = 45;
        })
        .expect_err("the write is refused");

    assert!(matches!(err, ConfigError::Write(_)), "reported as a write failure: {err:?}");
    assert_eq!(f.core.config(), before, "the host enforces what it did before");
    for (key, was) in stored_before {
        assert_eq!(
            f.settings.get(key).unwrap(),
            was,
            "{key} was rolled back with the rest of the batch",
        );
    }
}

/// A store that cannot be read applies nothing.
///
/// `HostConfig::load` cannot tell an unreadable key from an unset one, so a
/// read error reaching it would substitute the shipped defaults for every
/// setting and report the write as applied — the settings equivalent of losing
/// them all.
#[test]
fn a_store_that_cannot_be_read_changes_nothing() {
    let f = fixture();
    f.dispatcher.host_config_write(&operator(), keys::STALL_AFTER_S, "90");
    assert_eq!(f.core.config().stall_after_s, 90);

    // The write has to succeed and the read-back fail, so the store is taken
    // away while a writer is parked between the two.
    let (release, parked) = std::sync::mpsc::channel();
    let (arrived, at_gate) = std::sync::mpsc::channel();
    f.core.set_config_gate(arrived, parked);
    let writer = {
        let core = f.core.clone();
        std::thread::spawn(move || core.set_config_value(keys::STALL_AFTER_S, "120"))
    };
    at_gate.recv_timeout(GATE).expect("the writer stored its value and parked");
    assert_eq!(f.settings.get(keys::STALL_AFTER_S).unwrap().as_deref(), Some("120"), "the write landed");
    f.db.with_conn(|c| c.execute("DROP TABLE settings", [])).expect("drop the table");
    drop(release);

    let refused = writer.join().expect("writer");
    assert!(matches!(refused, Err(ConfigError::Read(_))), "{refused:?}");
    assert_eq!(f.core.config().stall_after_s, 90, "the defaults were not applied over it");
}

/// The config a follower reads is one the host is already enforcing. The
/// desktop watches this to respawn open chats when confinement is switched on,
/// and a notification that arrived before the apply would race its own cause.
#[test]
fn a_follower_sees_the_config_after_it_is_in_force() {
    let f = fixture();
    let mut config = f.core.subscribe_config();
    f.dispatcher.host_config_write(&operator(), keys::CONFINE_CHAT, "false");
    assert!(config.has_changed().expect("the channel is open"), "a change was published");
    assert!(!config.borrow_and_update().confine_chat, "and it is the new value");
    assert!(!f.core.config().confine_chat, "which the host already enforces");
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

/// Rules the store must not be given are refused before it is written to.
///
/// `SettingsRepo` takes whatever it is handed, so a value over its 64 KiB
/// contract would be stored and read back at the next start as the posture to
/// enforce. Every rule here is valid, so only the size can refuse it.
#[test]
fn a_rules_value_too_big_for_the_store_is_refused() {
    let f = fixture();
    let before = f.settings.get(keys::VERIFY_ALLOW).unwrap();
    let huge = (0..6000).map(|n| format!("Bash(cmd{n}:*)")).collect::<Vec<_>>().join("\n");
    assert!(huge.len() > oximux_storage::SettingsRepo::MAX_VALUE_BYTES, "the value has to be over the limit");

    let refused = f.core.set_config_value(keys::VERIFY_ALLOW, &huge);
    assert!(matches!(refused, Err(ConfigError::BadValue(_))), "{refused:?}");
    assert_eq!(f.settings.get(keys::VERIFY_ALLOW).unwrap(), before, "and nothing was stored");
    assert_eq!(f.core.config(), HostConfig::default(), "nor applied");
}

/// The whole-config path holds the same limit, and holds it for every key.
///
/// The pane writes every key at once, so a check on the one value a verb was
/// given would miss it entirely: the oversized text arrives inside a config.
#[test]
fn a_whole_config_too_big_for_the_store_is_refused() {
    let f = fixture();
    let before = f.core.config();
    let stored_before: Vec<(&str, Option<String>)> =
        before.entries().iter().map(|(key, _)| (*key, f.settings.get(key).unwrap())).collect();
    let huge: Vec<String> = (0..6000).map(|n| format!("Bash(cmd{n}:*)")).collect();
    assert!(
        huge.join("\n").len() > oximux_storage::SettingsRepo::MAX_VALUE_BYTES,
        "the rules have to be over the limit once stored"
    );

    let refused = f.core.edit_config(|config| {
        config.confine_chat = true;
        config.verify_allow = huge;
    });

    assert!(matches!(refused, Err(ConfigError::BadValue(_))), "{refused:?}");
    assert_eq!(f.core.config(), before, "the host still enforces what it did");
    for (key, value) in stored_before {
        assert_eq!(f.settings.get(key).unwrap(), value, "{key} was left alone");
    }
}
