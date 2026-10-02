//! Query engine against a temp index seeded through the writer.

use std::time::Instant;

use rusqlite::Connection;

use super::turn::{MetaDelta, Row, identifiers};
use super::writer::{self, Batch};
use super::{Agent, Role, Scope, SearchError, SearchRequest, Sort, db, search};

struct Index {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    conn: Connection,
    n: usize,
}

impl Index {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idx.sqlite");
        let conn = db::open_writer(&path).unwrap();
        Self { _dir: dir, path, conn, n: 0 }
    }

    /// One session with `turns`, updated at `updated_ms`, run in `cwd`.
    fn add(&mut self, agent: Agent, cwd: &str, updated_ms: i64, turns: &[(Role, &str)]) {
        self.n += 1;
        let rows: Vec<Row> = turns
            .iter()
            .map(|(role, text)| Row { role: *role, text: text.to_string(), identifiers: identifiers(text), ts_ms: None })
            .collect();
        let meta = MetaDelta {
            cwd: Some(cwd.into()),
            first_prompt: Some(format!("session {}", self.n)),
            first_ts_ms: Some(updated_ms),
            last_ts_ms: Some(updated_ms),
            message_count: turns.len() as i64,
            ..Default::default()
        };
        let path = format!("/t/{}.jsonl", self.n);
        writer::commit_batch(
            &mut self.conn,
            &Batch {
                path: &path,
                agent,
                fallback_session_id: &format!("sid-{}", self.n),
                session_row_id: None,
                meta: &meta,
                rows: &rows,
                offset: 1,
                mtime_ms: 1,
                size: 1,
            },
        )
        .unwrap();
    }

    fn reader(&self) -> Connection {
        db::open_reader(&self.path).unwrap()
    }
}

fn req(q: &str) -> SearchRequest {
    SearchRequest { query: q.into(), ..Default::default() }
}

fn titles(idx: &Index, r: &SearchRequest) -> Vec<String> {
    search(&idx.reader(), r).unwrap().hits.into_iter().map(|h| h.title).collect()
}

#[test]
fn phrase_rung_wins_before_falling_back_to_and() {
    let mut idx = Index::new();
    idx.add(Agent::Claude, "/w", 1, &[(Role::User, "the flaky login test again")]);
    idx.add(Agent::Claude, "/w", 2, &[(Role::User, "flaky network"), (Role::Assistant, "the login page")]);
    assert_eq!(titles(&idx, &req("flaky login")), ["session 1"]);
    // No phrase match → messages holding both terms (AND is per message).
    idx.add(Agent::Claude, "/w", 3, &[(Role::User, "login is flaky")]);
    let mut both = titles(&idx, &req("login flaky"));
    both.sort();
    assert_eq!(both, ["session 1", "session 3"]);
    // No message holds both → any term.
    assert_eq!(titles(&idx, &req("network page")).len(), 1);
    assert_eq!(titles(&idx, &req("network test")).len(), 2);
}

#[test]
fn conversation_scope_excludes_tool_only_hits() {
    let mut idx = Index::new();
    idx.add(Agent::Claude, "/w", 1, &[(Role::Tool, "Bash: cargo build --release"), (Role::User, "hello")]);
    idx.add(Agent::Codex, "/w", 2, &[(Role::Assistant, "run cargo first")]);
    assert_eq!(titles(&idx, &req("cargo")).len(), 2);
    let r = SearchRequest { scope: Scope::Conversation, ..req("cargo") };
    assert_eq!(titles(&idx, &r), ["session 2"]);
}

#[test]
fn cwd_prefix_matches_whole_components_and_agents_filter() {
    let mut idx = Index::new();
    idx.add(Agent::Claude, "/a/b", 1, &[(Role::User, "needle")]);
    idx.add(Agent::Claude, "/a/b/c", 2, &[(Role::User, "needle")]);
    idx.add(Agent::Codex, "/a/bc", 3, &[(Role::User, "needle")]);
    let r = SearchRequest { cwd_prefixes: vec!["/a/b/".into()], sort: Sort::Newest, ..req("needle") };
    assert_eq!(titles(&idx, &r), ["session 2", "session 1"]);
    let r = SearchRequest { agents: vec![Agent::Codex], ..req("needle") };
    assert_eq!(titles(&idx, &r), ["session 3"]);
    let r = SearchRequest { since_ms: Some(2), sort: Sort::Newest, ..req("needle") };
    assert_eq!(titles(&idx, &r), ["session 3", "session 2"]);
}

#[test]
fn snippets_highlight_the_match_for_every_role() {
    let mut idx = Index::new();
    idx.add(Agent::Claude, "/w", 1, &[(Role::User, "please rename the gizmo module")]);
    idx.add(Agent::Claude, "/w", 2, &[(Role::Assistant, "I renamed gizmo everywhere")]);
    idx.add(Agent::Claude, "/w", 3, &[(Role::Tool, "Bash: rg gizmo src")]);
    let page = search(&idx.reader(), &req("gizmo")).unwrap();
    assert_eq!(page.hits.len(), 3);
    for h in &page.hits {
        let hit: Vec<&str> = h.snippet.iter().filter(|s| s.hit).map(|s| s.text.as_str()).collect();
        assert_eq!(hit, ["gizmo"], "{:?}", h.role);
    }
    let roles: std::collections::HashSet<Role> = page.hits.iter().map(|h| h.role).collect();
    assert_eq!(roles.len(), 3);
}

#[test]
fn identifier_pieces_and_prefixes_match() {
    let mut idx = Index::new();
    idx.add(Agent::Claude, "/w", 1, &[(Role::Assistant, "fixed resolveTerminalPath in shell.rs. done.")]);
    assert_eq!(titles(&idx, &req("terminal path")).len(), 1);
    assert_eq!(titles(&idx, &req("resolveTerm")).len(), 1, "prefix of the last term");
    assert_eq!(titles(&idx, &req("shell.rs")).len(), 1);
    assert_eq!(titles(&idx, &req("done")).len(), 1, "sentence punctuation does not hide a word");
}

#[test]
fn pages_are_stable_and_cursors_survive_writes() {
    let mut idx = Index::new();
    for i in 0..25 {
        idx.add(Agent::Claude, "/w", i, &[(Role::User, "common term")]);
    }
    let all = titles(&idx, &SearchRequest { limit: 100, ..req("common") });
    let mut paged = Vec::new();
    let mut r = SearchRequest { limit: 10, ..req("common") };
    loop {
        let page = search(&idx.reader(), &r).unwrap();
        assert_eq!(page.total, 25);
        paged.extend(page.hits.into_iter().map(|h| h.title));
        match page.next_cursor {
            Some(c) => r.cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(paged, all);
    let r = SearchRequest { limit: 10, ..req("common") };
    let cursor = search(&idx.reader(), &r).unwrap().next_cursor.unwrap();
    idx.add(Agent::Claude, "/w", 99, &[(Role::User, "common again")]);
    // The indexer wrote in between: the next page still loads.
    let next = search(&idx.reader(), &SearchRequest { cursor: Some(cursor.clone()), ..r.clone() }).unwrap();
    assert_eq!(next.total, 26);
    assert_eq!(next.hits.len(), 10);
    let other = search(&idx.reader(), &SearchRequest { cursor: Some(cursor), ..req("different") });
    assert!(matches!(other, Err(SearchError::InvalidCursor)));
}

#[test]
fn no_printable_input_is_an_fts_syntax_error() {
    let mut idx = Index::new();
    idx.add(Agent::Claude, "/w", 1, &[(Role::User, "a b-c C++ \"x\" (y) {z} NEAR/2 OR AND NOT * ^ : col:val")]);
    let conn = idx.reader();
    let alphabet: Vec<char> =
        " \t\"'()*+-./:^{}[]\\|&!?#@$%~`,;<>=_0123456789abcXYZ éß日本🙂ANDORNOTNEAR".chars().collect();
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..2000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 24) as usize;
        let q: String = (0..len)
            .map(|i| alphabet[((seed >> (i % 8 * 8)) as usize + i * 31) % alphabet.len()])
            .collect();
        if let Err(e) = search(&conn, &req(&q)) {
            panic!("query {q:?} failed: {e}");
        }
    }
    for q in ["AND", "OR", "NOT", "NEAR", "\"", "*", "-x", "col:val", "a OR", "(", "^a"] {
        search(&conn, &req(q)).unwrap_or_else(|e| panic!("query {q:?} failed: {e}"));
    }
}

#[test]
fn search_stays_fast_on_a_larger_index() {
    let mut idx = Index::new();
    let words = ["build", "test", "deploy", "render", "parser", "index", "query", "socket", "cache", "token"];
    for s in 0..200 {
        let turns: Vec<(Role, String)> = (0..100)
            .map(|i| {
                let role = [Role::User, Role::Assistant, Role::Tool][i % 3];
                (role, format!("{} the {} for {} step {i}", words[(s + i) % 10], words[(s * 7 + i) % 10], words[i % 10]))
            })
            .collect();
        let refs: Vec<(Role, &str)> = turns.iter().map(|(r, t)| (*r, t.as_str())).collect();
        idx.add(Agent::Claude, &format!("/w/{}", s % 7), s as i64, &refs);
    }
    let conn = idx.reader();
    let start = Instant::now();
    for q in ["build", "parser cache", "socket token step", "zzz", "deploy the"] {
        search(&conn, &req(q)).unwrap();
    }
    // 20K rows; generous for an unoptimized debug build of SQLite.
    assert!(start.elapsed().as_millis() < 3000, "search too slow: {:?}", start.elapsed());
}

#[cfg(unix)]
#[test]
fn scope_prefixes_match_through_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir_all(real.join("sub")).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut idx = Index::new();
    // Recorded through the link; searched by the real path, and vice versa.
    idx.add(Agent::Claude, &link.join("sub").to_string_lossy(), 1, &[(Role::User, "needle")]);
    idx.add(Agent::Claude, &real.to_string_lossy(), 2, &[(Role::User, "needle")]);
    for prefix in [&real, &link] {
        let r = SearchRequest { cwd_prefixes: vec![prefix.to_string_lossy().into_owned()], ..req("needle") };
        assert_eq!(titles(&idx, &r).len(), 2, "prefix {}", prefix.display());
    }
}
