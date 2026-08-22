//! Tasks page — rendered when the Tasks pane tab is active.
//!
//! A GitHub/GitLab issue/PR browser. Lists across a chosen **scope** — every
//! known project at once ([`TaskScope::All`], the default) or a single one —
//! so it works like a global inbox rather than being pinned to the active
//! project. Owns its own async fetch + filter state (scope, kind, state,
//! assigned-to-me, query) so the list survives re-renders; data is fetched
//! through the [`ForgeProvider`] seam (never `gh` directly), one concurrent
//! call per in-scope project. Mounted by the pane system as an
//! `Entity<TasksView>`; row actions (create workspace, open in browser)
//! dispatch up via `weak_root`.
//!
//! Layout (top → bottom):
//!   1. Toolbar    — `table::render_toolbar` (scope picker + kind/state/query)
//!   2. Col-header — `table::render_col_header`
//!   3. Body       — virtualized `uniform_list` of `row::render_task_row`

mod detail;
pub mod row;
mod scope_picker;
mod table;

use std::path::PathBuf;
use std::time::Duration;

use futures::future::join_all;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    div, px, uniform_list,
};
use gpui_component::input::{InputEvent, InputState};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::{
    AuthState, Forge, ForgeItem, ForgeListFilter, ForgeProvider, ForgeState, ItemDetail,
    fetch_item_detail,
};
use crate::shell::tasks_view::row::render_task_row;
use crate::shell::tasks_view::table::{render_col_header, render_toolbar};
use crate::workspace_root::WorkspaceRoot;

/// Debounce window for the query box — a key per ~300ms instead of per
/// keystroke, matching the repo's search-panel idiom.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(300);

/// Whether the page is listing issues or pull requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Issues,
    Prs,
}

/// Which projects the listing spans. [`TaskScope::All`] fans the query out over
/// every known project (the default — a global issue/PR inbox); [`TaskScope::One`]
/// narrows to a single project by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TaskScope {
    All,
    One(String),
}

/// One listed issue/PR paired with the project it belongs to. The pairing is
/// essential under [`TaskScope::All`]: each row's `+ Workspace` / detail action
/// must run against *its own* project's working tree, not a single active one.
#[derive(Clone)]
pub(super) struct TaskRow {
    pub(super) project: Project,
    pub(super) item: ForgeItem,
}

/// Identifies what a given `items` snapshot was fetched for, so re-activating
/// the page with the same scope + filter doesn't re-hit the network. The scope
/// key (`"*all*"` or a project id) and the query string are part of the key so
/// a nav toggle never replays stale rows from a different scope/search. A
/// change to the project *set* is handled separately (it nulls `loaded_key`).
type FetchKey = (String, TaskKind, ForgeState, bool, Option<String>);

/// One fetch's result: the rows plus the context needed to pick the right
/// empty-state copy (was a forge even detected, and is its CLI authenticated).
#[derive(Default)]
struct FetchOutcome {
    items: Vec<TaskRow>,
    /// Whether any in-scope project's `origin` resolved to a supported forge.
    forge_detected: bool,
    /// Auth state of the forge CLI. Only probed (and meaningful) for a
    /// single-project scope; `Ok` for the aggregate view.
    auth: AuthState,
}

pub struct TasksView {
    weak_root: WeakEntity<WorkspaceRoot>,
    /// Focus handle so `PaneContent::Tasks` can satisfy `Focusable` and forward
    /// focus correctly. The page's only text input (the query box) carries its
    /// own focus; this handle anchors the pane itself.
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Every known project, so the scope picker can offer them and
    /// [`TaskScope::All`] can fan out. Refreshed via [`TasksView::set_projects`].
    projects: Vec<Project>,
    /// Which project(s) the listing spans. Defaults to [`TaskScope::All`].
    scope: TaskScope,
    kind: TaskKind,
    filter: ForgeListFilter,
    /// GitHub-search query box. Its `Change`/`Enter` events drive
    /// [`TasksView::set_search`] (debounced) and refine the listing.
    query_input: Entity<InputState>,
    items: Vec<TaskRow>,
    /// Whether the last fetch found a supported forge for the scope's repo(s).
    /// Drives the "no GitHub/GitLab remote" hint vs the authenticated states.
    forge_detected: bool,
    /// Auth state from the last single-project fetch's `gh auth status` probe
    /// (cached here so the hint branches without re-probing per render).
    auth: AuthState,
    loading: bool,
    loaded_key: Option<FetchKey>,
    /// Issue/PR currently shown in the in-pane detail view, or `None` for the
    /// list. Holds the row (project + [`ForgeItem`]) so the detail renders its
    /// metadata immediately while the body loads and acts on the right project.
    selected: Option<TaskRow>,
    /// Lazily-fetched body + author for [`Self::selected`].
    detail: Option<ItemDetail>,
    detail_loading: bool,
    /// Generation guard for the detail fetch (same role as `fetch_gen`): a slow
    /// body that resolves after the user opened a different row is discarded.
    detail_gen: u64,
    _detail_task: Option<Task<()>>,
    /// Monotonic fetch id. Each fetch captures the current value; a result only
    /// applies if it still matches, so a slow query that resolves after a newer
    /// one is discarded instead of clobbering fresher rows.
    fetch_gen: u64,
    scroll: UniformListScrollHandle,
    /// Pending debounce timer for the query box (dropped — and thus cancelled —
    /// when a newer keystroke schedules another).
    debounce_task: Option<Task<()>>,
    _fetch_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TasksView {
    pub fn new(
        weak_root: WeakEntity<WorkspaceRoot>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter, e.g. is:open label:bug")
        });
        let subs = vec![cx.subscribe_in(
            &query_input,
            window,
            |me, _, ev: &InputEvent, window, cx| me.on_query_event(ev, window, cx),
        )];
        Self {
            weak_root,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            projects: Vec::new(),
            scope: TaskScope::All,
            kind: TaskKind::Issues,
            filter: ForgeListFilter::default(),
            query_input,
            items: Vec::new(),
            forge_detected: false,
            auth: AuthState::Ok,
            loading: false,
            loaded_key: None,
            selected: None,
            detail: None,
            detail_loading: false,
            detail_gen: 0,
            _detail_task: None,
            fetch_gen: 0,
            scroll: UniformListScrollHandle::new(),
            debounce_task: None,
            _fetch_task: None,
            _subscriptions: subs,
        }
    }

    /// Push the known-project set. Stores it without fetching — the pane system
    /// calls [`TasksView::activate`]/[`TasksView::refresh`] to drive the
    /// network. A changed set invalidates the cache (a newly-added project
    /// should appear in the `All` view) and drops a scope pinned to a project
    /// that no longer exists back to `All`.
    pub fn set_projects(&mut self, projects: Vec<Project>, cx: &mut Context<Self>) {
        let changed = self
            .projects
            .iter()
            .map(|p| &p.id)
            .ne(projects.iter().map(|p| &p.id));
        self.projects = projects;
        if let TaskScope::One(id) = &self.scope
            && !self.projects.iter().any(|p| &p.id == id)
        {
            self.scope = TaskScope::All;
        }
        if changed {
            self.loaded_key = None;
            cx.notify();
        }
    }

    /// Called when the page becomes visible. Fetches only when the current
    /// scope + filter hasn't already been loaded (cheap nav toggling).
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        if self.loaded_key.is_some() && self.loaded_key.as_ref() == Some(&self.current_key()) {
            return;
        }
        self.fetch(cx);
    }

    /// Force a re-fetch (Refresh button / live project switch).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loaded_key = None;
        self.fetch(cx);
    }

    fn current_key(&self) -> FetchKey {
        (
            self.scope_key(),
            self.kind,
            self.filter.state,
            self.filter.mine,
            self.filter.search.clone(),
        )
    }

    /// Stable string for the active scope — `"*all*"` for the aggregate view
    /// (a sentinel no project id can collide with) or the project id.
    fn scope_key(&self) -> String {
        match &self.scope {
            TaskScope::All => "*all*".to_string(),
            TaskScope::One(id) => id.clone(),
        }
    }

    /// Switch the listing scope (picker selection). Returns to the list view and
    /// refetches.
    pub(super) fn set_scope(&mut self, scope: TaskScope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        // Leaving the detail view: a selected row may belong to a project the
        // new scope no longer shows.
        self.selected = None;
        self.detail = None;
        self.detail_loading = false;
        self.detail_gen = self.detail_gen.wrapping_add(1);
        self.fetch(cx);
    }

    pub(super) fn scope(&self) -> &TaskScope {
        &self.scope
    }

    pub(super) fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub(super) fn set_kind(&mut self, kind: TaskKind, cx: &mut Context<Self>) {
        if self.kind != kind {
            self.kind = kind;
            self.fetch(cx);
        }
    }

    pub(super) fn set_state(&mut self, state: ForgeState, cx: &mut Context<Self>) {
        if self.filter.state != state {
            self.filter.state = state;
            self.fetch(cx);
        }
    }

    pub(super) fn toggle_mine(&mut self, cx: &mut Context<Self>) {
        self.filter.mine = !self.filter.mine;
        self.fetch(cx);
    }

    /// True when a non-blank query is committed — drives the clear (×)
    /// affordance. Reads the committed filter (kept in lockstep with the input
    /// by `commit_query`) rather than re-reading the input entity per frame.
    pub(super) fn has_query(&self) -> bool {
        self.filter.search.is_some()
    }

    /// Query-box event hook. `Change` debounces a refetch; `Enter` fetches
    /// immediately (no wait).
    fn on_query_event(&mut self, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            InputEvent::Change => {
                self.set_search(window, cx);
                // Re-render so the typed text and the clear (×) affordance
                // reflect the new value (the input is a child of our element).
                cx.notify();
            }
            InputEvent::PressEnter { .. } => {
                // Fetch now; `fetch` drops the pending debounce so it can't
                // refire.
                self.commit_query(cx);
                self.fetch(cx);
            }
            _ => {}
        }
    }

    /// Pull the live input value into `filter.search` (`None` when blank so the
    /// fetch takes the plain flag path), returning whether it actually changed.
    fn commit_query(&mut self, cx: &mut Context<Self>) -> bool {
        let raw = self.query_input.read(cx).value().trim().to_string();
        let next = (!raw.is_empty()).then_some(raw);
        if self.filter.search == next {
            return false;
        }
        self.filter.search = next;
        true
    }

    /// Debounced refetch from a query keystroke: commit the text, then schedule
    /// a fetch ~300ms out. A newer keystroke drops this pending task, so only
    /// the last pause in typing actually hits the network.
    fn set_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.commit_query(cx) {
            return;
        }
        self.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(QUERY_DEBOUNCE).await;
            let _ = this.update(cx, |tv, cx| tv.fetch(cx));
        }));
    }

    /// Clear the query box (× button) and refetch the unfiltered listing.
    pub(super) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.debounce_task = None;
        if self.commit_query(cx) {
            self.fetch(cx);
        }
    }

    /// Open the in-pane detail view for one row, fetching its body + author
    /// lazily against the row's own project. The row's metadata renders
    /// immediately; the body streams in.
    pub(super) fn open_detail(&mut self, row: TaskRow, cx: &mut Context<Self>) {
        let project = row.project.clone();
        let number = row.item.number;
        self.selected = Some(row);
        self.detail = None;
        self.detail_loading = true;
        self.detail_gen = self.detail_gen.wrapping_add(1);
        let my_gen = self.detail_gen;
        cx.notify();

        let cwd = PathBuf::from(&project.root_path);
        let kind = match self.kind {
            TaskKind::Issues => oximux_core::ForgeRefKind::Issue,
            TaskKind::Prs => oximux_core::ForgeRefKind::Pull,
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<ItemDetail>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let detail = match Forge::detect(&cwd).await {
                        Some(forge) => fetch_item_detail(forge, &cwd, kind, number).await,
                        None => None,
                    };
                    let _ = tx.send(detail);
                });
            }
            Err(_) => {
                self.detail_loading = false;
                cx.notify();
                return;
            }
        }
        self._detail_task = Some(cx.spawn(async move |this, cx| {
            let detail = rx.await.unwrap_or(None);
            let _ = this.update(cx, |tv, cx| {
                // Discard a body the user already navigated away from.
                if tv.detail_gen != my_gen {
                    return;
                }
                tv.detail = detail;
                tv.detail_loading = false;
                cx.notify();
            });
        }));
    }

    /// Return from the detail view to the list.
    pub(super) fn close_detail(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_none() {
            return;
        }
        self.selected = None;
        self.detail = None;
        self.detail_loading = false;
        // Invalidate any in-flight body fetch so it can't ghost-write `detail`
        // (and fire a spurious notify) after we've returned to the list.
        self.detail_gen = self.detail_gen.wrapping_add(1);
        cx.notify();
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        // This fetch supersedes any pending debounce — drop it so a chip click
        // (or Enter) can't leave an orphaned timer that refires a redundant
        // fetch ~300ms later (and flashes the loading state).
        self.debounce_task = None;

        // Resolve the scope to the concrete projects to query.
        let targets: Vec<Project> = match &self.scope {
            TaskScope::All => self.projects.clone(),
            TaskScope::One(id) => self
                .projects
                .iter()
                .filter(|p| &p.id == id)
                .cloned()
                .collect(),
        };
        if targets.is_empty() {
            self.items.clear();
            self.loading = false;
            self.loaded_key = None;
            self.forge_detected = false;
            self.auth = AuthState::Ok;
            cx.notify();
            return;
        }

        self.loaded_key = Some(self.current_key());
        self.loading = true;
        // Drop the previous result so a scope/kind/state/Mine switch shows the
        // loading state instead of stale rows from the old query.
        self.items.clear();
        cx.notify();

        // Bump the generation so a slower in-flight fetch (e.g. an earlier
        // query keystroke) can't apply its result over this newer one.
        self.fetch_gen = self.fetch_gen.wrapping_add(1);
        let my_gen = self.fetch_gen;

        let kind = self.kind;
        let filter = self.filter.clone();
        // The per-project auth probe is only meaningful (and surfaced) when the
        // scope is a single project — the aggregate view skips it.
        let probe_auth = matches!(self.scope, TaskScope::One(_));
        let (tx, rx) = tokio::sync::oneshot::channel::<FetchOutcome>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    // One concurrent forge call per in-scope project. A project
                    // whose `origin` isn't a supported forge (or that has no
                    // remote at all) contributes nothing and is skipped — the
                    // aggregate degrades gracefully exactly like the rest of the
                    // page does on a forge-CLI failure.
                    let per_project = targets.into_iter().map(|project| {
                        let filter = filter.clone();
                        async move {
                            let cwd = PathBuf::from(&project.root_path);
                            match Forge::detect(&cwd).await {
                                Some(forge) => {
                                    let list = async {
                                        match kind {
                                            TaskKind::Issues => {
                                                forge.list_issues(&cwd, filter).await
                                            }
                                            TaskKind::Prs => forge.list_prs(&cwd, filter).await,
                                        }
                                    };
                                    // For a single-project scope the auth probe
                                    // and the listing are independent reads, so
                                    // run them concurrently (no stacked timeout).
                                    let (auth, items) = if probe_auth {
                                        tokio::join!(forge.auth_state(&cwd), list)
                                    } else {
                                        (AuthState::Ok, list.await)
                                    };
                                    (true, auth, project, items)
                                }
                                None => (false, AuthState::Ok, project, Vec::new()),
                            }
                        }
                    });
                    let results = join_all(per_project).await;

                    let mut rows: Vec<TaskRow> = Vec::new();
                    let mut any_detected = false;
                    let mut probed_auth = AuthState::Ok;
                    for (detected, auth, project, items) in results {
                        any_detected |= detected;
                        if probe_auth {
                            probed_auth = auth;
                        }
                        for item in items {
                            rows.push(TaskRow {
                                project: project.clone(),
                                item,
                            });
                        }
                    }
                    // Newest-first. RFC-3339 timestamps sort chronologically as
                    // plain strings; a missing/empty stamp sinks to the bottom.
                    rows.sort_by(|a, b| b.item.updated_at.cmp(&a.item.updated_at));

                    let _ = tx.send(FetchOutcome {
                        items: rows,
                        forge_detected: any_detected,
                        auth: probed_auth,
                    });
                });
            }
            Err(_) => {
                // No tokio runtime (headless test) — leave items as-is, clear
                // the spinner so the page doesn't hang.
                self.loading = false;
                cx.notify();
                return;
            }
        }
        self._fetch_task = Some(cx.spawn(async move |this, cx| {
            let outcome = rx.await.unwrap_or_default();
            let _ = this.update(cx, |tv, cx| {
                // Discard a result that a newer fetch has already superseded.
                if tv.fetch_gen != my_gen {
                    return;
                }
                tv.items = outcome.items;
                tv.forge_detected = outcome.forge_detected;
                tv.auth = outcome.auth;
                tv.loading = false;
                cx.notify();
            });
        }));
    }

    fn render_body(&self, weak_tasks: WeakEntity<TasksView>) -> AnyElement {
        let theme = self.theme;
        if self.projects.is_empty() {
            return self.hint("Open a project to browse its issues.");
        }
        if self.loading && self.items.is_empty() {
            return self.hint("Loading\u{2026}");
        }
        if self.items.is_empty() {
            return self.hint(&self.empty_hint());
        }

        let items = self.items.clone();
        let density = self.density;
        let typography = self.typography.clone();
        let kind = self.kind;
        let weak_root = self.weak_root.clone();
        // Under the aggregate scope each row tags which project it came from.
        let show_project = matches!(self.scope, TaskScope::All);
        // One clock read per render (not per row) for the relative `Updated`
        // column; moved into the list closure and borrowed by each row.
        let now = chrono::Utc::now().to_rfc3339();
        let row_count = items.len();
        let list = uniform_list(
            "tasks-rows",
            row_count,
            move |range: std::ops::Range<usize>, _window, _cx| {
                range
                    .filter_map(|i| items.get(i).cloned())
                    .map(|row| {
                        render_task_row(
                            &row.item,
                            kind,
                            weak_tasks.clone(),
                            weak_root.clone(),
                            row.project.clone(),
                            show_project,
                            &now,
                            theme,
                            density,
                            &typography,
                        )
                    })
                    .collect::<Vec<AnyElement>>()
            },
        )
        .track_scroll(&self.scroll)
        .h_full();
        div().flex_1().w_full().child(list).into_any_element()
    }

    /// Copy for the empty list. For a single-project scope it disambiguates the
    /// failure modes the old single string conflated (no supported remote, an
    /// unauthenticated CLI, or a genuinely empty repo). For the aggregate scope
    /// the per-project auth nuance doesn't apply, so it stays generic.
    fn empty_hint(&self) -> String {
        let what = match self.kind {
            TaskKind::Issues => "issues",
            TaskKind::Prs => "pull requests",
        };
        let scope = if self.filter.state == ForgeState::Open {
            "open "
        } else {
            ""
        };
        if matches!(self.scope, TaskScope::All) {
            if !self.forge_detected {
                return "No GitHub or GitLab remotes across your projects.".to_string();
            }
            return format!("No {scope}{what} across your projects.");
        }
        if !self.forge_detected {
            return "No GitHub or GitLab remote for this repo.".to_string();
        }
        match self.auth {
            // Distinct remedies: an unauthenticated CLI needs a login; an
            // absent binary can't run `gh auth login` at all and needs install
            // first — collapsing them would point the user at the wrong step.
            AuthState::NotAuthed => format!("Sign in with `gh auth login` to see {what}."),
            AuthState::GhMissing => format!("Install the `gh` CLI and sign in to see {what}."),
            AuthState::Ok => format!("No {scope}{what}."),
        }
    }

    fn hint(&self, text: &str) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .w_full()
            .p(px(self.density.pad_panel))
            .text_size(px(self.typography.t_body_sm))
            .text_color(self.theme.fg_subtle)
            .child(text.to_string())
            .into_any_element()
    }
}

impl Focusable for TasksView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TasksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
        // Tasks sits on the content canvas (`bg_panel`), not the rail surface.
        let root = div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_panel);
        // A selected row swaps the whole pane for the issue/PR detail view.
        if self.selected.is_some() {
            return root.child(detail::render_detail(self, cx));
        }
        let has_query = self.has_query();
        let toolbar = render_toolbar(
            self.kind,
            &self.filter,
            self.scope(),
            self.projects(),
            &self.query_input,
            has_query,
            self.theme,
            self.density,
            &self.typography,
            cx,
        );
        let col_header = render_col_header(self.theme, self.density, &self.typography);
        let weak_tasks = cx.entity().downgrade();
        let body = self.render_body(weak_tasks);
        root.child(toolbar).child(col_header).child(body)
    }
}
