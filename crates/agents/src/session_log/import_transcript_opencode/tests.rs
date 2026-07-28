//! Fixture-driven tests: happy-path transcript shape, read-only open, and the
//! corrupt/missing-store degrade paths.

use super::*;
use rusqlite::Connection;

/// The scrubbed real-schema extract in `testdata/opencode_import_fixture.sql`
/// (message + part tables, placeholder text).
const FIXTURE_SQL: &str = include_str!("../testdata/opencode_import_fixture.sql");

fn write_fixture_db(home: &std::path::Path) {
    let dir = home.join(".local/share/opencode");
    std::fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join("opencode.db")).unwrap();
    conn.execute_batch(FIXTURE_SQL).unwrap();
}

#[test]
fn maps_text_reasoning_and_tool_parts_in_turn_order() {
    let home = tempfile::tempdir().unwrap();
    write_fixture_db(home.path());
    let entries = opencode_transcript(home.path(), "ses_fixture");

    assert_eq!(entries.len(), 4, "{entries:?}");
    assert_eq!(
        entries[0],
        ThreadEntry::User {
            text: "placeholder prompt one".into(),
            images: Vec::new(),
            checkpoint: None,
        }
    );
    match &entries[1] {
        ThreadEntry::Assistant(m) => {
            assert_eq!(m.text, "placeholder reply one");
            assert_eq!(m.thinking, "placeholder reasoning");
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
    // The tool part never leaks its raw JSON (command/output strings) — only
    // its title + status land in the notice.
    match &entries[2] {
        ThreadEntry::ContextCompaction { summary } => {
            assert_eq!(summary, "Ran a command (completed)");
            assert!(!summary.contains("echo placeholder"));
            assert!(!summary.contains("{"));
        }
        other => panic!("expected ContextCompaction, got {other:?}"),
    }
    assert_eq!(
        entries[3],
        ThreadEntry::User {
            text: "placeholder prompt two".into(),
            images: Vec::new(),
            checkpoint: None,
        }
    );
}

#[test]
fn plugin_injected_user_messages_are_skipped_but_real_prompts_kept() {
    // OpenCode plugins record standalone user-role messages the user never
    // typed. They must not become bubbles or count as user turns — the
    // companion-terminal sync anchors on the user-prompt count.
    let home = tempfile::tempdir().unwrap();
    write_fixture_db(home.path());
    let conn =
        Connection::open(home.path().join(".local/share/opencode/opencode.db")).unwrap();
    for (i, text) in [
        "<ultrawork-mode>\n\n**MANDATORY**: placeholder",
        "  <user-prompt-submit-hook>\n## Session placeholder",
        "a real prompt that merely mentions <user-prompt-submit-hook> later",
    ]
    .iter()
    .enumerate()
    {
        let mid = format!("msg_plumb{i}");
        let t = 100 + i as i64;
        conn.execute(
            "INSERT INTO message VALUES (?1,'ses_fixture',?2,?2,'{\"role\":\"user\"}')",
            rusqlite::params![mid, t],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1,?2,'ses_fixture',?3,?3,?4)",
            rusqlite::params![
                format!("prt_plumb{i}"),
                mid,
                t,
                serde_json::json!({ "type": "text", "text": text }).to_string()
            ],
        )
        .unwrap();
    }
    let entries = opencode_transcript(home.path(), "ses_fixture");
    let users: Vec<&str> = entries
        .iter()
        .filter_map(|e| match e {
            ThreadEntry::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        [
            "placeholder prompt one",
            "placeholder prompt two",
            "a real prompt that merely mentions <user-prompt-submit-hook> later",
        ],
        "plumbing rows skipped, real prompts (fixture + appended) kept"
    );
}

#[test]
fn missing_database_yields_empty_transcript() {
    let home = tempfile::tempdir().unwrap();
    let entries = opencode_transcript(home.path(), "ses_fixture");
    assert!(entries.is_empty());
}

#[test]
fn unknown_session_id_yields_empty_transcript() {
    let home = tempfile::tempdir().unwrap();
    write_fixture_db(home.path());
    let entries = opencode_transcript(home.path(), "ses_does_not_exist");
    assert!(entries.is_empty());
}

#[test]
fn corrupt_store_degrades_to_a_single_notice_row_not_a_panic() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".local/share/opencode");
    std::fs::create_dir_all(&dir).unwrap();
    // A schema that doesn't carry the columns the query expects (version
    // drift / a torn file) — the mapper must degrade, never panic.
    let conn = Connection::open(dir.join("opencode.db")).unwrap();
    conn.execute_batch("CREATE TABLE message (id TEXT PRIMARY KEY);").unwrap();
    let entries = opencode_transcript(home.path(), "ses_fixture");
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        ThreadEntry::ContextCompaction { summary } => assert_eq!(summary, UNREADABLE_NOTICE),
        other => panic!("expected a notice row, got {other:?}"),
    }
}

#[test]
fn opens_the_database_strictly_read_only() {
    let home = tempfile::tempdir().unwrap();
    write_fixture_db(home.path());
    let db = home.path().join(OPENCODE_DB_REL);

    let _entries = opencode_transcript(home.path(), "ses_fixture");

    // A read-only open never creates WAL/SHM side files, and a follow-up
    // read-write connection must still be able to write (proving the mapper
    // never held an exclusive/write lock on the store).
    assert!(!db.with_extension("db-wal").exists());
    assert!(!db.with_extension("db-shm").exists());
    let rw = Connection::open(&db).unwrap();
    rw.execute("INSERT INTO message VALUES ('msg_u3','ses_fixture',7,7,'{\"role\":\"user\"}')", [])
        .expect("store must not be left write-locked by a read-only mapper");
}
