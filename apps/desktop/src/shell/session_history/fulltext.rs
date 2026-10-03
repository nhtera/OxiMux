//! Full-text mode of the session-history picker: when session search is on
//! and the query has at least two characters, the list shows transcript hits
//! from the local index instead of title matches.
//!
//! Queries run off the UI thread on a fresh read-only connection, debounced
//! and latest-wins: a generation counter drops any result that is no longer
//! the newest, and the superseded query is interrupted in SQLite rather than
//! left to finish. The index can change between pages (an agent is still
//! talking), so an appended page drops sessions the list already shows.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{Context, Task};
use oximux_agents::session_log::session_index::SessionEntry;
use oximux_agents::session_search::{
    Agent, InterruptHandle, SearchError, SearchHit, SearchPage, SearchRequest, Sort, open_reader, search,
};
use oximux_core::AgentAdapter;

use super::SessionHistoryModal;
use super::picker::AgentTypeFilter;
use crate::session_search_service::SessionSearchService;

/// Wait after the last keystroke before querying.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Shortest query that switches the list to transcript hits.
pub const MIN_QUERY_CHARS: usize = 2;
const PAGE_SIZE: usize = 30;

/// Which sessions the picker lists. `Worktree` is offered only when the index
/// is on and a worktree is known; without the index the picker keeps its
/// two-way project / all toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    Worktree,
    Project,
    All,
}

impl HistoryScope {
    pub fn label(self) -> &'static str {
        match self {
            HistoryScope::Worktree => "Worktree",
            HistoryScope::Project => "Project",
            HistoryScope::All => "All",
        }
    }

    /// The ⌃A cycle over the scopes `offered` allows.
    pub fn next(self, offered: &[HistoryScope]) -> HistoryScope {
        let i = offered.iter().position(|s| *s == self).unwrap_or(0);
        offered.get((i + 1) % offered.len().max(1)).copied().unwrap_or(HistoryScope::All)
    }
}

/// An open agent tab, for "jump to open tab" on a hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTab {
    pub project_id: String,
    pub uid: u64,
    /// The agent's own conversation id, once it has reported one.
    pub provider_session: Option<String>,
    pub cwd: String,
    pub title: String,
}

/// What the picker is opened with.
#[derive(Clone, Debug, Default)]
pub struct HistoryContext {
    /// Active project's launch dirs (root + worktrees).
    pub project_paths: Vec<String>,
    /// The worktree the active tab works in.
    pub worktree_path: Option<String>,
    pub live_tabs: Vec<LiveTab>,
}

/// Full-text state held by the modal.
#[derive(Default)]
pub struct FullText {
    /// The index was running when the picker opened.
    pub available: bool,
    /// A pass was reading files when the picker opened.
    pub indexing: bool,
    pub sort: Sort,
    pub hits: Vec<SearchHit>,
    pub total: usize,
    pub next_cursor: Option<String>,
    pub loading: bool,
    /// The query, scope, sort or filter changed and the new first page has
    /// not landed: the rows shown answer the old controls, so they are dimmed
    /// and cannot be opened.
    pub replacing: bool,
    /// The query the shown hits answer.
    pub shown_query: String,
    pub error: Option<String>,
    generation: u64,
    /// The running query's connection, to interrupt when superseded.
    inflight: Option<Arc<Mutex<Option<InterruptHandle>>>>,
    _task: Option<Task<()>>,
}

impl FullText {
    /// Snapshot the service at open.
    pub fn reset(&mut self, cx: &gpui::App) {
        let status = cx.try_global::<SessionSearchService>().and_then(SessionSearchService::live_status);
        *self = FullText { available: status.is_some(), indexing: status.is_some_and(|s| s.indexing), ..Default::default() };
        if let Some(svc) = cx.try_global::<SessionSearchService>() {
            svc.kick();
        }
    }
}

/// The agents a type filter maps to in the index, or `None` for a filter the
/// index does not cover (it holds Claude and Codex transcripts only).
pub fn agents_for(filter: AgentTypeFilter) -> Option<Vec<Agent>> {
    match filter {
        AgentTypeFilter::All => Some(Vec::new()),
        AgentTypeFilter::Claude => Some(vec![Agent::Claude]),
        AgentTypeFilter::Codex => Some(vec![Agent::Codex]),
        _ => None,
    }
}

/// cwd prefixes for a scope.
pub fn scope_prefixes(scope: HistoryScope, ctx: &HistoryContext) -> Vec<String> {
    match scope {
        HistoryScope::Worktree => ctx.worktree_path.iter().cloned().collect(),
        HistoryScope::Project => ctx.project_paths.clone(),
        HistoryScope::All => Vec::new(),
    }
}

/// A hit as the history entry resume / fork / open-as-chat act on.
pub fn hit_entry(hit: &SearchHit) -> SessionEntry {
    SessionEntry {
        session_id: hit.session_id.clone(),
        adapter: match hit.agent {
            Agent::Claude => AgentAdapter::ClaudeCode,
            Agent::Codex => AgentAdapter::Codex,
        },
        preset_id: None,
        path: Some(hit.file_path.clone()),
        cwd: hit.cwd.clone(),
        title: (!hit.title.is_empty()).then(|| hit.title.clone()),
        custom_title: None,
        git_branch: hit.branch.clone(),
        tag: None,
        created_at_ms: None,
        last_message_ts_ms: hit.updated_ms,
        message_count: usize::try_from(hit.message_count).ok(),
        size_bytes: None,
        entry_count: None,
    }
}

/// The open tab already running this hit's session, matched by the
/// conversation id its agent reported. A tab that has not reported one yet is
/// never guessed at by directory or title: two agents (or two sessions) can
/// share both, and a wrong jump hides the session the user picked.
pub fn live_tab_for<'a>(hit: &SearchHit, tabs: &'a [LiveTab]) -> Option<&'a LiveTab> {
    tabs.iter().find(|t| t.provider_session.as_deref() == Some(hit.session_id.as_str()))
}

impl SessionHistoryModal {
    /// Whether the list currently shows transcript hits.
    /// Off for agent filters the index does not cover (those keep title
    /// matching).
    pub(super) fn fulltext_active(&self) -> bool {
        self.fulltext.available
            && self.query.trim().chars().count() >= MIN_QUERY_CHARS
            && agents_for(self.type_filter).is_some()
    }

    fn fulltext_request(&self, cursor: Option<String>) -> Option<SearchRequest> {
        Some(SearchRequest {
            query: self.query.trim().to_string(),
            sort: self.fulltext.sort,
            agents: agents_for(self.type_filter)?,
            cwd_prefixes: scope_prefixes(self.scope, &self.context),
            limit: PAGE_SIZE,
            cursor,
            ..Default::default()
        })
    }

    /// Re-run the query (debounced) after the query, scope, sort or type
    /// filter changed. Clears the hits when full-text mode is off.
    pub(super) fn schedule_fulltext(&mut self, cx: &mut Context<Self>) {
        self.supersede_fulltext();
        if !self.fulltext_active() {
            self.fulltext.hits.clear();
            self.fulltext.total = 0;
            self.fulltext.next_cursor = None;
            self.fulltext.loading = false;
            self.fulltext._task = None;
            return;
        }
        let Some(req) = self.fulltext_request(None) else {
            self.fulltext.hits.clear();
            self.fulltext.total = 0;
            self.fulltext.next_cursor = None;
            self.fulltext.loading = false;
            return;
        };
        self.fulltext.loading = true;
        self.fulltext.replacing = !self.fulltext.hits.is_empty();
        self.run_fulltext(req, false, Some(DEBOUNCE), cx);
    }

    /// Fetch the next page onto the current hits.
    pub(super) fn load_more_fulltext(&mut self, cx: &mut Context<Self>) {
        // A newer query is still debouncing: its first page replaces these.
        if self.fulltext.loading || self.query.trim() != self.fulltext.shown_query {
            return;
        }
        let Some(cursor) = self.fulltext.next_cursor.clone() else { return };
        let Some(req) = self.fulltext_request(Some(cursor)) else { return };
        self.supersede_fulltext();
        self.fulltext.loading = true;
        self.run_fulltext(req, true, None, cx);
        cx.notify();
    }

    /// Retire the running query: newer results win, and its SQLite work stops.
    fn supersede_fulltext(&mut self) {
        self.fulltext.generation = self.fulltext.generation.wrapping_add(1);
        if let Some(slot) = self.fulltext.inflight.take()
            && let Some(handle) = slot.lock().ok().and_then(|mut h| h.take())
        {
            handle.interrupt();
        }
    }

    fn run_fulltext(&mut self, req: SearchRequest, append: bool, delay: Option<Duration>, cx: &mut Context<Self>) {
        let generation = self.fulltext.generation;
        let slot: Arc<Mutex<Option<InterruptHandle>>> = Arc::default();
        self.fulltext.inflight = Some(slot.clone());
        let Some(path) = cx
            .try_global::<SessionSearchService>()
            .and_then(|s| s.db_path().map(|p| p.to_path_buf()))
        else {
            return;
        };
        self.fulltext._task = Some(cx.spawn(async move |this, cx| {
            if let Some(d) = delay {
                cx.background_executor().timer(d).await;
            }
            if this.read_with(cx, |m, _| m.fulltext.generation != generation).unwrap_or(true) {
                return;
            }
            let query = req.query.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let conn = open_reader(&path).map_err(SearchError::Db)?;
                    if let Ok(mut h) = slot.lock() {
                        *h = Some(conn.get_interrupt_handle());
                    }
                    search(&conn, &req)
                })
                .await;
            let _ = this.update(cx, |m, cx| {
                if m.fulltext.generation != generation {
                    return;
                }
                m.fulltext.loading = false;
                m.fulltext.replacing = false;
                match result {
                    Ok(page) => m.apply_page(page, append, query),
                    Err(e) => {
                        // A failed first page must not leave the old rows
                        // posing as its answer.
                        if !append {
                            m.fulltext.hits.clear();
                            m.fulltext.total = 0;
                            m.fulltext.next_cursor = None;
                        }
                        m.fulltext.error = Some(e.to_string());
                    }
                }
                cx.notify();
            });
        }));
    }

    fn apply_page(&mut self, page: SearchPage, append: bool, query: String) {
        let ft = &mut self.fulltext;
        if append {
            let shown: std::collections::HashSet<i64> = ft.hits.iter().map(|h| h.session_row_id).collect();
            ft.hits.extend(page.hits.into_iter().filter(|h| !shown.contains(&h.session_row_id)));
        } else {
            ft.hits = page.hits;
            self.selected_idx = 0;
        }
        ft.total = page.total;
        ft.next_cursor = page.next_cursor;
        ft.shown_query = query;
        ft.error = None;
    }

    pub(super) fn set_fulltext_sort(&mut self, sort: Sort, cx: &mut Context<Self>) {
        if self.fulltext.sort != sort {
            self.fulltext.sort = sort;
            self.schedule_fulltext(cx);
            cx.notify();
        }
    }

    /// Scopes ⌃A cycles through right now.
    pub(super) fn offered_scopes(&self) -> Vec<HistoryScope> {
        let mut out = Vec::new();
        if self.fulltext.available && self.context.worktree_path.is_some() {
            out.push(HistoryScope::Worktree);
        }
        if !self.context.project_paths.is_empty() {
            out.push(HistoryScope::Project);
        }
        out.push(HistoryScope::All);
        out
    }

    /// The selected row as a history entry, in either mode.
    pub(super) fn selected_entry(&self) -> Option<SessionEntry> {
        if self.fulltext_active() {
            if self.fulltext.replacing {
                return None;
            }
            return self.fulltext.hits.get(self.selected_idx).map(hit_entry);
        }
        let order = self.filtered();
        order.get(self.selected_idx).and_then(|&i| self.entries.get(i)).cloned()
    }

    /// The open tab running the selected hit's session, if any.
    pub(super) fn selected_live_tab(&self) -> Option<LiveTab> {
        if !self.fulltext_active() || self.fulltext.replacing {
            return None;
        }
        let hit = self.fulltext.hits.get(self.selected_idx)?;
        live_tab_for(hit, &self.context.live_tabs).cloned()
    }

    /// Rows in the list right now (hits, or filtered title entries).
    pub(super) fn row_count(&self) -> usize {
        if self.fulltext_active() { self.fulltext.hits.len() } else { self.filtered().len() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(session_id: &str, cwd: &str, title: &str) -> SearchHit {
        SearchHit {
            session_row_id: 1,
            agent: Agent::Codex,
            session_id: session_id.into(),
            title: title.into(),
            cwd: Some(cwd.into()),
            branch: Some("main".into()),
            file_path: "/h/.codex/sessions/r.jsonl".into(),
            updated_ms: Some(5),
            message_count: 12,
            role: oximux_agents::session_search::Role::Tool,
            snippet: Vec::new(),
            score: 1.0,
        }
    }

    fn tab(uid: u64, session: Option<&str>, cwd: &str, title: &str) -> LiveTab {
        LiveTab {
            project_id: "p".into(),
            uid,
            provider_session: session.map(str::to_string),
            cwd: cwd.into(),
            title: title.into(),
        }
    }

    #[test]
    fn scope_maps_to_cwd_prefixes() {
        let ctx = HistoryContext {
            project_paths: vec!["/r".into(), "/r-wt".into()],
            worktree_path: Some("/r-wt".into()),
            live_tabs: Vec::new(),
        };
        assert_eq!(scope_prefixes(HistoryScope::Worktree, &ctx), ["/r-wt"]);
        assert_eq!(scope_prefixes(HistoryScope::Project, &ctx), ["/r", "/r-wt"]);
        assert!(scope_prefixes(HistoryScope::All, &ctx).is_empty());
    }

    #[test]
    fn scope_cycle_wraps_over_offered() {
        let all = [HistoryScope::Worktree, HistoryScope::Project, HistoryScope::All];
        assert_eq!(HistoryScope::Worktree.next(&all), HistoryScope::Project);
        assert_eq!(HistoryScope::All.next(&all), HistoryScope::Worktree);
        let two = [HistoryScope::Project, HistoryScope::All];
        assert_eq!(HistoryScope::All.next(&two), HistoryScope::Project);
        assert_eq!(HistoryScope::Worktree.next(&two), HistoryScope::All);
    }

    #[test]
    fn hits_map_to_resumable_entries() {
        let e = hit_entry(&hit("019f-uuid", "/r", "Rename billing"));
        assert_eq!(e.adapter, AgentAdapter::Codex);
        assert_eq!((e.session_id.as_str(), e.cwd.as_deref()), ("019f-uuid", Some("/r")));
        assert_eq!(e.path.as_deref(), Some("/h/.codex/sessions/r.jsonl"));
        assert_eq!(e.message_count, Some(12));
        let claude = hit_entry(&SearchHit { agent: Agent::Claude, ..hit("abc", "/r", "") });
        assert_eq!(claude.adapter, AgentAdapter::ClaudeCode);
        assert!(claude.title.is_none());
    }

    #[test]
    fn only_indexed_agents_are_searchable() {
        assert_eq!(agents_for(AgentTypeFilter::All), Some(vec![]));
        assert_eq!(agents_for(AgentTypeFilter::Claude), Some(vec![Agent::Claude]));
        assert_eq!(agents_for(AgentTypeFilter::OpenCode), None);
    }

    #[test]
    fn live_tab_matches_only_by_reported_session() {
        let h = hit("s1", "/r", "Fix login");
        let by_id = [tab(1, Some("other"), "/r", "Fix login"), tab(2, Some("s1"), "/x", "y")];
        assert_eq!(live_tab_for(&h, &by_id).map(|t| t.uid), Some(2));
        // Same directory and title, no reported id: not a match.
        assert!(live_tab_for(&h, &[tab(3, None, "/r", "Fix login")]).is_none());
        assert!(live_tab_for(&h, &[]).is_none());
    }
}
