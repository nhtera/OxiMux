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

#[test]
fn settings_set_many_upserts_every_pair() {
    let s = repo();
    s.set("a", "old").expect("set");
    s.set_many(&[("a", "new".into()), ("b", "fresh".into())]).expect("set_many");
    assert_eq!(s.get("a").expect("get").as_deref(), Some("new"), "an existing key is updated");
    assert_eq!(s.get("b").expect("get").as_deref(), Some("fresh"), "a new one is inserted");
}

/// One transaction: a pair the database refuses takes the whole batch with it.
///
/// A whole-config write must not half-apply — rows left holding a mixture of
/// the old settings and the new ones would be read back as the posture to
/// enforce at the next start. The trigger refuses the second pair, so the
/// first has already been written when the batch fails.
#[test]
fn settings_set_many_writes_nothing_when_one_pair_is_refused() {
    let db = open_memory().expect("open memory");
    let s = SettingsRepo::new(db.clone());
    s.set("a", "old").expect("set");
    db.with_conn(|c| {
        c.execute_batch(
            "CREATE TRIGGER refuse_b BEFORE INSERT ON settings \
             WHEN NEW.key = 'b' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
    })
    .expect("trigger");

    s.set_many(&[("a", "new".into()), ("b", "fresh".into())]).expect_err("the batch is refused");

    assert_eq!(s.get("a").expect("get").as_deref(), Some("old"), "the first pair was rolled back");
    assert!(s.get("b").expect("get").is_none(), "and the refused one never landed");
}
