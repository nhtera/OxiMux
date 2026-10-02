//! Search over the index: per-session best hits ranked by bm25, filtered by
//! scope / agent / time / cwd, sorted, paged, with a highlighted snippet per
//! hit on the returned page.
//!
//! Callers run this on a background executor with a short-lived read-only
//! connection ([`super::open_reader`]); nothing here holds a read transaction
//! between calls.

use rusqlite::{Connection, ToSql};

use super::cursor::{Cursor, request_key};
use super::query_plan::{self, QueryPlan};
use super::snippet::{self, Span};
use super::turn::Role;
use super::{Agent, canonical_cwd_key, cwd_key};

/// Sessions considered per query before sorting and paging.
const CANDIDATE_CAP: usize = 600;
/// Best-scoring rows grouped into sessions. A very common word matches a
/// large share of the index; past this many rows the weakest add nothing a
/// 600-session list would show, and joining them all costs hundreds of ms.
const ROW_CAP: usize = 20_000;
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;
/// bm25 column weights: user, assistant, tool, identifiers.
const WEIGHTS: &str = "3.0, 2.0, 1.0, 1.0";
/// Long sessions match more by sheer size; damp that slightly.
const LENGTH_DAMPING: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Scope {
    /// What the user and the agent said (tool calls and output excluded).
    Conversation,
    /// Everything, including tool calls and their output.
    #[default]
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Sort {
    #[default]
    Relevance,
    Newest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SearchRequest {
    pub query: String,
    pub scope: Scope,
    pub sort: Sort,
    /// Empty = every agent.
    pub agents: Vec<Agent>,
    /// Keep sessions whose cwd is one of these or inside one. Empty = any.
    pub cwd_prefixes: Vec<String>,
    /// Keep sessions updated at or after this unix-ms time.
    pub since_ms: Option<i64>,
    /// Page size (0 → [`DEFAULT_LIMIT`]; capped at [`MAX_LIMIT`]).
    pub limit: usize,
    /// From a previous page's `next_cursor`.
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub session_row_id: i64,
    pub agent: Agent,
    pub session_id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub file_path: String,
    pub updated_ms: Option<i64>,
    pub message_count: i64,
    /// Role of the best-matching row (the snippet's source).
    pub role: Role,
    pub snippet: Vec<Span>,
    pub score: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// Matching sessions (capped at the candidate limit).
    pub total: usize,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("invalid cursor")]
    InvalidCursor,
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for SearchError {
    fn from(e: rusqlite::Error) -> Self {
        SearchError::Db(e.into())
    }
}

struct Candidate {
    hit: SearchHit,
    best_rowid: i64,
    rank: f64,
}

pub fn search(conn: &Connection, req: &SearchRequest) -> Result<SearchPage, SearchError> {
    let Some(plan) = query_plan::plan(&req.query) else { return Ok(SearchPage::default()) };
    let key = request_key(&(&req.query, req.scope, req.sort, &req.agents, &req.cwd_prefixes, req.since_ms));
    let offset = match &req.cursor {
        None => 0,
        Some(raw) => Cursor::verify(raw, key).map_err(|_| SearchError::InvalidCursor)?.o,
    };
    let mut candidates = Vec::new();
    for rung in &plan.rungs {
        candidates = candidates_for(conn, rung, req)?;
        if !candidates.is_empty() {
            break;
        }
    }
    if candidates.is_empty() {
        return Ok(SearchPage::default());
    }
    match req.sort {
        Sort::Relevance => candidates.sort_by(|a, b| {
            b.rank.total_cmp(&a.rank).then_with(|| b.hit.updated_ms.cmp(&a.hit.updated_ms))
        }),
        Sort::Newest => candidates.sort_by(|a, b| {
            b.hit.updated_ms.cmp(&a.hit.updated_ms).then_with(|| b.rank.total_cmp(&a.rank))
        }),
    }
    let total = candidates.len();
    let limit = match req.limit {
        0 => DEFAULT_LIMIT,
        n => n.min(MAX_LIMIT),
    };
    let end = (offset + limit).min(total);
    let mut hits = Vec::with_capacity(end.saturating_sub(offset));
    for c in candidates.into_iter().skip(offset).take(limit) {
        let mut hit = c.hit;
        hit.snippet = snippet_for(conn, &plan, c.best_rowid, hit.role)?;
        hits.push(hit);
    }
    let next_cursor = (end < total).then(|| Cursor { o: end, k: key }.encode());
    Ok(SearchPage { hits, total, next_cursor })
}

/// Best row per session for one match expression, filtered in SQL.
fn candidates_for(conn: &Connection, expr: &str, req: &SearchRequest) -> Result<Vec<Candidate>, SearchError> {
    // Filters narrow the rows *before* the row cap, so a scoped search for a
    // common word still sees every session in scope.
    let mut filters: Vec<String> = Vec::new();
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(expr.to_string())];
    if req.scope == Scope::Conversation {
        filters.push("m.role != 'tool'".into());
    }
    if !req.agents.is_empty() {
        filters.push(format!("s.agent IN ({})", vec!["?"; req.agents.len()].join(", ")));
        args.extend(req.agents.iter().map(|a| Box::new(a.as_str()) as Box<dyn ToSql>));
    }
    if let Some(since) = req.since_ms {
        filters.push("s.updated_ms >= ?".into());
        args.push(Box::new(since));
    }
    if !req.cwd_prefixes.is_empty() {
        // `/a/b` covers `/a/b` and `/a/b/…` but not `/a/bc`: '0' sorts right
        // after '/', so the range is exactly the paths under `/a/b/`.
        // Each prefix as written and with symlinks resolved, matching the
        // resolved form the index stores.
        let mut keys: Vec<String> =
            req.cwd_prefixes.iter().flat_map(|p| [cwd_key(p), canonical_cwd_key(p)]).collect();
        keys.sort();
        keys.dedup();
        let ors = vec!["(s.cwd_key = ? OR (s.cwd_key >= ? AND s.cwd_key < ?))"; keys.len()];
        filters.push(format!("({})", ors.join(" OR ")));
        for k in keys {
            let base = if k == "/" { String::new() } else { k.clone() };
            args.push(Box::new(k));
            args.push(Box::new(format!("{base}/")));
            args.push(Box::new(format!("{base}0")));
        }
    }
    // A filter is a join inside the CTE, never `rowid IN (subquery)`: FTS5
    // treats that as a list of rowid lookups and re-runs the MATCH per id.
    let scoped = if filters.is_empty() {
        String::new()
    } else {
        format!(
            " JOIN messages m ON m.id = f.rowid JOIN sessions s ON s.id = m.session_row_id WHERE messages_fts MATCH ?1 AND {}",
            filters.join(" AND ")
        )
    };
    let from = if scoped.is_empty() { " WHERE messages_fts MATCH ?1".to_string() } else { scoped };
    // bm25() is unavailable inside an aggregate, so rows are scored in a
    // materialized CTE and grouped outside it. With one max() aggregate,
    // SQLite takes the bare columns (rowid, role) from the max row.
    let sql = format!(
        "WITH hits AS MATERIALIZED (
           SELECT f.rowid AS rid, -bm25(messages_fts, {WEIGHTS}) AS score
           FROM messages_fts f{from}
           ORDER BY score DESC LIMIT {ROW_CAP}
         )
         SELECT m.session_row_id, max(h.score) AS score, h.rid, m.role,
           s.agent, s.session_id, COALESCE(s.custom_title, s.ai_title, s.last_prompt, s.first_prompt, ''),
           s.cwd, s.branch, s.file_path, s.updated_ms, s.message_count
         FROM hits h
         JOIN messages m ON m.id = h.rid
         JOIN sessions s ON s.id = m.session_row_id
         GROUP BY m.session_row_id ORDER BY score DESC LIMIT {CANDIDATE_CAP}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn ToSql> = args.iter().map(|a| a.as_ref()).collect();
    let rows = stmt.query_map(params.as_slice(), |r| {
        let score: f64 = r.get(1)?;
        let message_count: i64 = r.get(11)?;
        let role: String = r.get(3)?;
        let agent: String = r.get(4)?;
        Ok(Candidate {
            best_rowid: r.get(2)?,
            rank: score - LENGTH_DAMPING * (1.0 + message_count.max(0) as f64).ln(),
            hit: SearchHit {
                session_row_id: r.get(0)?,
                agent: Agent::parse(&agent).unwrap_or(Agent::Claude),
                session_id: r.get(5)?,
                title: r.get(6)?,
                cwd: r.get(7)?,
                branch: r.get(8)?,
                file_path: r.get(9)?,
                updated_ms: r.get(10)?,
                message_count,
                role: Role::parse(&role).unwrap_or(Role::Assistant),
                snippet: Vec::new(),
                score,
            },
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The best row's text, highlighted for the query.
fn snippet_for(conn: &Connection, plan: &QueryPlan, rowid: i64, role: Role) -> Result<Vec<Span>, SearchError> {
    let column = match role {
        Role::User => "user_text",
        Role::Assistant => "assistant_text",
        Role::Tool => "tool_text",
    };
    let sql = format!("SELECT {column} FROM messages_fts WHERE rowid = ?1");
    let text: Option<String> = conn.query_row(&sql, [rowid], |r| r.get(0)).ok().flatten();
    Ok(text.map(|t| snippet::build(&t, &plan.terms, plan.prefix_last)).unwrap_or_default())
}
