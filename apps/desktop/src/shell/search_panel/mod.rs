//! SearchPanel — repo-wide full-text search via `ripgrep --json`.
//!
//! Three input fields (`query`, include glob, exclude glob) feed `SearchOptions`.
//! Every change schedules a 300ms-debounced background search that runs
//! `rg_runner::run_ripgrep`. Each search has a monotonic `latest_search_id`
//! so stale results from cancelled invocations are dropped on arrival.

pub mod header_render;
pub mod match_render;
pub mod rg_runner;
pub mod rows;
pub mod search_state;

mod result_row;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Subscription, Task, UniformListScrollHandle, Window, div, px, uniform_list,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::scroll::ScrollableElement as _;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::search_panel::header_render::{SummaryState, render_header};
use crate::shell::search_panel::result_row::{RowPaintCtx, paint_file_row, paint_match_row};
use crate::shell::search_panel::rg_runner::{DEFAULT_MAX_RESULTS, SearchResults, run_ripgrep};
use crate::shell::search_panel::rows::{SearchRow, build_search_rows};
use crate::shell::search_panel::search_state::SearchOptions;

const DEBOUNCE: Duration = Duration::from_millis(300);

/// Result of the last completed (or cancelled) search.
#[derive(Debug, Clone)]
enum LastSearch {
    None,
    Loading,
    Ok(SearchResults),
    Err(String),
}

pub struct SearchPanel {
    repo_root: PathBuf,
    options: SearchOptions,
    last: LastSearch,
    collapsed_files: HashSet<PathBuf>,
    flat_rows: Vec<SearchRow>,
    list_scroll: UniformListScrollHandle,
    latest_search_id: u64,
    debounce_task: Option<Task<()>>,
    rg_available: bool,

    query_input: Entity<InputState>,
    include_input: Entity<InputState>,
    exclude_input: Entity<InputState>,

    theme: Theme,
    #[allow(dead_code)]
    density: Density,
    typography: Typography,

    _subscriptions: Vec<Subscription>,
}

impl SearchPanel {
    pub fn new(
        repo_root: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let include_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("files to include (e.g. *.ts, src/**)")
        });
        let exclude_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("files to exclude (e.g. *.min.js, dist/**)")
        });

        let subs = vec![
            cx.subscribe_in(
                &query_input,
                window,
                |me, _, ev: &InputEvent, window, cx| {
                    me.on_input_event(InputField::Query, ev, window, cx);
                },
            ),
            cx.subscribe_in(
                &include_input,
                window,
                |me, _, ev: &InputEvent, window, cx| {
                    me.on_input_event(InputField::Include, ev, window, cx);
                },
            ),
            cx.subscribe_in(
                &exclude_input,
                window,
                |me, _, ev: &InputEvent, window, cx| {
                    me.on_input_event(InputField::Exclude, ev, window, cx);
                },
            ),
        ];

        // Detect rg availability once at construction. Result is mirrored
        // into the entity via cx.spawn — until then, treat it as available
        // so the empty state doesn't flash incorrectly on first paint.
        let mut panel = Self {
            repo_root,
            options: SearchOptions::default(),
            last: LastSearch::None,
            collapsed_files: HashSet::new(),
            flat_rows: Vec::new(),
            list_scroll: UniformListScrollHandle::new(),
            latest_search_id: 0,
            debounce_task: None,
            rg_available: true,
            query_input,
            include_input,
            exclude_input,
            theme,
            density,
            typography,
            _subscriptions: subs,
        };
        panel.spawn_rg_probe(cx);
        panel
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn density(&self) -> Density {
        self.density
    }

    pub fn typography_ref(&self) -> &Typography {
        &self.typography
    }

    pub fn options(&self) -> &SearchOptions {
        &self.options
    }

    pub fn query_input_ref(&self) -> &Entity<InputState> {
        &self.query_input
    }

    pub fn include_input_ref(&self) -> &Entity<InputState> {
        &self.include_input
    }

    pub fn exclude_input_ref(&self) -> &Entity<InputState> {
        &self.exclude_input
    }

    pub(super) fn toggle_flag(
        &mut self,
        which: header_render::HeaderToggle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use header_render::HeaderToggle::*;
        match which {
            CaseSensitive => self.options.case_sensitive = !self.options.case_sensitive,
            WholeWord => self.options.whole_word = !self.options.whole_word,
            UseRegex => self.options.use_regex = !self.options.use_regex,
        }
        self.schedule_search(window, cx);
        cx.notify();
    }

    pub(super) fn toggle_file_collapse(&mut self, file_path: PathBuf, cx: &mut Context<Self>) {
        if !self.collapsed_files.remove(&file_path) {
            self.collapsed_files.insert(file_path);
        }
        self.recompute_rows();
        cx.notify();
    }

    pub(super) fn open_match(&self, file_path: &std::path::Path, cx: &gpui::App) {
        cx.open_with_system(file_path);
    }

    pub(super) fn last_results(&self) -> Option<&SearchResults> {
        if let LastSearch::Ok(r) = &self.last {
            Some(r)
        } else {
            None
        }
    }

    pub(super) fn summary_state(&self) -> SummaryState<'_> {
        if !self.rg_available {
            return SummaryState::RgMissing;
        }
        match &self.last {
            LastSearch::Loading => SummaryState::Loading,
            LastSearch::Ok(r) if !r.files.is_empty() => SummaryState::Results(r),
            LastSearch::Ok(_) if self.options.has_query() => SummaryState::Results(
                // Empty result set — keep the "0 results in 0 files" banner.
                empty_ref(),
            ),
            LastSearch::Ok(_) => SummaryState::Idle,
            LastSearch::Err(e) => SummaryState::Error(e.clone()),
            LastSearch::None => SummaryState::Idle,
        }
    }

    fn on_input_event(
        &mut self,
        field: InputField,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            InputEvent::Change => {
                let new_value = match field {
                    InputField::Query => self.query_input.read(cx).value().to_string(),
                    InputField::Include => self.include_input.read(cx).value().to_string(),
                    InputField::Exclude => self.exclude_input.read(cx).value().to_string(),
                };
                match field {
                    InputField::Query => self.options.query = new_value,
                    InputField::Include => self.options.include_glob = new_value,
                    InputField::Exclude => self.options.exclude_glob = new_value,
                }
                self.schedule_search(window, cx);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.run_now(window, cx),
            _ => {}
        }
    }

    fn schedule_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Empty query short-circuits — drop any pending task and clear results.
        if !self.options.has_query() {
            self.debounce_task = None;
            self.last = LastSearch::None;
            self.flat_rows.clear();
            return;
        }
        self.latest_search_id = self.latest_search_id.wrapping_add(1);
        let my_id = self.latest_search_id;
        self.last = LastSearch::Loading;
        let cwd = self.repo_root.clone();
        let opts = self.options.clone();
        self.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let result = run_ripgrep(cwd, opts, DEFAULT_MAX_RESULTS).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.latest_search_id != my_id {
                    return;
                }
                panel.apply_result(result, cx);
            });
        }));
    }

    fn run_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.options.has_query() {
            return;
        }
        self.latest_search_id = self.latest_search_id.wrapping_add(1);
        let my_id = self.latest_search_id;
        self.last = LastSearch::Loading;
        let cwd = self.repo_root.clone();
        let opts = self.options.clone();
        self.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = run_ripgrep(cwd, opts, DEFAULT_MAX_RESULTS).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.latest_search_id != my_id {
                    return;
                }
                panel.apply_result(result, cx);
            });
        }));
    }

    fn apply_result(
        &mut self,
        result: Result<SearchResults, rg_runner::RgError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(r) => self.last = LastSearch::Ok(r),
            Err(rg_runner::RgError::NotFound) => {
                self.rg_available = false;
                self.last = LastSearch::None;
            }
            Err(e) => self.last = LastSearch::Err(e.to_string()),
        }
        self.recompute_rows();
        cx.notify();
    }

    pub(super) fn is_loading(&self) -> bool {
        matches!(self.last, LastSearch::Loading)
    }

    pub(super) fn clear_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.options.query.clear();
        self.schedule_search(window, cx);
        cx.notify();
    }

    fn recompute_rows(&mut self) {
        let r = self.last_results();
        self.flat_rows = build_search_rows(r, &self.collapsed_files);
    }

    fn spawn_rg_probe(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let available = rg_runner::detect_rg_available().await;
            let _ = this.update(cx, |panel, cx| {
                panel.rg_available = available;
                cx.notify();
            });
        });
        // Detach: the probe is one-shot; dropping the task on next state change
        // is fine because it only writes a single bool.
        task.detach();
    }
}

#[derive(Clone, Copy)]
enum InputField {
    Query,
    Include,
    Exclude,
}

// Returns a stable, empty `SearchResults` for the `Results(...)` summary state
// when the live results are empty. `format_summary` still renders "0 results
// in 0 files" so the user knows the search ran.
fn empty_ref() -> &'static SearchResults {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<SearchResults> = OnceLock::new();
    EMPTY.get_or_init(SearchResults::default)
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
        let theme = self.theme;
        let header = render_header(self, cx);

        // Rows list — uniform_list keeps virtualisation cheap.
        let row_count = self.flat_rows.len();
        let list = uniform_list(
            "search-panel-results",
            row_count,
            cx.processor(
                |me: &mut SearchPanel,
                 range: std::ops::Range<usize>,
                 _window: &mut Window,
                 cx: &mut Context<SearchPanel>| {
                    let flat = me.flat_rows.clone();
                    let results = me.last_results().cloned();
                    let ctx = RowPaintCtx {
                        theme: me.theme,
                        typography: me.typography.clone(),
                    };
                    range
                        .map(|i| match &flat[i] {
                            SearchRow::File {
                                file_index,
                                match_count,
                                collapsed,
                            } => paint_file_row(
                                results.as_ref(),
                                *file_index,
                                *match_count,
                                *collapsed,
                                &ctx,
                                cx,
                            ),
                            SearchRow::Match {
                                file_index,
                                match_index,
                            } => paint_match_row(
                                results.as_ref(),
                                *file_index,
                                *match_index,
                                &ctx,
                                cx,
                            ),
                        })
                        .collect::<Vec<AnyElement>>()
                },
            ),
        )
        .track_scroll(&self.list_scroll)
        .h_full()
        .w_full();

        let body: AnyElement = if !self.rg_available {
            // Empty body — summary banner already explains the install hint.
            div().flex_1().w_full().into_any_element()
        } else if !self.options.has_query() {
            div()
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.fg_subtle)
                .text_size(px(self.typography.t_body_sm))
                .child("Type to search files in workspace.")
                .into_any_element()
        } else if matches!(self.last, LastSearch::Ok(ref r) if r.files.is_empty()) {
            div()
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.fg_subtle)
                .text_size(px(self.typography.t_body_sm))
                .child("No matches.")
                .into_any_element()
        } else {
            // Stateful + relative so the gpui-component vertical scrollbar
            // overlay anchors against this container. uniform_list already
            // virtualises rows; the scrollbar just paints the visible track.
            div()
                .id("search-results-pane")
                .relative()
                .flex_1()
                .w_full()
                .child(list)
                .vertical_scrollbar(&self.list_scroll)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_panel)
            .child(header)
            .child(body)
            .into_any_element()
    }
}
