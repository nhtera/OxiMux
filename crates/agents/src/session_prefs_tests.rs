//! Prefs: defaults without a row, forward-only views, and the rekey.

use super::*;
use chrono::TimeZone;

fn repo() -> SessionPrefsRepo {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    SessionPrefsRepo::new(db.conn())
}

fn t(minute: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 11, 12, 12, minute, 0).single().unwrap()
}

fn ms(at: DateTime<Local>) -> i64 {
    at.timestamp_millis()
}

#[test]
fn a_session_with_no_row_reads_as_never_looked_at() {
    let r = repo();
    assert_eq!(r.get("chat:s1").unwrap(), SessionPrefs::default());
    assert_eq!(r.get("chat:s1").unwrap().last_viewed_at, None, "so its first alert shows");
}

#[test]
fn a_view_is_recorded_and_only_ever_moves_forward() {
    let r = repo();
    r.mark_viewed("chat:s1", t(10)).unwrap();
    assert_eq!(r.get("chat:s1").unwrap().last_viewed_at, Some(ms(t(10))));
    r.mark_viewed("chat:s1", t(20)).unwrap();
    assert_eq!(r.get("chat:s1").unwrap().last_viewed_at, Some(ms(t(20))));
    // A tick that raced a newer view cannot un-see what was seen.
    r.mark_viewed("chat:s1", t(5)).unwrap();
    assert_eq!(r.get("chat:s1").unwrap().last_viewed_at, Some(ms(t(20))), "forward only");
}

#[test]
fn marking_unread_survives_a_look_and_a_look_spends_it() {
    let r = repo();
    r.mark_viewed("chat:s1", t(10)).unwrap();
    r.mark_unread("chat:s1", t(11)).unwrap();
    let p = r.get("chat:s1").unwrap();
    assert!(p.unread_override, "the person asked for it to stand out");
    assert_eq!(p.last_viewed_at, Some(ms(t(10))), "the mark did not rewrite the view");
    r.mark_viewed("chat:s1", t(12)).unwrap();
    assert!(!r.get("chat:s1").unwrap().unread_override, "looking again spends the mark");
}

#[test]
fn the_notify_mode_round_trips_and_an_unknown_one_reads_as_default() {
    let r = repo();
    for mode in [NotifyMode::All, NotifyMode::NeedsYou, NotifyMode::Muted, NotifyMode::Default] {
        r.set_notify_mode("chat:s1", mode, t(10)).unwrap();
        assert_eq!(r.get("chat:s1").unwrap().notify_mode, mode);
    }
    // A row a newer build wrote must not silence the session on an older one.
    assert_eq!(NotifyMode::from_text("whisper"), NotifyMode::Default);
}

#[test]
fn collapsed_is_stored_for_the_tree_and_does_not_disturb_the_rest() {
    let r = repo();
    r.mark_viewed("chat:s1", t(10)).unwrap();
    r.set_collapsed("chat:s1", true, t(11)).unwrap();
    let p = r.get("chat:s1").unwrap();
    assert!(p.collapsed);
    assert_eq!(p.last_viewed_at, Some(ms(t(10))));
    r.set_collapsed("chat:s1", false, t(12)).unwrap();
    assert!(!r.get("chat:s1").unwrap().collapsed);
}

#[test]
fn a_write_to_a_session_with_no_row_creates_it_with_the_other_defaults() {
    let r = repo();
    r.set_collapsed("plan:run-1", true, t(10)).unwrap();
    let p = r.get("plan:run-1").unwrap();
    assert_eq!(
        p,
        SessionPrefs { collapsed: true, ..SessionPrefs::default() },
        "one write does not imply a view"
    );
}

#[test]
fn every_row_can_be_read_at_once() {
    let r = repo();
    r.mark_viewed("chat:s1", t(10)).unwrap();
    r.mark_unread("chat:s2", t(11)).unwrap();
    let mut all = r.all().unwrap();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].0, "chat:s1");
    assert_eq!(all[0].1.last_viewed_at, Some(ms(t(10))));
    assert!(all[1].1.unread_override);
}

#[test]
fn a_rekeyed_chat_keeps_what_the_person_had_already_seen() {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    let r = SessionPrefsRepo::new(db.conn());
    r.mark_viewed(&chat_key("placeholder"), t(10)).unwrap();
    r.set_notify_mode(&chat_key("placeholder"), NotifyMode::Muted, t(10)).unwrap();

    rekey(&db, "placeholder", "real-id");

    assert_eq!(r.get(&chat_key("placeholder")).unwrap(), SessionPrefs::default(), "moved");
    let moved = r.get(&chat_key("real-id")).unwrap();
    assert_eq!(moved.last_viewed_at, Some(ms(t(10))), "the alerts it dealt with stay dealt with");
    assert_eq!(moved.notify_mode, NotifyMode::Muted);
}

#[test]
fn a_rekey_onto_a_session_that_already_has_prefs_merges_the_two() {
    for (old_view, new_view) in [(10, 30), (30, 10)] {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        let r = SessionPrefsRepo::new(db.conn());
        r.mark_viewed(&chat_key("old"), t(old_view)).unwrap();
        r.set_notify_mode(&chat_key("old"), NotifyMode::Muted, t(old_view)).unwrap();
        r.mark_viewed(&chat_key("new"), t(new_view)).unwrap();

        rekey(&db, "old", "new");

        let moved = r.get(&chat_key("new")).unwrap();
        assert_eq!(
            moved.last_viewed_at,
            Some(ms(t(old_view.max(new_view)))),
            "the later view wins, so nothing seen is un-seen"
        );
        assert_eq!(moved.notify_mode, NotifyMode::Muted, "the mode the person set, from the old tab");
        assert_eq!(r.get(&chat_key("old")).unwrap(), SessionPrefs::default(), "the old key is gone");
    }
}

#[test]
fn an_unread_mark_on_either_side_of_a_rekey_survives_it() {
    for mark_old in [true, false] {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        let r = SessionPrefsRepo::new(db.conn());
        r.mark_viewed(&chat_key("old"), t(10)).unwrap();
        r.mark_viewed(&chat_key("new"), t(10)).unwrap();
        let marked = if mark_old { "old" } else { "new" };
        r.mark_unread(&chat_key(marked), t(11)).unwrap();

        rekey(&db, "old", "new");

        assert!(
            r.get(&chat_key("new")).unwrap().unread_override,
            "marked on `{marked}`: swallowing an alert is worse than showing one twice"
        );
    }
}

#[test]
fn a_rekey_of_a_session_with_no_prefs_writes_nothing() {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    let r = SessionPrefsRepo::new(db.conn());
    rekey(&db, "old", "new");
    assert!(r.all().unwrap().is_empty(), "no row invented for a session nobody looked at");
}

/// Carry `old`'s prefs to `new` the way the lineage rekey does: inside one
/// transaction on the same connection.
fn rekey(db: &oximux_storage::db::Db, old: &str, new: &str) {
    let conn = db.conn();
    let mut guard = conn.lock().unwrap();
    let tx = guard.transaction().unwrap();
    rekey_in(&tx, &chat_key(old), &chat_key(new)).unwrap();
    tx.commit().unwrap();
}

#[test]
fn a_chat_and_a_run_root_cannot_collide_on_one_key() {
    assert_eq!(chat_key("plan:7"), "chat:plan:7");
    assert_ne!(chat_key("plan:7"), "plan:7");
    assert_eq!(cockpit_key("42"), "cockpit:42");
}

#[test]
fn prefs_can_be_forgotten_with_the_session() {
    let r = repo();
    r.mark_viewed("chat:s1", t(10)).unwrap();
    r.forget("chat:s1").unwrap();
    assert_eq!(r.get("chat:s1").unwrap(), SessionPrefs::default());
}

#[test]
fn a_view_that_arrives_late_does_not_undo_a_newer_unread_mark() {
    let r = repo();
    r.mark_viewed("chat:s1", t(9)).unwrap();
    r.mark_unread("chat:s1", t(11)).unwrap();
    // A view computed at minute 10 reaches the store after the mark at 11.
    r.mark_viewed("chat:s1", t(10)).unwrap();
    let p = r.get("chat:s1").unwrap();
    assert!(p.unread_override, "the person asked for it after that view was taken");
    assert_eq!(p.last_viewed_at, Some(ms(t(10))), "the view itself still counts");
    // A view genuinely after the mark does spend it.
    r.mark_viewed("chat:s1", t(12)).unwrap();
    assert!(!r.get("chat:s1").unwrap().unread_override);
}

#[test]
fn a_late_write_of_any_kind_cannot_let_a_stale_view_clear_an_unread_mark() {
    // `mark_viewed` decides by `updated_at`, so every writer has to keep it
    // moving forward: a delayed collapse or mode change that moved it back
    // would make an older view look recent enough to spend the mark.
    for late in ["collapsed", "notify_mode"] {
        let r = repo();
        r.mark_unread("chat:s1", t(11)).unwrap();
        match late {
            "collapsed" => r.set_collapsed("chat:s1", true, t(9)).unwrap(),
            _ => r.set_notify_mode("chat:s1", NotifyMode::Muted, t(9)).unwrap(),
        }
        r.mark_viewed("chat:s1", t(10)).unwrap();
        assert!(
            r.get("chat:s1").unwrap().unread_override,
            "a late {late} write must not rewind the clock the mark is judged by"
        );
    }
}

#[test]
fn a_rekey_keeps_the_later_clock_rather_than_taking_the_current_one() {
    // `mark_viewed` judges an unread mark by `updated_at`, so a rekey that
    // stamped the current time would rewind it whenever the system clock had
    // stepped back, and let a stale view clear a mark made after it.
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    let r = SessionPrefsRepo::new(db.conn());
    r.mark_viewed(&chat_key("old"), t(10)).unwrap();
    r.mark_unread(&chat_key("new"), t(40)).unwrap();

    rekey(&db, "old", "new");

    let moved = r.get(&chat_key("new")).unwrap();
    assert!(moved.unread_override, "the mark survived the rekey");
    // A view from before the mark must still not spend it, however long ago
    // the rekey happened to run.
    r.mark_viewed(&chat_key("new"), t(20)).unwrap();
    assert!(
        r.get(&chat_key("new")).unwrap().unread_override,
        "the rekey kept the clock the mark is judged by"
    );
    r.mark_viewed(&chat_key("new"), t(50)).unwrap();
    assert!(!r.get(&chat_key("new")).unwrap().unread_override, "a later view does spend it");
}
