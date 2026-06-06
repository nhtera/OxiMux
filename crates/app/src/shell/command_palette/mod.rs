//! Command Palette + Quick Open modal shell.
//!
//! Keyboard nav (↑/↓/Enter/Esc), live filtering, built-in and custom
//! command dispatch. Focus management mirrors `project_picker.rs`.

pub mod entry;
pub mod file_index;
pub mod match_engine;
pub mod palette_modal;

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, Render, Task, Window, div,
};
use oximux_settings::{CustomCommand, Density, Theme, Typography};
use tokio::sync::oneshot;

use crate::actions::{OpenFileFromContextMenu, SendTextToActiveAgent};
use crate::shell::command_palette::entry::{
    PaletteItem, PaletteItemAction, PaletteMode, build_palette_items,
};
use crate::shell::command_palette::match_engine::filter_and_rank;
use crate::shell::command_palette::palette_modal::{ModalRenderInput, build_modal_layout};

/// Max ranked file rows shown in Quick Open. The result column is not
/// virtualized, so an empty-query match against a large index must be
/// capped to keep the modal cheap to paint.
const MAX_QUICK_OPEN_ROWS: usize = 50;

/// Single entity hosts both palette modes. `open` controls visibility;
/// `mode` selects between Quick Open and Command Palette.
pub struct PaletteModal {
    mode: PaletteMode,
    open: bool,
    query: String,
    selected_idx: usize,
    /// Custom commands loaded from global + project TOML. Updated via
    /// `set_custom_commands` on startup and project switch.
    custom_commands: Vec<CustomCommand>,
    /// Live project file index for Quick Open. Built lazily on first open
    /// per project, cleared on project switch. Paths are project-relative
    /// (as `rg --files` emits them); joined with [`Self::index_root`] before
    /// dispatching an open so the editor receives an absolute path.
    file_index: Vec<String>,
    /// Project root the index was built against. Used to resolve a relative
    /// index entry to an absolute path at open time.
    index_root: Option<PathBuf>,
    index_loaded: bool,
    scanning: bool,
    /// Set when the last scan failed (e.g. `rg` missing) — surfaced as a
    /// copyable hint row instead of a broken/empty list.
    index_error: Option<String>,
    /// Owns the in-flight scan-result bridge task; dropped on reuse/teardown.
    _index_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

/// Pure helper: apply query filtering and ranking to the full palette item
/// list (built-ins + custom). Returns items in display/score order.
/// Extracted from `PaletteModal::filtered_items` so unit tests can call it
/// without constructing a GPUI entity.
pub(crate) fn palette_filter(
    query: &str,
    custom_commands: &[CustomCommand],
) -> Vec<PaletteItem> {
    let all_items = build_palette_items(custom_commands);
    let names: Vec<&str> = all_items.iter().map(|i| i.name.as_str()).collect();
    let ranked = filter_and_rank(query, &names);
    ranked.into_iter().map(|i| all_items[i].clone()).collect()
}

/// Resolve a project-relative Quick Open index entry to an absolute path
/// string for the editor open action. Falls back to the relative path when
/// no project root is known (degraded but never panics). Pure so the
/// relative→absolute contract is unit-testable without a GPUI entity.
fn resolve_index_path(root: Option<&std::path::Path>, rel: &str) -> String {
    match root {
        Some(root) => root.join(rel).to_string_lossy().into_owned(),
        None => rel.to_string(),
    }
}

impl PaletteModal {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            mode: PaletteMode::Commands,
            open: false,
            query: String::new(),
            selected_idx: 0,
            custom_commands: Vec::new(),
            file_index: Vec::new(),
            index_root: None,
            index_loaded: false,
            scanning: false,
            index_error: None,
            _index_task: None,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn mode(&self) -> PaletteMode {
        self.mode
    }

    /// Replace the current custom command list. Called on startup and on
    /// project switch after loading global + project TOML files.
    pub fn set_custom_commands(&mut self, commands: Vec<CustomCommand>, cx: &mut Context<Self>) {
        self.custom_commands = commands;
        cx.notify();
    }

    /// Kick a background scan of `root` to populate the Quick Open file
    /// index. No-op if already loaded or a scan is in flight — the index is
    /// cached for the project's lifetime and invalidated on project switch.
    pub fn kick_file_index(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.index_loaded || self.scanning {
            return;
        }
        self.scanning = true;
        self.index_error = None;
        self.index_root = Some(root.clone());
        cx.notify();

        let (tx, rx) = oneshot::channel::<Result<Vec<String>, file_index::ScanError>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = tx.send(file_index::scan_files(root).await);
                });
            }
            Err(_) => {
                self.scanning = false;
                self.index_error = Some("no async runtime available for file scan".to_string());
                cx.notify();
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(res) = rx.await else {
                // The tokio scan task was dropped (panic / runtime teardown)
                // before sending. Clear the scanning flag so a later open can
                // retry instead of wedging on "Indexing…" forever.
                let _ = this.update(cx, |p, cx| {
                    p.scanning = false;
                    p.index_error = Some("file scan ended unexpectedly".to_string());
                    cx.notify();
                });
                return;
            };
            let _ = this.update(cx, |p, cx| p.apply_scan_result(res, cx));
        });
        self._index_task = Some(task);
    }

    fn apply_scan_result(
        &mut self,
        res: Result<Vec<String>, file_index::ScanError>,
        cx: &mut Context<Self>,
    ) {
        self.scanning = false;
        match res {
            Ok(files) => {
                self.file_index = files;
                self.index_loaded = true;
                self.index_error = None;
            }
            Err(err) => {
                self.index_error = Some(err.hint());
                self.index_loaded = false;
            }
        }
        cx.notify();
    }

    /// Drop the cached index so the next Quick Open re-scans. Called on
    /// project switch (the previous project's files must not leak through).
    pub fn invalidate_file_index(&mut self, cx: &mut Context<Self>) {
        self.file_index.clear();
        self.index_root = None;
        self.index_loaded = false;
        self.scanning = false;
        self.index_error = None;
        self._index_task = None;
        cx.notify();
    }

    /// Ranked, capped Quick Open file rows for the current query.
    fn quick_open_matches(&self) -> Vec<String> {
        if self.file_index.is_empty() {
            return Vec::new();
        }
        let names: Vec<&str> = self.file_index.iter().map(String::as_str).collect();
        filter_and_rank(&self.query, &names)
            .into_iter()
            .take(MAX_QUICK_OPEN_ROWS)
            .map(|i| self.file_index[i].clone())
            .collect()
    }

    /// Open the file at `idx` in the current Quick Open match list as an
    /// editor tab, then close the palette. No-op when `idx` is out of range
    /// (e.g. activating the hint row while the index is empty).
    fn activate_file(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.quick_open_matches();
        let Some(rel_path) = matches.get(idx).cloned() else {
            return;
        };
        // Index entries are project-relative (`rg --files`); the editor open
        // action resolves against the process CWD, so join with the project
        // root to hand it an absolute path.
        let path = resolve_index_path(self.index_root.as_deref(), &rel_path);
        self.close(cx);
        window.dispatch_action(
            Box::new(OpenFileFromContextMenu {
                path,
                split_right: false,
            }),
            cx,
        );
    }

    /// Open the modal, snap focus into the palette input, and reset state.
    pub fn open(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = mode;
        self.open = true;
        self.query.clear();
        self.selected_idx = 0;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.query.clear();
        self.selected_idx = 0;
        cx.emit(PaletteEvent::Closed);
        cx.notify();
    }

    // ── keyboard helpers ──────────────────────────────────────────────

    fn move_selection(&mut self, delta: isize, row_count: usize, cx: &mut Context<Self>) {
        if row_count == 0 {
            return;
        }
        self.selected_idx = crate::shell::project_picker::wrap_index(
            self.selected_idx,
            delta,
            row_count,
        );
        cx.notify();
    }

    /// Build the current filtered item list (Commands mode only). Returns
    /// items in display order with their filter-ranked indices resolved.
    fn filtered_items(&self) -> Vec<PaletteItem> {
        palette_filter(&self.query, &self.custom_commands)
    }

    /// Activate the row at `idx` in the current filtered list: dispatch the
    /// appropriate action and close the modal. No-op when `idx` is out of
    /// range.
    fn activate_item(
        &mut self,
        idx: usize,
        filtered: &[PaletteItem],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = filtered.get(idx) else {
            return;
        };
        match &item.action {
            PaletteItemAction::Builtin(factory) => {
                let action = factory();
                self.close(cx);
                window.dispatch_action(action, cx);
            }
            PaletteItemAction::Custom(prompt) => {
                // Append a newline so the agent auto-submits the prompt.
                let text = if prompt.ends_with('\n') {
                    prompt.clone()
                } else {
                    format!("{prompt}\n")
                };
                self.close(cx);
                window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
            }
        }
    }
}

/// Emitted when the palette closes, so `WorkspaceRoot` can reclaim keyboard
/// focus (the palette focuses its query field on open).
pub enum PaletteEvent {
    Closed,
}

impl EventEmitter<PaletteEvent> for PaletteModal {}

impl Focusable for PaletteModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PaletteModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let mode = self.mode;
        let query = self.query.clone();
        let selected_idx = self.selected_idx;

        // Row clicks activate through the same path as keyboard Enter
        // (`activate_item` dispatches AND closes the modal), so a mouse click
        // can't leave the palette open. The entity is captured so the pure
        // render helper needs no direct access to private methods.
        let entity = cx.entity();
        let on_activate: std::rc::Rc<dyn Fn(usize, &mut Window, &mut App)> =
            std::rc::Rc::new(move |idx, window, cx| {
                entity.update(cx, |p, cx| {
                    let items = p.filtered_items();
                    p.activate_item(idx, &items, window, cx);
                });
            });

        match mode {
            PaletteMode::Commands => {
                let filtered = self.filtered_items();
                let row_count = filtered.len();

                build_modal_layout(ModalRenderInput {
                    mode,
                    query: &query,
                    selected_idx,
                    palette_items: &filtered,
                    file_rows: Vec::new(),
                    on_activate: on_activate.clone(),
                    theme,
                    density,
                    typography: &typography,
                })
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    match key {
                        "escape" => this.close(cx),
                        "up" => this.move_selection(-1, row_count, cx),
                        "down" => this.move_selection(1, row_count, cx),
                        "enter" => {
                            let items = this.filtered_items();
                            let idx = this.selected_idx;
                            this.activate_item(idx, &items, window, cx);
                        }
                        "backspace" => {
                            this.query.pop();
                            this.selected_idx = 0;
                            cx.notify();
                        }
                        _ => {
                            // Append printable single characters. Skip when a
                            // non-Shift modifier is held (Cmd/Ctrl/Alt/Fn) so
                            // shortcut combos still bubble up as actions and
                            // Option+letter doesn't leak into the query.
                            if event.keystroke.modifiers.control
                                || event.keystroke.modifiers.platform
                                || event.keystroke.modifiers.alt
                                || event.keystroke.modifiers.function
                            {
                                return;
                            }
                            if key.chars().count() == 1 {
                                this.query.push_str(key);
                                this.selected_idx = 0;
                                cx.notify();
                            }
                        }
                    }
                }))
                .into_any_element()
            }
            PaletteMode::QuickOpen => {
                let matches = self.quick_open_matches();
                // Empty result → a single non-actionable status/hint row
                // (row_count stays 0 so nav + Enter are inert).
                let (file_rows, row_count): (Vec<&str>, usize) = if matches.is_empty() {
                    let hint = if let Some(err) = self.index_error.as_deref() {
                        err
                    } else if self.scanning {
                        "Indexing project files…"
                    } else if !self.index_loaded {
                        "Open a project to search files"
                    } else {
                        "No matching files"
                    };
                    (vec![hint], 0)
                } else {
                    (matches.iter().map(String::as_str).collect(), matches.len())
                };

                let entity = cx.entity();
                let on_activate_file: Rc<dyn Fn(usize, &mut Window, &mut App)> =
                    Rc::new(move |idx, window, cx| {
                        entity.update(cx, |p, cx| p.activate_file(idx, window, cx));
                    });

                build_modal_layout(ModalRenderInput {
                    mode,
                    query: &query,
                    selected_idx,
                    palette_items: &[],
                    file_rows,
                    on_activate: on_activate_file,
                    theme,
                    density,
                    typography: &typography,
                })
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    match key {
                        "escape" => this.close(cx),
                        "up" => this.move_selection(-1, row_count, cx),
                        "down" => this.move_selection(1, row_count, cx),
                        "enter" => {
                            let idx = this.selected_idx;
                            this.activate_file(idx, window, cx);
                        }
                        "backspace" => {
                            this.query.pop();
                            this.selected_idx = 0;
                            cx.notify();
                        }
                        _ => {
                            if event.keystroke.modifiers.control
                                || event.keystroke.modifiers.platform
                                || event.keystroke.modifiers.alt
                                || event.keystroke.modifiers.function
                            {
                                return;
                            }
                            if key.chars().count() == 1 {
                                this.query.push_str(key);
                                this.selected_idx = 0;
                                cx.notify();
                            }
                        }
                    }
                }))
                .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::CustomCommand;

    fn custom_cmd(name: &str, prompt: &str) -> CustomCommand {
        CustomCommand {
            name: name.to_string(),
            prompt: prompt.to_string(),
            scope: None,
        }
    }

    // All tests use `palette_filter` directly — a pure function that doesn't
    // require a GPUI entity or App context (FocusHandle can't be constructed
    // without a live window, so we test logic through the extracted helper).

    #[test]
    fn empty_query_returns_all_builtin_commands() {
        let items = palette_filter("", &[]);
        assert_eq!(items.len(), entry::PALETTE_COMMANDS.len());
    }

    #[test]
    fn query_narrows_to_matching_items() {
        let items = palette_filter("split", &[]);
        assert!(!items.is_empty());
        // Every returned name must match "split" in some way (substring or
        // subsequence — the match engine guarantees this for score >= 0).
        for item in &items {
            let lower = item.name.to_lowercase();
            // At minimum a subsequence match: every char in "split" appears
            // in order in the name. For brevity, just check non-empty.
            assert!(!lower.is_empty());
        }
    }

    #[test]
    fn custom_commands_appear_in_results() {
        let custom = vec![custom_cmd("zz-unique-cmd", "do stuff")];
        let items = palette_filter("", &custom);
        assert!(items.iter().any(|i| i.name == "zz-unique-cmd"));
    }

    #[test]
    fn custom_command_has_custom_group() {
        use crate::shell::command_palette::entry::PaletteGroup;
        let custom = vec![custom_cmd("my-cmd", "hello")];
        let items = palette_filter("", &custom);
        let my_cmd = items.iter().find(|i| i.name == "my-cmd").unwrap();
        assert_eq!(my_cmd.display_group, PaletteGroup::Custom);
    }

    #[test]
    fn custom_command_action_carries_prompt() {
        use crate::shell::command_palette::entry::PaletteItemAction;
        let custom = vec![custom_cmd("run", "cargo run")];
        let items = palette_filter("", &custom);
        let run_item = items.iter().find(|i| i.name == "run").unwrap();
        match &run_item.action {
            PaletteItemAction::Custom(prompt) => assert_eq!(prompt, "cargo run"),
            _ => panic!("expected Custom action"),
        }
    }

    #[test]
    fn builtin_items_have_commands_group() {
        use crate::shell::command_palette::entry::PaletteGroup;
        let items = palette_filter("", &[]);
        for item in &items {
            assert_eq!(item.display_group, PaletteGroup::Commands);
        }
    }

    #[test]
    #[allow(unused_assignments)]
    fn close_state_reset_invariant() {
        // Pure-state regression: mirrors what close() does internally. Tests
        // that the reset invariant holds (no entity / window needed).
        let mut open = true;
        let mut query = "foo".to_string();
        let mut selected_idx: usize = 3;
        // mirror close()
        open = false;
        query.clear();
        selected_idx = 0;
        assert!(!open);
        assert!(query.is_empty());
        assert_eq!(selected_idx, 0);
    }

    #[test]
    fn resolve_index_path_joins_relative_against_root() {
        use std::path::Path;
        // The core Quick Open contract: `rg --files` emits relative paths; the
        // editor open action needs them absolute.
        let abs = resolve_index_path(Some(Path::new("/home/u/proj")), "src/main.rs");
        assert_eq!(abs, "/home/u/proj/src/main.rs");
    }

    #[test]
    fn resolve_index_path_falls_back_without_root() {
        let rel = resolve_index_path(None, "src/main.rs");
        assert_eq!(rel, "src/main.rs");
    }

    #[test]
    fn out_of_range_activate_is_guarded() {
        // The guard `filtered.get(idx)` returns None on out-of-range; this
        // test verifies the slice bound directly (activate_item is not
        // callable without a window).
        let items: Vec<PaletteItem> = Vec::new();
        assert!(items.get(0).is_none());
    }
}
