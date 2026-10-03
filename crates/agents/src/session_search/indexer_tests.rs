//! Indexer passes over synthetic transcripts in a temp home.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::indexer::Worker;
use super::watermark::{self, ReadPlan};
use super::{IndexSources, db, writer};

const CLAUDE_SESSION: &str = "11111111-2222-3333-4444-555555555555";

fn claude_lines() -> Vec<String> {
    vec![
        r#"{"type":"user","cwd":"/work/app","gitBranch":"main","timestamp":"2026-09-01T10:00:00.000Z","message":{"role":"user","content":"Fix the flaky login test"}}"#.into(),
        r#"{"type":"assistant","timestamp":"2026-09-01T10:00:05.000Z","message":{"content":[{"type":"thinking","thinking":"Look at the session cache"},{"type":"text","text":"Looking at resolveTerminalPath first."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test zebracorn_suite","description":"run"}}]}}"#.into(),
        r#"{"type":"user","timestamp":"2026-09-01T10:00:09.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"test result: ok. quokkaflux passed"}]}}"#.into(),
        r#"{"type":"user","isMeta":true,"timestamp":"2026-09-01T10:00:10.000Z","message":{"content":"injected caveat metaonlyword"}}"#.into(),
        r#"{"type":"user","timestamp":"2026-09-01T10:00:11.000Z","message":{"content":"<task-notification><summary>background echidnaflag done</summary></task-notification>"}}"#.into(),
        r#"{"type":"ai-title","aiTitle":"Flaky login test"}"#.into(),
    ]
}

fn codex_lines() -> Vec<String> {
    vec![
        r#"{"timestamp":"2026-09-02T08:00:00.000Z","type":"session_meta","payload":{"id":"0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee","cwd":"/work/api","timestamp":"2026-09-02T08:00:00.000Z","git":{"branch":"feat/x"}}}"#.into(),
        r##"{"timestamp":"2026-09-02T08:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions injectedword"}]}}"##.into(),
        r#"{"timestamp":"2026-09-02T08:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Rename the billing module"}]}}"#.into(),
        r#"{"timestamp":"2026-09-02T08:00:03.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Plan the rename carefully"}]}}"#.into(),
        r#"{"timestamp":"2026-09-02T08:00:04.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"rg -n platypusgrep src\"]}"}}"#.into(),
        r#"{"timestamp":"2026-09-02T08:00:05.000Z","type":"response_item","payload":{"type":"function_call_output","output":"{\"output\":\"src/billing.rs:1: narwhalout\"}"}}"#.into(),
        r#"{"timestamp":"2026-09-02T08:00:06.000Z","type":"event_msg","payload":{"type":"agent_message","message":"echoonlyword"}}"#.into(),
    ]
}

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        Self { _dir: dir, root }
    }
    // Built one component at a time: the indexer keys files by the path
    // discovery yields, which on Windows uses backslashes throughout.
    fn claude_file(&self) -> PathBuf {
        self.root.join(".claude").join("projects").join("-work-app").join(format!("{CLAUDE_SESSION}.jsonl"))
    }
    fn codex_file(&self) -> PathBuf {
        ["sessions", "2026", "09", "02", "rollout-2026-09-02T08-00-00-0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"]
            .iter()
            .fold(self.root.join(".codex"), |p, c| p.join(c))
    }
    fn db(&self) -> PathBuf {
        self.root.join("data").join("session-search.sqlite")
    }
    fn worker(&self) -> Worker {
        let sources = IndexSources {
            claude_dir: Some(self.root.join(".claude")),
            codex_dir: Some(self.root.join(".codex")),
        };
        Worker::new(sources, self.db(), Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Default::default())))
    }
    fn reader(&self) -> Connection {
        db::open_reader(&self.db()).unwrap()
    }
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
}

fn append(path: &Path, text: &str) {
    fs::OpenOptions::new().append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
}

/// Sessions (by agent) whose rows match an FTS expression.
fn hits(conn: &Connection, expr: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT s.agent FROM messages_fts f JOIN messages m ON m.id = f.rowid
             JOIN sessions s ON s.id = m.session_row_id WHERE messages_fts MATCH ?1 ORDER BY s.agent",
        )
        .unwrap();
    stmt.query_map([expr], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

fn row_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0)).unwrap()
}

#[test]
fn fts5_is_available() {
    let home = Home::new();
    db::open_writer(&home.db()).unwrap();
}

#[test]
fn indexes_claude_and_codex_turns_including_tool_text() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    write_lines(&home.codex_file(), &codex_lines());
    let mut w = home.worker();
    assert!(!w.pass());
    let c = home.reader();
    assert_eq!(hits(&c, "zebracorn_suite"), ["claude"]);
    assert_eq!(hits(&c, "quokkaflux"), ["claude"]);
    assert_eq!(hits(&c, "platypusgrep"), ["codex"]);
    assert_eq!(hits(&c, "narwhalout"), ["codex"]);
    assert_eq!(hits(&c, "careful*"), ["codex"]);
    // The identifier split makes compound names answer to their words.
    assert_eq!(hits(&c, "terminal path"), ["claude"]);
    // Harness notifications are searchable, but as tool text.
    let role: String = c
        .query_row(
            "SELECT m.role FROM messages_fts f JOIN messages m ON m.id = f.rowid WHERE messages_fts MATCH 'echidnaflag'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(role, "tool");
    // Injected context, meta turns and event echoes stay out.
    for word in ["metaonlyword", "injectedword", "echoonlyword"] {
        assert!(hits(&c, word).is_empty(), "{word} should not be indexed");
    }
    let (agent, sid, title, cwd, branch, count): (String, String, String, String, String, i64) = c
        .query_row(
            "SELECT agent, session_id, COALESCE(custom_title, ai_title, last_prompt, first_prompt), cwd, branch,
               message_count FROM sessions ORDER BY agent LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!((agent.as_str(), sid.as_str()), ("claude", CLAUDE_SESSION));
    assert_eq!((title.as_str(), cwd.as_str(), branch.as_str()), ("Flaky login test", "/work/app", "main"));
    // User records (prompt + notification) and one assistant record; tool
    // results and meta excluded.
    assert_eq!(count, 3);
    let codex: (String, String, String) = c
        .query_row(
            "SELECT session_id, first_prompt, branch FROM sessions WHERE agent = 'codex'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(codex, ("0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(), "Rename the billing module".into(), "feat/x".into()));
    let status = w.status.lock().unwrap().clone();
    assert_eq!((status.sessions, status.indexing), (2, false));
    assert!(status.messages >= 9 && status.db_bytes > 0);
}

#[test]
fn appended_lines_are_read_from_the_watermark() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    let mut w = home.worker();
    w.pass();
    let before = row_count(&home.reader());
    let first_ids: Vec<i64> = {
        let c = home.reader();
        let mut s = c.prepare("SELECT id FROM messages ORDER BY id").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    append(
        &home.claude_file(),
        "{\"type\":\"assistant\",\"timestamp\":\"2026-09-01T11:00:00.000Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"appended wombatline\"}]}}\n",
    );
    w.pass();
    let c = home.reader();
    assert_eq!(row_count(&c), before + 1);
    assert_eq!(hits(&c, "wombatline"), ["claude"]);
    // The head was not re-read: the original rows keep their ids.
    let still: i64 = c
        .query_row(&format!("SELECT count(*) FROM messages WHERE id IN ({})", first_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")), [], |r| r.get(0))
        .unwrap();
    assert_eq!(still, before);
    let (count, updated): (i64, i64) =
        c.query_row("SELECT message_count, updated_ms FROM sessions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(count, 4);
    assert_eq!(updated, super::super::session_log::parse_timestamp_ms("2026-09-01T11:00:00.000Z").unwrap());
}

#[test]
fn a_partial_last_line_waits_for_its_newline() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    let line = "{\"type\":\"assistant\",\"timestamp\":\"2026-09-01T11:00:00.000Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"halfwritten koalaword\"}]}}";
    let (head, tail) = line.split_at(40);
    append(&home.claude_file(), head);
    let mut w = home.worker();
    w.pass();
    assert!(hits(&home.reader(), "koalaword").is_empty());
    append(&home.claude_file(), &format!("{tail}\n"));
    w.pass();
    assert_eq!(hits(&home.reader(), "koalaword"), ["claude"]);
}

#[test]
fn rewritten_file_is_replaced() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    let mut w = home.worker();
    w.pass();
    write_lines(
        &home.claude_file(),
        &[r#"{"type":"user","timestamp":"2026-09-03T10:00:00.000Z","message":{"content":"brand new echidnaword"}}"#.into()],
    );
    w.pass();
    let c = home.reader();
    assert!(hits(&c, "zebracorn_suite").is_empty());
    assert_eq!(hits(&c, "echidnaword"), ["claude"]);
    let sessions: i64 = c.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap();
    assert_eq!(sessions, 1);
}

#[test]
fn deleted_file_is_retired_on_the_sweep() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    write_lines(&home.codex_file(), &codex_lines());
    let mut w = home.worker();
    w.pass();
    fs::remove_file(home.codex_file()).unwrap();
    w.last_sweep = None;
    w.pass();
    let c = home.reader();
    assert!(hits(&c, "platypusgrep").is_empty());
    let files: i64 = c.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
    assert_eq!(files, 1);
}

#[cfg(unix)]
#[test]
fn repeated_failures_hold_a_file_out() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    fs::set_permissions(home.claude_file(), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(home.claude_file()).is_ok() {
        return; // Running as root: permissions do not bite.
    }
    let mut w = home.worker();
    for _ in 0..watermark::MAX_FAILURES {
        w.pass();
    }
    let conn = db::open_writer(&home.db()).unwrap();
    let key = home.claude_file().to_string_lossy().into_owned();
    let prev = writer::load_watermark(&conn, &key).unwrap();
    let stat =
        super::ingest::discover(&IndexSources { claude_dir: Some(home.root.join(".claude")), codex_dir: None }).files[0].stat;
    assert_eq!(watermark::decide(prev.as_ref(), stat, |_| true), ReadPlan::HeldOut);
    fs::set_permissions(home.claude_file(), fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn schema_mismatch_rebuilds_the_index() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    home.worker().pass();
    {
        let c = Connection::open(home.db()).unwrap();
        c.execute("UPDATE meta SET value = '0' WHERE key = 'schema_version'", []).unwrap();
    }
    let c = db::open_writer(&home.db()).unwrap();
    assert_eq!(row_count(&c), 0);
}

#[cfg(unix)]
#[test]
fn index_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    home.worker().pass();
    // Reopen so the WAL/SHM siblings exist when permissions are asserted.
    let _c = db::open_writer(&home.db()).unwrap();
    for p in db::index_files(&home.db()) {
        if p.exists() {
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{}", p.display());
        }
    }
    let dir = fs::metadata(home.db().parent().unwrap()).unwrap();
    assert_eq!(dir.permissions().mode() & 0o777, 0o700);
}

#[test]
fn a_file_ending_in_a_partial_line_is_skipped_once_read() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    append(&home.claude_file(), "{\"type\":\"user\",\"message\":{\"content\":\"still being wri");
    let mut w = home.worker();
    w.pass();
    let conn = db::open_writer(&home.db()).unwrap();
    let key = home.claude_file().to_string_lossy().into_owned();
    let prev = writer::load_watermark(&conn, &key).unwrap();
    let found = super::ingest::discover(&IndexSources { claude_dir: Some(home.root.join(".claude")), codex_dir: None });
    assert!(found.complete);
    // Nothing new since the read: no re-read (and no empty commit) next pass.
    assert_eq!(watermark::decide(prev.as_ref(), found.files[0].stat, |_| true), ReadPlan::Skip);
}

#[test]
fn a_failure_after_a_replace_keeps_counting() {
    let home = Home::new();
    write_lines(&home.claude_file(), &claude_lines());
    home.worker().pass();
    let mut conn = db::open_writer(&home.db()).unwrap();
    let key = home.claude_file().to_string_lossy().into_owned();
    for _ in 0..2 {
        writer::record_failure(&conn, &key, super::Agent::Claude, 77).unwrap();
        // A Replace drops the content before reading; the count must stay.
        writer::drop_content(&mut conn, &key).unwrap();
    }
    writer::record_failure(&conn, &key, super::Agent::Claude, 77).unwrap();
    let w = writer::load_watermark(&conn, &key).unwrap().unwrap();
    assert_eq!((w.fail_count, w.offset, w.session_row_id), (3, 0, None));
    assert_eq!(row_count(&conn), 0);
}
