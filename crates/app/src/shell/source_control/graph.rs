//! Commit-graph panel (flat recent commits list, no DAG drawing for v1).
//!
//! Loads `Repository::log_recent(20)` on mount; "Load more" extends in
//! 20-row chunks. State machine: `Loading → Ready | Failed`.

use gpui::{
    App, ClickEvent, Context, ElementId, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled, Task,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
    uniform_list,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use oximux_core::{CommitInfo, RefLabel};
use oximux_git::{GitError, Repository};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;
use tokio::sync::oneshot;

use crate::scm_layout_settings;
use crate::shell::source_control::style as sc_style;

const PAGE_SIZE: u32 = 20;

/// Max ref chips rendered inline per commit row before the overflow
/// `+N` chip kicks in. Tuned for the standard sidebar width — two
/// chips leave room for the subject, author, date, and SHA columns.
const REF_CHIPS_VISIBLE: usize = 2;

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
        let (tx, rx) = oneshot::channel::<Result<Vec<CommitInfo>, GitError>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.log_recent(PAGE_SIZE).await;
                    let _ = tx.send(r);
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
            let Ok(result) = rx.await else { return };
            let _ = this.update(cx, |g, cx| {
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
        let (tx, rx) = oneshot::channel::<Result<Vec<CommitInfo>, GitError>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.log_page(offset, PAGE_SIZE).await;
                    let _ = tx.send(r);
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
            let Ok(result) = rx.await else { return };
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
            .gap(px(2.0))
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
                    .text_size(px(sc_style::META_TEXT))
                    .text_color(theme.fg_subtle)
                    .child(count_label),
            )
            .child(if can_load_more_flag {
                div()
                    .text_size(px(sc_style::META_TEXT))
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
            .text_size(px(sc_style::TEXT))
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
                                rows.push(render_commit_row(
                                    c,
                                    theme_cap,
                                    density_cap,
                                    &typography_cap,
                                    weak.clone(),
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
                        .text_size(px(sc_style::META_TEXT))
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

fn render_commit_row(
    c: &CommitInfo,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<CommitGraph>,
) -> gpui::AnyElement {
    // Single-line row: dot + subject (truncates) + author + date + short sha.
    // Reference layout collapses the v1 two-line "subject / author • date"
    // into one tight row so 20+ commits stay scannable inside the sidebar.
    //
    // The timeline column stacks a connector line above the dot and another
    // below it, so consecutive rows draw an unbroken vertical line through
    // the dot centers (the canonical commit-graph spine).
    let timeline = div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(14.0))
        .h_full()
        .child(div().w(px(1.0)).flex_1().bg(theme.border_inactive))
        .child(
            div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(theme.focus_ring),
        )
        .child(div().w(px(1.0)).flex_1().bg(theme.border_inactive));

    // Subject flex-grows but truncates. `w_full` on the outer row gives the
    // `flex_1` child a definite width to shrink against; without it taffy
    // hands the row its intrinsic (content) width and truncation never
    // engages — the long subject paints past the panel's right edge.
    //
    // `min_w(px(80))` is a hard floor so the subject can never collapse to
    // nothing under narrow-panel pressure. At 12px sans-serif that's ~11
    // characters before ellipsis — enough to keep the row meaningful at
    // any width the cockpit shell allows. Combined with `flex_shrink_0`
    // on the date/sha trailing columns and `overflow_hidden` on the row,
    // any remaining overflow clips from the right (sha first, then date),
    // giving the desired collapse priority without per-column shrink
    // factors that GPUI's Styled trait doesn't expose numerically.
    let subject = div()
        .flex_1()
        .min_w(px(80.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(sc_style::TEXT))
        .text_color(theme.fg_base)
        .child(c.subject.clone());

    // Author capped to ~88px so a long display name can't crowd out the
    // subject column. Date and SHA stay shrink-0 because they're naturally
    // short and always need to be readable.
    let author = div()
        .flex_shrink()
        .min_w(px(0.0))
        .max_w(px(88.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(sc_style::META_TEXT))
        .text_color(theme.fg_subtle)
        .child(c.author.clone());

    let date = div()
        .flex_shrink_0()
        .text_size(px(sc_style::META_TEXT))
        .text_color(theme.fg_subtle)
        .child(c.short_date.clone());

    // Short OID rendered in the typography mono face at 10px to match the
    // reference — the smaller monospaced rendering keeps the hash readable
    // without competing visually with the subject/author meta.
    let sha = div()
        .flex_shrink_0()
        .text_size(px(10.0))
        .font_family(typography.family_mono.clone())
        .text_color(theme.fg_subtle)
        .child(c.short_oid.clone());

    // Row has an explicit height so the timeline's flex_1 connector lines
    // have a definite parent height to distribute into. Without `h(…)` the
    // row collapses to text line-height and the connector lines shrink to a
    // near-invisible 2-3 px each.
    //
    // The hover tooltip surfaces the full subject (no truncation) and the
    // commit body, so a contributor can read the whole message without
    // checking out the commit. Captures are cheap `SharedString` clones.
    let tip_subject: SharedString = c.subject.clone().into();
    let tip_body: SharedString = c.body.clone().into();
    let tip_theme = theme;
    let tip_typography = typography.clone();

    // Row carries an `.id(...)` so the tooltip can attach: `.tooltip(...)`
    // lives on `StatefulInteractiveElement`, which div only implements
    // after an id is set. The short OID is unique per commit, so a
    // per-row id is both stable across renders and collision-free.
    let row_id = ElementId::Name(format!("graph-commit-{}", c.short_oid).into());
    let chips = render_ref_chips(&c.refs, &c.short_oid, theme, typography);

    // Capture-by-clone for the click closure — emits `ShowCommitRequested`
    // so the host workspace can open a commit-detail tab. The closure
    // holds `String` clones rather than borrowing the row's `CommitInfo`
    // because GPUI listeners outlive the render call's borrow stack.
    let click_sha = c.oid.clone();
    let click_short = c.short_oid.clone();
    let click_subject = c.subject.clone();

    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .px(px(sc_style::PAD_H))
        .h(px(sc_style::COMMIT_ROW_H))
        .w_full()
        .overflow_hidden()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .child(timeline.self_stretch())
        .child(subject)
        .when_some(chips, |row, chips| row.child(chips))
        .child(author)
        .child(date)
        .child(sha)
        .tooltip(move |window, cx| {
            let subject = tip_subject.clone();
            let body = tip_body.clone();
            let theme = tip_theme;
            let typography = tip_typography.clone();
            Tooltip::element(move |_window, _cx| {
                render_commit_tooltip(subject.clone(), body.clone(), theme, typography.clone())
            })
            .build(window, cx)
        })
        .on_click(move |_: &ClickEvent, _window, cx| {
            let _ = weak.update(cx, |_graph, cx| {
                cx.emit(ShowCommitRequested {
                    sha: click_sha.clone(),
                    short_oid: click_short.clone(),
                    subject: click_subject.clone(),
                });
            });
        })
        .into_any_element()
}

/// Render the `RefLabel` chip cluster shown between subject and author
/// columns. `None` when the commit has no decorations — caller skips
/// the slot entirely so the row layout stays tight.
///
/// Cap at `REF_CHIPS_VISIBLE` visible chips; overflow collapses into a
/// `+N` chip whose tooltip lists the hidden refs. HEAD chip is
/// foregrounded with `status_info` so the current commit reads at a
/// glance.
fn render_ref_chips(
    refs: &[RefLabel],
    row_id: &str,
    theme: Theme,
    typography: &Typography,
) -> Option<gpui::Div> {
    if refs.is_empty() {
        return None;
    }
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .flex_shrink_0();
    let mut shown = 0usize;
    for r in refs.iter().take(REF_CHIPS_VISIBLE) {
        row = row.child(ref_chip_for(r, theme, typography));
        shown += 1;
    }
    let hidden = refs.len().saturating_sub(shown);
    if hidden > 0 {
        let overflow_text: SharedString = format!("+{hidden}").into();
        // Tooltip text lists every hidden ref's literal label; built
        // once at render so the closure doesn't have to re-walk.
        let hidden_labels: Vec<String> = refs
            .iter()
            .skip(shown)
            .map(ref_label_text)
            .collect();
        let tip_text: SharedString = hidden_labels.join(", ").into();
        // Per-commit unique id — keying on count alone collides when
        // two visible commits both have the same total ref count
        // (overflow chip tooltips would cross-pollute via shared
        // interactive state).
        let chip_id =
            ElementId::Name(format!("ref-chip-overflow-{row_id}").into());
        row = row.child(
            div()
                .id(chip_id)
                .px(px(6.0))
                .py(px(1.0))
                .rounded(px(3.0))
                .bg(theme.bg_panel_alt)
                .text_size(px(sc_style::SUB_LABEL_TEXT))
                .text_color(theme.fg_muted)
                .child(overflow_text)
                .tooltip(move |window, cx| {
                    Tooltip::new(tip_text.clone()).build(window, cx)
                }),
        );
    }
    Some(row)
}

/// Single chip for one `RefLabel`. HEAD pops in `status_info` blue;
/// branch tips, remote-tracking branches, and tags each pick their
/// own neutral tint so the cluster scans like a legend without
/// shouting.
fn ref_chip_for(r: &RefLabel, theme: Theme, typography: &Typography) -> gpui::Div {
    let (label, fg, bg) = match r {
        RefLabel::Head => (
            "HEAD".to_string(),
            theme.status_info,
            gpui::Hsla {
                a: 0.20,
                ..theme.status_info
            },
        ),
        RefLabel::BranchTip { name } => (
            name.clone(),
            theme.fg_base,
            theme.bg_panel_alt,
        ),
        RefLabel::RemoteBranch { name } => (
            name.clone(),
            theme.fg_muted,
            theme.bg_panel_alt,
        ),
        RefLabel::Tag { name } => (
            format!("⚑ {name}"),
            theme.status_warning,
            gpui::Hsla {
                a: 0.20,
                ..theme.status_warning
            },
        ),
        RefLabel::Other { raw } => (raw.clone(), theme.fg_subtle, theme.bg_panel_alt),
    };
    let _ = typography; // kept for future font-tweak hooks (mono vs sans).
    div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(3.0))
        .bg(bg)
        .text_size(px(sc_style::SUB_LABEL_TEXT))
        .text_color(fg)
        .child(label)
}

/// Render text for a ref's tooltip representation. Mirrors what
/// `git log --decorate` shows so the tooltip reads naturally to
/// anyone who's used the CLI.
fn ref_label_text(r: &RefLabel) -> String {
    match r {
        RefLabel::Head => "HEAD".to_string(),
        RefLabel::BranchTip { name } => name.clone(),
        RefLabel::RemoteBranch { name } => name.clone(),
        RefLabel::Tag { name } => format!("tag: {name}"),
        RefLabel::Other { raw } => raw.clone(),
    }
}

/// Build the multi-line tooltip body shown on commit-row hover. Subject sits
/// on its own as the title; the commit body — when present — renders below
/// it with the original line breaks preserved. Width is capped so long
/// subjects wrap instead of stretching the popover across the workspace.
fn render_commit_tooltip(
    subject: SharedString,
    body: SharedString,
    theme: Theme,
    typography: Typography,
) -> impl IntoElement {
    // Width cap chosen so a typical 60-column body wraps once or twice
    // rather than producing a single very wide line.
    let max_width = px(440.0);

    let mut col = div()
        .flex()
        .flex_col()
        .max_w(max_width)
        .text_size(px(sc_style::TEXT))
        .text_color(theme.fg_base)
        .child(div().font_weight(typography.w_semibold).child(subject));

    if !body.is_empty() {
        // GPUI's text element doesn't split on `\n` itself, so we emit one
        // child per body line and substitute a small spacer for blank lines.
        // This preserves the original paragraph structure of the commit
        // message (bullet lists, code-block-style indents, footer trailers).
        let mut body_col = div()
            .flex()
            .flex_col()
            .pt(px(sc_style::PAD_V_TIGHT))
            .text_color(theme.fg_muted);
        for line in body.lines() {
            if line.is_empty() {
                body_col = body_col.child(div().h(px(6.0)));
            } else {
                body_col = body_col.child(div().child(line.to_string()));
            }
        }
        col = col.child(body_col);
    }
    col
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
        .text_size(px(sc_style::TEXT))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
