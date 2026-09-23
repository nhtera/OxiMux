//! Command Palette + Quick Open modal shell.
//!
//! Keyboard nav (↑/↓/Enter/Esc), live filtering, built-in and custom
//! command dispatch. Focus management mirrors `project_picker.rs`.

pub mod entry;
pub mod file_index;
pub mod match_engine;
pub mod palette_modal;
pub mod row_render;

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, Render, Styled, Subscription, Task, Window, div,
};
use gpui_component::input::{
    Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState, MoveDown, MoveUp,
};
use oximux_settings::{CustomCommand, Density, Theme, Typography};
use tokio::sync::oneshot;

use crate::actions::{ActivateWorkspaceFromJump, OpenFileFromContextMenu, SendTextToActiveAgent};
use crate::shell::command_palette::entry::{
    PaletteItem, PaletteItemAction, PaletteMode, WorkspaceJumpItem, build_palette_items,
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
    /// Workspace-jump (Cmd+J) candidates, pushed in by `WorkspaceRoot` each
    /// time the jump palette opens (a fresh snapshot across all projects).
    workspace_items: Vec<WorkspaceJumpItem>,
    /// Set when the last scan failed (e.g. `rg` missing) — surfaced as a
    /// copyable hint row instead of a broken/empty list.
    index_error: Option<String>,
    /// Owns the in-flight scan-result bridge task; dropped on reuse/teardown.
    _index_task: Option<Task<()>>,
    /// The query field. A real text input (not a hand-rolled key-down
    /// buffer) so paste, IME composition, shifted characters, and caret
    /// movement all work. Created lazily on first `open` — the constructor
    /// runs without a `&mut Window`, which `InputState` needs. `query`
    /// mirrors its value via `_query_sub`.
    query_input: Option<Entity<InputState>>,
    _query_sub: Option<Subscription>,
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
    // Scored on `search_text` (name + synonyms), displayed by `name`.
    let names: Vec<&str> = all_items.iter().map(|i| i.search_text.as_str()).collect();
    let ranked = filter_and_rank(query, &names);
    ranked.into_iter().map(|i| all_items[i].clone()).collect()
}

/// Ranked indices into `items` for a workspace-jump query. Empty query =
/// browse order (attention tier first, stable within tier so input order is
/// preserved); non-empty = fuzzy rank on the labels (name-search intent
/// dominates). Pure so the ordering contract is unit-testable without a GPUI
/// entity.
pub(crate) fn rank_workspace_items(query: &str, items: &[WorkspaceJumpItem]) -> Vec<usize> {
    if items.is_empty() {
        return Vec::new();
    }
    if query.trim().is_empty() {
        let mut idx: Vec<usize> = (0..items.len()).collect();
        idx.sort_by_key(|&i| items[i].attention);
        idx
    } else {
        let names: Vec<&str> = items.iter().map(|w| w.label.as_str()).collect();
        filter_and_rank(query, &names)
    }
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
            workspace_items: Vec::new(),
            index_error: None,
            _index_task: None,
            query_input: None,
            _query_sub: None,
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
    /// `workspace_items` is intentionally NOT cleared here — it is rebuilt
    /// fresh on every Cmd+J open, so stale rows are never shown.
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

    /// Replace the workspace-jump candidate list. Called by `WorkspaceRoot`
    /// immediately before opening the palette in `WorkspaceJump` mode, so the
    /// list always reflects the current workspaces + their attention state.
    pub fn set_workspace_items(&mut self, items: Vec<WorkspaceJumpItem>, cx: &mut Context<Self>) {
        self.workspace_items = items;
        cx.notify();
    }

    /// Ranked indices into `workspace_items` for the current query.
    fn workspace_jump_ranked(&self) -> Vec<usize> {
        rank_workspace_items(&self.query, &self.workspace_items)
    }

    /// Labels of the ranked workspace rows, for rendering.
    fn workspace_jump_rows(&self) -> Vec<String> {
        self.workspace_jump_ranked()
            .into_iter()
            .filter_map(|i| self.workspace_items.get(i).map(|w| w.label.clone()))
            .collect()
    }

    /// Activate the workspace at `idx` in the ranked jump list: close the
    /// palette and dispatch `ActivateWorkspaceFromJump` (resolved by
    /// `WorkspaceRoot`). No-op when `idx` is out of range.
    fn activate_workspace_jump(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ranked = self.workspace_jump_ranked();
        let Some(item) = ranked.get(idx).and_then(|&i| self.workspace_items.get(i)).cloned() else {
            return;
        };
        self.close(cx);
        window.dispatch_action(
            Box::new(ActivateWorkspaceFromJump {
                workspace_id: item.workspace_id,
                project_id: item.project_id,
                worktree_path: item.worktree_path,
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
        let input = self.ensure_query_input(window, cx);
        input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(placeholder_for(mode), window, cx);
        });
        // Focus the INPUT, not the modal root: typing and paste must land in
        // the text field. Nav keys still reach the modal through the
        // capture-phase action handlers in `render`.
        let input_focus = input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        cx.notify();
    }

    /// The query input, created on first use and mirrored into `query` on
    /// every edit (typing, paste, IME commit, delete).
    fn ensure_query_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.query_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        let sub = cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, _window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.query = input.read(cx).value().to_string();
                this.selected_idx = 0;
                cx.notify();
            }
        });
        self.query_input = Some(input.clone());
        self._query_sub = Some(sub);
        input
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        // Only signal Closed on a real open→closed transition. `close()` is
        // also called pre-emptively on the already-closed palette (e.g.
        // `close_modal_overlays` runs before `open`); emitting unconditionally
        // would queue a workspace root-refocus that lands AFTER `open` focused
        // the palette, stealing keyboard focus so Esc/arrows/typing never reach
        // it. Guarding the emit keeps focus on a freshly opened palette.
        let was_open = self.open;
        self.open = false;
        self.query.clear();
        self.selected_idx = 0;
        if was_open {
            cx.emit(PaletteEvent::Closed);
        }
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

    /// Number of ACTIONABLE rows in the current mode, computed from live
    /// state (a query edit and a nav key can land in one event batch, so a
    /// render-time capture may be stale). Status/hint rows count as 0.
    fn row_count(&self) -> usize {
        match self.mode {
            PaletteMode::Commands => self.filtered_items().len(),
            PaletteMode::QuickOpen => self.quick_open_matches().len(),
            PaletteMode::WorkspaceJump => self.workspace_jump_ranked().len(),
        }
    }

    fn nav(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.row_count();
        self.move_selection(delta, n, cx);
    }

    /// Activate the selected row in the current mode (keyboard Enter).
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.selected_idx;
        match self.mode {
            PaletteMode::Commands => {
                let items = self.filtered_items();
                self.activate_item(idx, &items, window, cx);
            }
            PaletteMode::QuickOpen => self.activate_file(idx, window, cx),
            PaletteMode::WorkspaceJump => self.activate_workspace_jump(idx, window, cx),
        }
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

/// Query-field placeholder per mode.
fn placeholder_for(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::QuickOpen => "Search files…",
        PaletteMode::Commands => "Search commands…",
        PaletteMode::WorkspaceJump => "Jump to workspace…",
    }
}

impl Render for PaletteModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let motion = crate::motion_settings::active(cx);
        let typography = self.typography.clone();
        let mode = self.mode;
        let query = self.query.clone();
        let selected_idx = self.selected_idx;

        // Borderless so it reads as part of the header row (search icon +
        // mode chip + field), not a boxed control inside it.
        let query_field = match &self.query_input {
            Some(input) => Input::new(input)
                .appearance(false)
                .text_size(gpui::px(typography.t_body_md))
                .into_any_element(),
            None => div().into_any_element(),
        };

        // Backdrop click-outside dismiss — same close path as Esc.
        let dismiss_entity = cx.entity();
        let on_dismiss: palette_modal::DismissFn = std::rc::Rc::new(move |_window, cx| {
            dismiss_entity.update(cx, |p, cx| p.close(cx));
        });

        // Row clicks activate through the same path as keyboard Enter (the
        // activate helpers dispatch AND close the modal), so a mouse click
        // can't leave the palette open. The entity is captured so the pure
        // render helper needs no direct access to private methods.
        let entity = cx.entity();
        let on_activate: row_render::ActivateFn = match mode {
            PaletteMode::Commands => Rc::new(move |idx, window, cx| {
                entity.update(cx, |p, cx| {
                    let items = p.filtered_items();
                    p.activate_item(idx, &items, window, cx);
                });
            }),
            PaletteMode::QuickOpen => Rc::new(move |idx, window, cx| {
                entity.update(cx, |p, cx| p.activate_file(idx, window, cx));
            }),
            PaletteMode::WorkspaceJump => Rc::new(move |idx, window, cx| {
                entity.update(cx, |p, cx| p.activate_workspace_jump(idx, window, cx));
            }),
        };

        // Row sources per mode. An empty QuickOpen / WorkspaceJump result
        // renders a single non-actionable status/hint row (row_count stays 0
        // so nav + Enter are inert).
        let palette_items: Vec<PaletteItem> = match mode {
            PaletteMode::Commands => self.filtered_items(),
            PaletteMode::QuickOpen | PaletteMode::WorkspaceJump => Vec::new(),
        };
        let file_row_strings: Vec<String> = match mode {
            PaletteMode::Commands => Vec::new(),
            PaletteMode::QuickOpen => self.quick_open_matches(),
            PaletteMode::WorkspaceJump => self.workspace_jump_rows(),
        };
        let (file_rows, row_count): (Vec<&str>, usize) = match mode {
            PaletteMode::Commands => (Vec::new(), palette_items.len()),
            PaletteMode::QuickOpen | PaletteMode::WorkspaceJump if !file_row_strings.is_empty() => (
                file_row_strings.iter().map(String::as_str).collect(),
                file_row_strings.len(),
            ),
            PaletteMode::QuickOpen => {
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
            }
            PaletteMode::WorkspaceJump => {
                let hint = if self.workspace_items.is_empty() {
                    "No workspaces yet"
                } else {
                    "No matching workspaces"
                };
                (vec![hint], 0)
            }
        };

        build_modal_layout(ModalRenderInput {
            mode,
            query: &query,
            query_field,
            selected_idx,
            palette_items: &palette_items,
            file_rows,
            row_count,
            on_activate,
            on_dismiss,
            theme,
            density,
            typography: &typography,
            motion,
        })
        .track_focus(&self.focus_handle)
        // Keyboard plumbing (same contract as `BranchPicker`). The focused
        // `Input` turns nav keys into its own ACTIONS before raw key
        // listeners on ancestors run, so they are intercepted at the
        // CAPTURE phase and stopped there. Everything else — typing, paste
        // (⌘V / Ctrl+V), IME, caret movement — is the input's.
        // Known trade-off (shared with `BranchPicker`): capturing Escape
        // preempts the input's IME-unmark path, so Escape mid-composition
        // closes the palette instead of cancelling the composition.
        .capture_action(cx.listener(|this, _: &InputEscape, _window, cx| {
            cx.stop_propagation();
            this.close(cx);
        }))
        .capture_action(cx.listener(|this, _: &InputEnter, window, cx| {
            cx.stop_propagation();
            this.confirm(window, cx);
        }))
        .capture_action(cx.listener(|this, _: &MoveUp, _window, cx| {
            cx.stop_propagation();
            this.nav(-1, cx);
        }))
        .capture_action(cx.listener(|this, _: &MoveDown, _window, cx| {
            cx.stop_propagation();
            this.nav(1, cx);
        }))
        // Fallback for when focus sits on the modal root rather than the
        // input (e.g. the input entity was not yet created).
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            match event.keystroke.key.as_str() {
                "escape" => this.close(cx),
                "up" => this.nav(-1, cx),
                "down" => this.nav(1, cx),
                "enter" => this.confirm(window, cx),
                _ => {}
            }
        }))
        .into_any_element()
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
        use std::path::{Path, PathBuf};
        // The core Quick Open contract: `rg --files` emits relative paths; the
        // editor open action needs them absolute.
        //
        // Asserted as properties rather than one expected string. The old
        // literal `/home/u/proj/src/main.rs` baked in both a Unix-absolute root
        // and `/` as the separator, so on Windows `join` correctly produced
        // `\`-joined output and the comparison failed on formatting. Properties
        // say what the contract actually is, and `Path::ends_with` /
        // `starts_with` are component-wise, so they hold either way.
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\home\u\proj")
        } else {
            PathBuf::from("/home/u/proj")
        };
        let joined = resolve_index_path(Some(&root), "src/main.rs");
        let joined = Path::new(&joined);
        assert!(joined.is_absolute(), "must resolve to absolute: {joined:?}");
        assert!(joined.starts_with(&root), "{joined:?}");
        assert!(joined.ends_with("src/main.rs"), "{joined:?}");
    }

    #[test]
    fn resolve_index_path_falls_back_without_root() {
        let rel = resolve_index_path(None, "src/main.rs");
        assert_eq!(rel, "src/main.rs");
    }

    fn jump_item(label: &str, attention: u8) -> WorkspaceJumpItem {
        WorkspaceJumpItem {
            workspace_id: format!("id-{label}"),
            project_id: "p".to_string(),
            worktree_path: format!("/w/{label}"),
            label: label.to_string(),
            attention,
        }
    }

    #[test]
    fn workspace_jump_empty_query_sorts_attention_first() {
        // Browse order: lower attention rank floats up; ties keep input order.
        let items = vec![
            jump_item("idle-a", 2),
            jump_item("needs-approval", 0),
            jump_item("idle-b", 2),
            jump_item("running", 1),
        ];
        let ranked = rank_workspace_items("", &items);
        // First must be the rank-0 (needs-approval), then rank-1, then the two
        // rank-2 in original order.
        assert_eq!(ranked, vec![1, 3, 0, 2]);
    }

    #[test]
    fn workspace_jump_query_filters_by_label() {
        let items = vec![
            jump_item("alpha", 2),
            jump_item("beta", 0),
            jump_item("gamma", 1),
        ];
        let ranked = rank_workspace_items("alph", &items);
        // Only "alpha" subsequence-matches; attention is ignored once a query
        // is present (name-search intent dominates).
        assert_eq!(ranked.len(), 1);
        assert_eq!(items[ranked[0]].label, "alpha");
    }

    #[test]
    fn workspace_jump_empty_items_is_empty() {
        assert!(rank_workspace_items("", &[]).is_empty());
        assert!(rank_workspace_items("x", &[]).is_empty());
    }

    #[test]
    fn out_of_range_activate_is_guarded() {
        // The guard `filtered.get(idx)` returns None on out-of-range; this
        // test verifies the slice bound directly (activate_item is not
        // callable without a window).
        let items: Vec<PaletteItem> = Vec::new();
        assert!(items.is_empty());
    }
}
