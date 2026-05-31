//! Commit-graph panel (flat recent commits list, no DAG drawing for v1).
//!
//! Loads `Repository::log_recent(20)` on mount; "Load more" extends in
//! 20-row chunks. State machine: `Loading → Ready | Failed`.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, ClickEvent, Context, ElementId, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, SharedString, Styled, Task, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
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
use crate::shell::source_control::graph_row::render_commit_row;
use crate::shell::source_control::style as sc_style;

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
        };
        graph.spawn_load_initial(cx);
        graph
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
        // Combined payload: the commit page AND its per-commit numstat
        // map arrive together so the tooltip stats slot is populated
        // the first time the user hovers a row. Sequential fetch
        // inside one tokio task — at 20 commits × ~50 ms per `git
        // diff --numstat` shellout that's ~1 s total wall-clock,
        // happening once on panel mount. Acceptable for the polish
        // pass; a future revision can switch to a `JoinSet` for
        // parallelism if profiling shows it matters.
        let (tx, rx) =
            oneshot::channel::<(Result<Vec<CommitInfo>, GitError>, HashMap<String, (u32, u32)>)>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let commits_result = repo.log_recent(PAGE_SIZE).await;
                    let stats = collect_commit_stats(&workdir, commits_result.as_ref().ok()).await;
                    let _ = tx.send((commits_result, stats));
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
            let Ok((result, stats)) = rx.await else { return };
            let _ = this.update(cx, |g, cx| {
                // Wholesale replace — a fresh load drops every prior
                // page's stats (refresh path), so the cache can't
                // accumulate orphan entries from commits no longer
                // visible.
                g.stat_cache = stats;
                g.state = match result {
                    Ok(commits) => GraphState::Ready {
                        can_load_more: commits.len() == PAGE_SIZE as usize,
                        commits,
                        loading_more: false,
                        refreshing: false,
                    },
                    Err(e) => GraphState::Failed(e.to_string()),
                };
                cx.notify();
            });
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
    let mut out = HashMap::new();
    let Some(commits) = commits else {
        return out;
    };
    for c in commits {
        match oximux_git::diff_numstat_commit(workdir, &c.oid).await {
            Ok(stats) => {
                out.insert(c.oid.clone(), stats);
            }
            Err(err) => {
                tracing::debug!(
                    target: "oximux_app::source_control::graph",
                    oid = %c.oid,
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
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

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
            .gap(px(sc_style::ICON_CLUSTER_GAP))
            .child(
                Button::new("graph-help")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path("icons/circle-help.svg")
                            .size(px(sc_style::ICON)),
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
                            .size(px(sc_style::ICON)),
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
                    .size(px(sc_style::ICON))
                    .text_color(theme.fg_subtle),
            )
            .child("GRAPH")
            .child(
                div()
                    .text_size(px(sc_style::GRAPH_META_TEXT))
                    .text_color(theme.fg_subtle)
                    .child(count_label),
            )
            .child(if can_load_more_flag {
                div()
                    .text_size(px(sc_style::GRAPH_META_TEXT))
                    .text_color(theme.fg_subtle)
                    .child("+")
            } else {
                div()
            });

        let header = div()
            .flex()
            .items_center()
            .h(px(density.h_tab))
            .px(px(sc_style::PAD_H))
            .text_size(px(sc_style::BODY_TEXT))
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
                                rows.push(render_commit_row(
                                    c,
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
                        .py(px(sc_style::PAD_V_TIGHT))
                        .text_size(px(sc_style::GRAPH_META_TEXT))
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
            .bg(theme.bg_panel)
            .child(header);
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

impl CommitGraph {
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
    _typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .p(px(density.pad_panel))
        .h(px(height_px))
        .text_size(px(sc_style::BODY_TEXT))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
