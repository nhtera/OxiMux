//! SettingsRepo integration tests — upsert + delete + missing-key semantics.

use oximux_storage::{SettingsRepo, open_memory};

fn repo() -> SettingsRepo {
    SettingsRepo::new(open_memory().expect("open memory"))
}

#[test]
fn settings_get_missing_key_returns_none() {
    assert!(repo().get("nope").expect("get").is_none());
}

#[test]
fn settings_set_and_get_round_trip() {
    let s = repo();
    s.set("theme", "charcoal").expect("set");
    assert_eq!(s.get("theme").expect("get").as_deref(), Some("charcoal"));
}

#[test]
fn settings_overwrite_via_upsert() {
    let s = repo();
    s.set("k", "v1").expect("set v1");
    s.set("k", "v2").expect("set v2");
    assert_eq!(s.get("k").expect("get").as_deref(), Some("v2"));
}

#[test]
fn settings_delete_existing_key() {
    let s = repo();
    s.set("k", "v").expect("set");
    s.delete("k").expect("delete");
    assert!(s.get("k").expect("get").is_none());
}

#[test]
fn settings_delete_missing_key_is_noop() {
    let s = repo();
    s.delete("never_existed").expect("delete should succeed");
}
