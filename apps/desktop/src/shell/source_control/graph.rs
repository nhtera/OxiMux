//! Commit-graph panel (flat recent commits list, no DAG drawing for v1).
//!
//! Loads `Repository::log_recent(20)` on mount; "Load more" extends in
//! 20-row chunks. State machine: `Loading → Ready | Failed`.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled, Task,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants},
};
use oximux_core::CommitInfo;
use oximux_git::{GitError, Repository};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;
use tokio::sync::oneshot;

use crate::scm_layout_settings;
use crate::shell::source_control::graph_layout::{RowLayout, compute_graph, max_lanes};
use crate::shell::source_control::graph_row::render_commit_row;
use crate::shell::source_control::style::ScmStyle;

const PAGE_SIZE: u32 = 20;

/// Emitted by `CommitGraph` when the user clicks a commit row. The
/// host (workspace) subscribes and opens a commit-detail tab pinned
/// to the SHA.
#[derive(Debug, Clone)]
pub struct ShowCommitRequested {
    pub sha: String,
    /// Short label for the detail tab title — typically the 7-char
    /// short OID. Captured at click time so the host doesn't have to
    /// re-walk `CommitGraph` state.
    pub short_oid: String,
    /// Single-line subject for the tab title; truncated by the tab
    /// strip if too long.
    pub subject: String,
}

/// Drag payload tagging a graph-height resize. Empty — the new height is
/// derived per-tick from the cursor position against the anchor captured
/// in `CommitGraph` at drag start. Exists only so the matching
/// `on_drag_move` listener on the Source Control panel can type-select
/// these ticks apart from other drags (sidebar/rail width resizes).
#[derive(Debug, Clone)]
pub struct GraphResizePayload;

/// Zero-size drag preview. GPUI's `on_drag` must return an entity to render
/// the floating cursor preview; an edge resize-handle has none (the section
/// reflows live each tick), so this renders nothing.
pub struct GraphResizeGhost;

impl Render for GraphResizeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

#[derive(Debug, Clone)]
enum GraphState {
    Loading,
    Ready {
        commits: Vec<CommitInfo>,
        can_load_more: bool,
        loading_more: bool,
        /// Stale-while-revalidate flag: a page-0 fetch is in-flight but the
        /// previously loaded `commits` are still rendered. Lets the list,
        /// count badge, and load-more link stay visible while the refresh
        /// resolves, so the section never collapses to a placeholder and
        /// then re-expands under the user.
        refreshing: bool,
    },
    Failed(String),
}

/// Two-stage payload from the initial-load tokio task. The commit page
/// arrives first so the list paints immediately; the per-commit stats map
/// follows once its concurrent numstat fetch completes and fills the hover
/// tooltip cache under the already-visible list.
enum InitialLoad {
    Commits(Result<Vec<CommitInfo>, GitError>),
    Stats(HashMap<String, (u32, u32)>),
}

pub struct CommitGraph {
    repo: Repository,
    state: GraphState,
    collapsed: bool,
    scroll: UniformListScrollHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    _load_task: Option<Task<()>>,
    // ----- Phase 13: keyboard-resizable section height -----
    /// Live body height of the commit-list area. Pre-Phase-13 this was
    /// `px(240.0)` hard-coded — now state, clamped via
    /// `scm_layout_settings::clamp_graph_height` on every mutation.
    graph_height: Pixels,
    /// Global key/value settings store. When present, every mutation
    /// to `graph_height` writes back via `save_graph_height` so the
    /// chosen size survives a restart; `None` is the test wiring
    /// (in-memory only).
    settings_repo: Option<SettingsRepo>,
    /// Focusable rail rendered at the bottom of the body so the user
    /// can Tab to it and resize via Arrow / Shift+Arrow / Home / End.
    focus_handle: FocusHandle,
    /// Cached `(added, removed)` line counts per commit OID, populated
    /// inline by the same tokio task that loads each page (so commits
    /// and their stats arrive together on the first render). The
    /// hover tooltip reads from here to surface the +N/−N line below
    /// the commit body; absent entries (root commits, transient git
    /// failures) silently render without the stats slot. Replaced
    /// wholesale on a refresh / fresh `spawn_load_initial`; extended
    /// in place on `load_more` so prior pages keep their stats.
    stat_cache: HashMap<String, (u32, u32)>,
    /// Precomputed swimlane drawing model, 1:1 with the loaded commits.
    /// Recomputed whenever the commit set changes (initial load, refresh,
    /// load-more) rather than per render, since the layout is a function of
    /// the whole loaded window. `Rc` so the `uniform_list` row closure can
    /// hold a cheap clone and index it per visible row.
    graph_layout: Rc<Vec<RowLayout>>,
    /// Widest lane count across `graph_layout`, clamped to the lane cap.
    /// Drives the (fixed) gutter width so commit subjects stay aligned.
    graph_max_lanes: usize,
    /// `true` while a mouse drag-resize is in flight — keeps the top
    /// handle's highlight lit (hover styles are suppressed mid-drag) and is
    /// cleared on the first render after the drag ends.
    resizing: bool,
    /// Drag-resize anchor: `(height_at_drag_start, cursor_y_at_first_tick)`.
    /// The cursor-y is captured lazily on the first move tick (drag-start
    /// can't read the pointer), after which each tick maps the cursor delta
    /// to a height delta so the handle stays glued to the pointer.
    drag_anchor: Option<(f32, Option<f32>)>,
}

impl CommitGraph {
    pub fn new(
        repo: Repository,
        initial_height: Pixels,
        settings_repo: Option<SettingsRepo>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut graph = Self {
            repo,
            state: GraphState::Loading,
            collapsed: false,
            scroll: UniformListScrollHandle::new(),
            theme,
            density,
            typography,
            _load_task: None,
            graph_height: initial_height,
            settings_repo,
            focus_handle,
            stat_cache: HashMap::new(),
            graph_layout: Rc::new(Vec::new()),
            graph_max_lanes: 1,
            resizing: false,
            drag_anchor: None,
        };
        graph.spawn_load_initial(cx);
        graph
    }

    /// Recompute the swimlane layout from the currently loaded commits.
    /// Called after any change to the commit set; a no-op (empty layout)
    /// when the graph isn't in a `Ready` state.
    fn recompute_layout(&mut self) {
        if let GraphState::Ready { commits, .. } = &self.state {
            let layout = compute_graph(commits);
            self.graph_max_lanes = max_lanes(&layout);
            self.graph_layout = Rc::new(layout);
        } else {
            self.graph_layout = Rc::new(Vec::new());
            self.graph_max_lanes = 1;
        }
    }

    /// Current body height — exposed so callers (e.g. snapshot tests
    /// later) don't have to round-trip through render.
    #[allow(dead_code)]
    pub fn graph_height(&self) -> Pixels {
        self.graph_height
    }

    /// Apply a new graph height; clamps against the live window
    /// height, persists when a settings repo is wired, and notifies
    /// the view tree. No-op when the value would be unchanged so the
    /// repo isn't pelted with redundant writes during a held key.
    pub fn set_graph_height(
        &mut self,
        candidate: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_height = f32::from(window.bounds().size.height);
        let clamped = scm_layout_settings::clamp_graph_height(candidate, window_height);
        let new_height = px(clamped);
        if self.graph_height == new_height {
            return;
        }
        self.graph_height = new_height;
        if let Some(repo) = &self.settings_repo {
            scm_layout_settings::save_graph_height(repo, clamped);
        }
        cx.notify();
    }

    /// Apply one mouse drag-resize tick. `cursor_y` is the pointer's
    /// window-space Y and `window_height` the live window height (both from
    /// the workspace-root `on_drag_move` listener — it reads the window
    /// height off the listener-div bounds, so this never threads a `Window`
    /// through the nested entity updates). The first tick of a drag latches
    /// the anchor cursor-y; thereafter the height tracks the cursor delta —
    /// dragging the handle UP (smaller `cursor_y`) grows the section, DOWN
    /// shrinks it — clamped and persisted. No-op when no drag is armed.
    pub fn apply_graph_drag(&mut self, cursor_y: f32, window_height: f32, cx: &mut Context<Self>) {
        let Some((start_height, start_y)) = self.drag_anchor else {
            return;
        };
        self.resizing = true;
        let start_y = match start_y {
            Some(y) => y,
            None => {
                // First move tick — latch the pointer origin and wait for
                // real movement before changing the height.
                self.drag_anchor = Some((start_height, Some(cursor_y)));
                return;
            }
        };
        let candidate = start_height + (start_y - cursor_y);
        let clamped = scm_layout_settings::clamp_graph_height(candidate, window_height);
        let new_height = px(clamped);
        if self.graph_height == new_height {
            return;
        }
        self.graph_height = new_height;
        if let Some(repo) = &self.settings_repo {
            scm_layout_settings::save_graph_height(repo, clamped);
        }
        cx.notify();
    }

    /// Translate a key event on the resize rail into a height change.
    /// Returns `true` when the key was consumed so the listener can
    /// `cx.stop_propagation()` and prevent bubbling to other handlers.
    /// Arithmetic lives in `scm_layout_settings::next_graph_height` so
    /// the keyboard mapping is exercised by pure unit tests.
    fn handle_resize_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = ev.keystroke.key.as_str();
        let shift = ev.keystroke.modifiers.shift;
        let current = f32::from(self.graph_height);
        let window_height = f32::from(window.bounds().size.height);
        match scm_layout_settings::next_graph_height(current, key, shift, window_height) {
            Some(candidate) => {
                self.set_graph_height(candidate, window, cx);
                true
            }
            None => false,
        }
    }

    /// Refresh the graph (e.g. after a successful commit). When data is
    /// already loaded, runs in stale-while-revalidate mode: the previous
    /// commits stay painted and only the refresh button flips to its
    /// loading spinner. When no data is loaded yet (initial mount, prior
    /// failure), falls back to the full Loading placeholder.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match &mut self.state {
            GraphState::Ready { refreshing, .. } => {
                if *refreshing {
                    return;
                }
                *refreshing = true;
            }
            _ => {
                self.state = GraphState::Loading;
            }
        }
        self.spawn_load_initial(cx);
        cx.notify();
    }

    /// Toggle the section open/closed. Called from the header chevron.
    fn toggle_collapsed(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        cx.notify();
    }

    fn spawn_load_initial(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let workdir = self.repo.workdir().to_path_buf();
        // Two-stage delivery so the commit list paints the moment the
        // single `git log` returns instead of waiting on the per-commit
        // `--numstat` stats (which only feed the hover tooltip). Stage 1
        // sends the commit page; the UI renders it immediately. Stage 2
        // sends the stats map once the concurrent numstat fetch finishes,
        // and the tooltip cache fills in under the already-visible list.
        // Previously both arrived in one message, gating the whole list
        // behind ~1 s of stat shellouts.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InitialLoad>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let commits_result = repo.log_recent(PAGE_SIZE).await;
                    // Keep an owned copy for the stats pass before the
                    // result is moved into the stage-1 message.
                    let commits_for_stats = commits_result.as_ref().ok().cloned();
                    if tx.send(InitialLoad::Commits(commits_result)).is_err() {
                        return;
                    }
                    let stats = collect_commit_stats(&workdir, commits_for_stats.as_ref()).await;
                    let _ = tx.send(InitialLoad::Stats(stats));
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::source_control::graph",
                    "no tokio runtime; commit graph stays in Loading state"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.recv().await {
                let applied = this.update(cx, |g, cx| {
                    match msg {
                        InitialLoad::Commits(result) => {
                            g.state = match result {
                                Ok(commits) => GraphState::Ready {
                                    can_load_more: commits.len() == PAGE_SIZE as usize,
                                    commits,
                                    loading_more: false,
                                    refreshing: false,
                                },
                                Err(e) => GraphState::Failed(e.to_string()),
                            };
                            g.recompute_layout();
                        }
                        InitialLoad::Stats(stats) => {
                            // Wholesale replace — a fresh load drops every
                            // prior page's stats so the cache can't
                            // accumulate orphan entries from commits no
                            // longer visible.
                            g.stat_cache = stats;
                        }
                    }
                    cx.notify();
                });
                // Entity gone (panel dropped) — stop draining.
                if applied.is_err() {
                    return;
                }
            }
        });
        self._load_task = Some(task);
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        // Read the offset and flip `loading_more` in-place. The earlier
        // implementation used `std::mem::take(commits)` which left an empty
        // Vec inside the state for the duration of the async fetch — the
        // intervening render then hit the `commits.is_empty()` arm and
        // showed "No commits yet" + a "GRAPH 0 +" header before the new
        // page resolved. Keeping the previous list intact eliminates that
        // flash entirely.
        let offset = match &mut self.state {
            GraphState::Ready {
                commits,
                loading_more,
                refreshing,
                ..
            } if !*loading_more && !*refreshing => {
                *loading_more = true;
                commits.len() as u32
            }
            _ => return,
        };
        let repo = self.repo.clone();
        let workdir = self.repo.workdir().to_path_buf();
        let (tx, rx) =
            oneshot::channel::<(Result<Vec<CommitInfo>, GitError>, HashMap<String, (u32, u32)>)>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let page_result = repo.log_page(offset, PAGE_SIZE).await;
                    let stats = collect_commit_stats(&workdir, page_result.as_ref().ok()).await;
                    let _ = tx.send((page_result, stats));
                });
            }
            Err(_) => {
                if let GraphState::Ready { loading_more, .. } = &mut self.state {
                    *loading_more = false;
                }
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok((result, stats)) = rx.await else { return };
            let _ = this.update(cx, |g, cx| {
                if let GraphState::Ready {
                    commits,
                    can_load_more,
                    loading_more,
                    ..
                } = &mut g.state
                {
                    *loading_more = false;
                    match result {
                        Ok(mut more) => {
                            *can_load_more = more.len() == PAGE_SIZE as usize;
                            commits.append(&mut more);
                            // Extend cache in place — prior pages keep
                            // their stats; only new commits' entries
                            // get merged in.
                            g.stat_cache.extend(stats);
                        }
                        Err(e) => {
                            g.state = GraphState::Failed(e.to_string());
                        }
                    }
                }
                // Re-layout outside the `&mut g.state` borrow above:
                // appended parents may resolve lanes that previously hung
                // off the page bottom. No-op when the load failed.
                g.recompute_layout();
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }
}

/// Fetch per-commit numstat for every OID in `commits` (if `Some`),
/// returning a `oid → (added, removed)` map. Each commit is queried
/// sequentially via `oximux_git::diff_numstat_commit`; failures are
/// silently dropped — the tooltip caller treats a missing entry the
/// same as a successful zero-change diff and just omits the stats
/// slot. Returns an empty map when `commits` is `None` or empty.
async fn collect_commit_stats(
    workdir: &std::path::Path,
    commits: Option<&Vec<CommitInfo>>,
) -> HashMap<String, (u32, u32)> {
    let Some(commits) = commits else {
        return HashMap::new();
    };
    // One `git diff --numstat` shellout per commit. Run them concurrently
    // rather than awaiting each in turn: a page is 20 commits, and at
    // ~50 ms per shellout a serial loop costs ~1 s of wall-clock — long
    // enough to be the visible delay before the commit list's hover
    // tooltips populate. Concurrency is CAPPED so a page arrival never
    // forks 20 git processes at once (process churn + repo lock
    // contention); 4 in flight keeps the wall-clock near the uncapped
    // case while bounding the spike. Order is irrelevant: results land
    // in a map keyed by OID.
    const MAX_CONCURRENT_NUMSTAT: usize = 4;
    use futures::StreamExt;
    // Materialize the future list first (owned oids) — mapping lazily
    // inside `stream::iter` trips a higher-ranked lifetime check under
    // `tokio::spawn`'s Send bound.
    let futs: Vec<_> = commits
        .iter()
        .map(|c| {
            let oid = c.oid.clone();
            async move {
                let res = oximux_git::diff_numstat_commit(workdir, &oid).await;
                (oid, res)
            }
        })
        .collect();
    let results: Vec<_> = futures::stream::iter(futs)
        .buffer_unordered(MAX_CONCURRENT_NUMSTAT)
        .collect()
        .await;
    let mut out = HashMap::with_capacity(results.len());
    for (oid, res) in results {
        match res {
            Ok(stats) => {
                out.insert(oid, stats);
            }
            Err(err) => {
                tracing::debug!(
                    target: "oximux_app::source_control::graph",
                    oid = %oid,
                    error = %err,
                    "diff_numstat_commit failed; tooltip stats slot will be absent for this row"
                );
            }
        }
    }
    out
}

impl EventEmitter<ShowCommitRequested> for CommitGraph {}

impl Focusable for CommitGraph {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // A drag-resize is over once no drag is active: releasing the mouse
        // produces no further drag-move ticks, so the flag (and the latched
        // anchor) are cleared here on the next render instead.
        if self.resizing && !cx.has_active_drag() {
            self.resizing = false;
            self.drag_anchor = None;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let style = ScmStyle::new(density, typography);

        let (count_label, can_load_more_flag, is_refreshing) = match &self.state {
            GraphState::Ready {
                commits,
                can_load_more,
                refreshing,
                ..
            } => (format!("{}", commits.len()), *can_load_more, *refreshing),
            // Initial load (no data yet) — keep the header count placeholder
            // muted so the user sees the section is still resolving.
            _ => ("…".to_string(), false, false),
        };
        let is_initial_loading = matches!(self.state, GraphState::Loading);

        // Right-aligned cluster: a (?) help button (wires up when a refs-help
        // popover ships) plus a refresh button that reloads the page-0 commit
        // list immediately.
        let header_actions = div()
            .ml_auto()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(style.icon_cluster_gap))
            .child(
                Button::new("graph-help")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path("icons/circle-help.svg")
                            .size(px(style.icon)),
                    )
                    .tooltip("What are graph refs? (coming soon)")
                    .disabled(true),
            )
            .child(
                Button::new("graph-refresh")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path("icons/refresh-cw.svg")
                            .size(px(style.icon)),
                    )
                    // `.loading(true)` swaps the icon for the upstream
                    // spinner and short-circuits clicks, so a second refresh
                    // tap during an in-flight fetch is a no-op without us
                    // wiring extra guards in `on_click`. Pair with the SWR
                    // flag so the visual feedback only kicks in while a
                    // fetch is actually outstanding.
                    .loading(is_refreshing || is_initial_loading)
                    .tooltip(if is_refreshing {
                        "Refreshing…"
                    } else {
                        "Refresh graph"
                    })
                    .on_click(cx.listener(|graph, _: &ClickEvent, _window, cx| {
                        graph.refresh(cx);
                    })),
            );

        // Header sizing mirrors the reference: text-xs (12px) uppercase
        // semibold with a 14px chevron, count rendered at 11px tabular nums
        // so digits don't jitter as commit count grows. The label cluster
        // (chevron + GRAPH + count) shares one click target that toggles the
        // section open/closed; the right-side icon buttons stay as their own
        // independent clickable controls.
        let chevron_icon = if self.collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        let toggle_label = div()
            .id("graph-header-toggle")
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .hover(|s| s.text_color(theme.fg_base))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|graph, _: &MouseDownEvent, _window, cx| {
                    graph.toggle_collapsed(cx);
                }),
            )
            .child(
                Icon::new(chevron_icon)
                    .size(px(style.icon))
                    .text_color(theme.fg_subtle),
            )
            .child("GRAPH")
            .child(
                div()
                    .text_size(px(style.graph_meta_text))
                    .text_color(theme.fg_subtle)
                    .child(count_label),
            )
            .child(if can_load_more_flag {
                div()
                    .text_size(px(style.graph_meta_text))
                    .text_color(theme.fg_subtle)
                    .child("+")
            } else {
                div()
            });

        let header = div()
            .flex()
            .items_center()
            .h(px(density.h_tab))
            .px(px(style.pad_h))
            .text_size(px(style.body_text))
            .font_weight(typography.w_semibold)
            .text_color(theme.fg_muted)
            .child(toggle_label)
            .child(header_actions);

        let body = match &self.state {
            // Initial-load placeholder matches the eventual `uniform_list`
            // height so the section doesn't pop taller when the first
            // page resolves. Refresh never enters this arm — see
            // `refresh()` for the stale-while-revalidate path that keeps
            // the existing list painted. Reads `graph_height` (Phase 13)
            // so a user who shrank the section before quitting doesn't
            // see a tall placeholder on next launch.
            GraphState::Loading => placeholder_sized(
                "Loading commits…",
                f32::from(self.graph_height),
                theme,
                density,
                typography,
            )
            .into_any_element(),
            GraphState::Failed(e) => {
                placeholder(&format!("git log failed: {e}"), theme, density, typography)
                    .into_any_element()
            }
            GraphState::Ready { commits, .. } if commits.is_empty() => {
                placeholder("No commits yet", theme, density, typography).into_any_element()
            }
            GraphState::Ready { commits, .. } => {
                let commits = commits.clone();
                let theme_cap = theme;
                let typography_cap = self.typography.clone();
                let density_cap = self.density;
                // Precomputed swimlane model (1:1 with `commits`) + the
                // page's fixed gutter width. Cheap `Rc`/`usize` clones into
                // the row closure.
                let layout = self.graph_layout.clone();
                let max_lanes_cap = self.graph_max_lanes;
                // Snapshot the stat cache once for the closure so
                // each row's lookup is a constant-time HashMap hit.
                // Cloning at most 20–60 (oid → (u32, u32)) entries is
                // negligible compared to the per-row layout cost.
                let stat_cache = self.stat_cache.clone();
                // Author column auto-hide: skip rendering the author
                // chunk when every loaded commit shares one author
                // (solo-author repo). Short-circuits the scan as soon
                // as we see a second distinct author so a 20-page of
                // even-mixed commits costs O(2), not O(20). Recomputed
                // every render — cheap enough at typical page sizes
                // (under 100 commits) that caching is overkill.
                let show_author = {
                    let mut authors: HashSet<&str> = HashSet::new();
                    for c in &commits {
                        authors.insert(c.author.as_str());
                        if authors.len() > 1 {
                            break;
                        }
                    }
                    authors.len() > 1
                };
                // Weak self-handle so each row's click closure can
                // upgrade and emit `ShowCommitRequested` back to the
                // graph entity. The list callback receives only
                // `&mut App` (no entity context), so a strong listener
                // pattern doesn't fit — `WeakEntity::update` is the
                // canonical bridge.
                let weak = cx.entity().downgrade();
                uniform_list(
                    "commit-graph-list",
                    commits.len(),
                    move |range, _window, _cx| {
                        let mut rows = Vec::with_capacity(range.len());
                        for ix in range {
                            if let Some(c) = commits.get(ix) {
                                let stats = stat_cache.get(&c.oid).copied();
                                // Layout is rebuilt in lockstep with
                                // `commits`, so `ix` indexes both. The
                                // `unwrap_or_default` only guards a
                                // transient frame where a fresh page
                                // painted before its relayout landed.
                                let row_layout = layout.get(ix).cloned().unwrap_or_default();
                                rows.push(render_commit_row(
                                    c,
                                    &row_layout,
                                    max_lanes_cap,
                                    theme_cap,
                                    density_cap,
                                    &typography_cap,
                                    weak.clone(),
                                    show_author,
                                    stats,
                                ));
                            }
                        }
                        rows
                    },
                )
                // Phase 13: body height is state (graph_height) now,
                // not a const. Keyboard rail below the body mutates
                // it via Arrow / Shift+Arrow / Home / End.
                .h(self.graph_height)
                .track_scroll(&self.scroll)
                .into_any_element()
            }
        };

        // Subtle text-link "Load more" rather than a chunky ghost button —
        // the reference layout treats pagination as low-priority chrome that
        // shouldn't compete with the commit rows above it.
        let load_more = match &self.state {
            GraphState::Ready {
                can_load_more: true,
                loading_more,
                ..
            } => {
                let is_loading = *loading_more;
                Some(
                    div()
                        .id("graph-load-more")
                        .flex()
                        .justify_center()
                        .py(px(style.pad_v_tight))
                        .text_size(px(style.graph_meta_text))
                        .text_color(theme.fg_subtle)
                        .when(!is_loading, |s| {
                            s.cursor_pointer().hover(|s| s.text_color(theme.fg_base))
                        })
                        .when(!is_loading, |s| {
                            s.on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|graph, _: &MouseDownEvent, _window, cx| {
                                    graph.load_more(cx);
                                    cx.notify();
                                }),
                            )
                        })
                        .child(if is_loading {
                            "Loading…"
                        } else {
                            "Load more"
                        }),
                )
            }
            _ => None,
        };

        // `flex_shrink_0` keeps the graph at its natural height even when the
        // file list above grows — the section stays pinned to the bottom of
        // the panel rather than getting squeezed away by flex pressure.
        let mut col = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .bg(theme.bg_panel);
        // Mouse drag-resize handle at the section's top edge (only when
        // expanded — there's nothing to resize when collapsed). The matching
        // `on_drag_move` listener lives on the Source Control panel root.
        if !self.collapsed {
            col = col.child(self.render_drag_handle(cx));
        }
        col = col.child(header);
        if !self.collapsed {
            col = col.child(body);
            if let Some(lm) = load_more {
                col = col.child(lm);
            }
            // Phase 13: keyboard resize rail. Sits at the bottom of
            // the section so Tab order arrives here AFTER the
            // commit list. Reads the focus state to show the focus
            // ring on tab arrival; Arrow / Shift+Arrow / Home / End
            // mutate `graph_height`.
            col = col.child(self.render_resize_rail(window, cx));
        }
        col
    }
}

/// Total mouse hit-height of the top drag handle. A touch taller than the
/// visible line so the grab zone is forgiving (the line is centred in it).
const GRAPH_RESIZE_HIT_PX: f32 = 9.0;
/// Height of the hover/drag highlight bar painted over the hairline.
const GRAPH_RESIZE_BAR_PX: f32 = 3.0;

impl CommitGraph {
    /// Build the mouse drag-resize handle at the TOP edge of the graph
    /// section (the boundary it shares with the content above). Dragging it
    /// up grows the graph, down shrinks it — the VS Code gesture. A hairline
    /// at rest; a wider `border_active` bar lights on hover and stays lit for
    /// the whole drag via `resizing`. The matching `on_drag_move` listener
    /// lives on the Source Control panel root so the cursor stays inside its
    /// bounds even as it travels up out of this section.
    fn render_drag_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let resizing = self.resizing;
        let weak = cx.entity().downgrade();
        // Visible hairline centred in the hit zone; a wider bar lights over
        // it on hover / during a drag. Centring the line gives a few px of
        // grab tolerance on either side.
        let hover_bar = div()
            .absolute()
            .left_0()
            .right_0()
            .top(px((GRAPH_RESIZE_HIT_PX - GRAPH_RESIZE_BAR_PX) / 2.0))
            .h(px(GRAPH_RESIZE_BAR_PX))
            .bg(theme.border_inactive)
            .group_hover("graph-resize", move |s| s.bg(theme.border_active))
            .when(resizing, |b| b.bg(theme.border_active));
        div()
            .id(ElementId::Name(SharedString::from("graph-resize-handle")))
            .group("graph-resize")
            .relative()
            .w_full()
            .h(px(GRAPH_RESIZE_HIT_PX))
            .flex_shrink_0()
            .cursor_row_resize()
            .occlude()
            .child(hover_bar)
            .on_drag(GraphResizePayload, move |_payload, _offset, _window, cx| {
                // Latch the height at drag start; the cursor origin is
                // captured on the first move tick (drag-start can't read the
                // pointer position).
                let _ = weak.update(cx, |g, _| {
                    g.drag_anchor = Some((f32::from(g.graph_height), None));
                    g.resizing = true;
                });
                cx.new(|_| GraphResizeGhost)
            })
            .into_any_element()
    }

    /// Build the focusable keyboard-resize rail rendered at the bottom
    /// of the graph section. 3px tall; subtle by default, accent stripe
    /// on focus + hover. cursor `row_resize` for parity with mouse
    /// expectation even though only key handling is wired (mouse
    /// dragging here would compete with the sidebar drag — out of
    /// scope for Phase 13).
    fn render_resize_rail(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let focused = self.focus_handle.is_focused(window);
        let bar_color = if focused {
            theme.focus_ring
        } else {
            theme.border_inactive
        };
        div()
            .id(ElementId::Name(SharedString::from("graph-resize-rail")))
            .track_focus(&self.focus_handle)
            .w_full()
            .h(px(3.0))
            .flex_shrink_0()
            .bg(bar_color)
            .hover(|s| s.bg(theme.focus_ring))
            .cursor_row_resize()
            .on_key_down(
                cx.listener(|graph, ev: &KeyDownEvent, window, cx| {
                    if graph.handle_resize_key(ev, window, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
    }
}
fn placeholder(
    msg: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    placeholder_sized(msg, 80.0, theme, density, typography)
}

/// Variant of `placeholder` with a caller-controlled height. Used by the
/// initial-load arm so the placeholder matches the eventual list height and
/// the section doesn't visibly resize when commits arrive.
fn placeholder_sized(
    msg: &str,
    height_px: f32,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let style = ScmStyle::new(density, typography);
    div()
        .flex()
        .items_center()
        .justify_center()
        .p(px(density.pad_panel))
        .h(px(height_px))
        .text_size(px(style.body_text))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
