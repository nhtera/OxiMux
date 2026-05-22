//! `PaneGroup` — one tab-strip leaf in the workspace's group layout tree.
//!
//! Each `PaneGroup` is a single tab strip with one active tab + its
//! content. The workspace owns a tree of these via `PaneGroupManager`;
//! splitting creates a new sibling group beside (or above/below) the
//! focused one. Each group is independent: opening a file in one group
//! does NOT affect any other group's tab list.
//!
//! This is what `WorkspaceTabs` used to be in the pre-step-06 design —
//! the surface API is similar (open_tab / close_tab / set_active /
//! focus_active) but the entity is now per-leaf instead of per-project.
//! Persistence wiring + render code land in later steps; this file
//! holds the data model + mutation API only.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AppContext, Context, FocusHandle, Focusable, SharedString, Subscription, Task, Window,
};
use oximux_agents::{AgentRuntime, AgentStatusStream, CliRuntime, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::main_pane::PaneContent;
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};

/// Discriminator for `PaneGroupTab` carrying any per-kind metadata.
/// `PaneContent` answers "terminal vs editor" at the view level; this
/// enum carries the higher-level data (the file path for editor tabs;
/// the agent session metadata for agent tabs).
pub enum PaneGroupTabKind {
    /// User-spawned shell terminal. No extra metadata; the content
    /// `TerminalView` owns the session id.
    Terminal,
    /// Agent CLI tab. Carries the runtime session id + status stream so
    /// the host can subscribe + cancel on tab close.
    Agent {
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
    },
    /// File editor tab. Path doubles as the dedup key for "is this file
    /// already open?" lookups.
    Editor { path: PathBuf },
}

/// A single tab inside a `PaneGroup`. Bundles the label + content +
/// kind metadata. The `_observer` keeps `cx.observe` alive for the
/// lifetime of the tab so content notifies propagate to the group.
pub struct PaneGroupTab {
    pub label: SharedString,
    pub content: PaneContent,
    pub kind: PaneGroupTabKind,
    /// `cx.observe(content_entity, ...)` subscription — dropping
    /// unregisters the observer. `Option` because tests can skip it.
    pub _observer: Option<Subscription>,
    /// Per-tab status task (agent kinds only; `None` for terminal /
    /// editor). Dropped on tab close, which cancels the future.
    pub _status_task: Option<Task<()>>,
}

pub struct PaneGroup {
    tabs: Vec<PaneGroupTab>,
    active: usize,
    focus_handle: FocusHandle,
    /// Monotonic counter for default terminal labels ("Terminal 1",
    /// "Terminal 2", ...). Scoped per group — restart-from-1 inside a
    /// new group is fine, matches the industry-standard editor
    /// convention.
    next_terminal_n: u64,
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) typography: Typography,
    pub(crate) cwd: PathBuf,
    pub(crate) cli_runtime: Arc<CliRuntime>,
}

impl PaneGroup {
    /// Build an empty group. Caller usually follows up with
    /// `open_terminal_tab` to seed it; an empty group renders as a
    /// blank panel (no welcome view at this level — that's host
    /// concern).
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            focus_handle: cx.focus_handle(),
            next_terminal_n: 1,
            theme,
            density,
            typography,
            cwd,
            cli_runtime,
        }
    }

    /// Tab list (read-only). Render code walks this.
    pub fn tabs(&self) -> &[PaneGroupTab] {
        &self.tabs
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> Option<&PaneGroupTab> {
        self.tabs.get(self.active)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Spawn a fresh shell terminal and append it as a new tab. Returns
    /// the index of the new tab; `None` if the PTY spawn failed.
    pub fn open_terminal_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let (backend, session_id) = spawn_local_pty(self.cwd.clone())?;
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(backend, session_id, theme, density, typography, window, cx)
        });
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let n = self.next_terminal_n;
        self.next_terminal_n += 1;
        let tab = PaneGroupTab {
            label: SharedString::from(format!("Terminal {n}")),
            content: PaneContent::Terminal(view),
            kind: PaneGroupTabKind::Terminal,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
        Some(self.active)
    }

    /// Open `path` as an editor tab, or activate the existing tab if
    /// it's already open. Returns the index of the activated tab.
    /// Caller is expected to pre-filter unopenable files (see
    /// `openable_text_file::is_openable_text_file`).
    pub fn open_or_activate_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self.tabs.iter().position(
            |t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == &path),
        ) {
            self.active = idx;
            self.focus_active(window, cx);
            cx.notify();
            return idx;
        }
        let path_for_view = path.clone();
        let view = cx.new(|cx| oximux_editor::EditorView::new(path_for_view, window, cx));
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled")
            .to_string();
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Editor(view),
            kind: PaneGroupTabKind::Editor { path },
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
        self.active
    }

    /// Append a pre-built agent tab. Backend + session id are supplied
    /// by the caller (`WorkspaceRoot::spawn_agent_tab` runs the
    /// CliRuntime spawn off-thread before calling this).
    #[allow(clippy::too_many_arguments)]
    pub fn push_agent_tab(
        &mut self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        label_override: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(backend, term_id, theme, density, typography, window, cx)
        });
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let label = label_override.unwrap_or_else(|| adapter_id.to_string());
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(view),
            kind: PaneGroupTabKind::Agent {
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
            },
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
        self.active
    }

    /// Close the tab at `idx`. For agent tabs, spawns a detached
    /// `CliRuntime::cancel` so the agent process is reaped. Terminal
    /// tabs drop straight through to `TerminalView::Drop` (PTY close).
    /// Editor tabs drop to `EditorView::Drop` (LSP didClose).
    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(idx);
        if let PaneGroupTabKind::Agent { session_id, .. } = removed.kind {
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_this, _cx| {
                if let Err(err) = runtime.cancel(session_id).await {
                    tracing::warn!(?err, "pane-group close_tab: agent cancel failed");
                }
            })
            .detach();
        }
        if self.tabs.is_empty() {
            // Empty group — the host's PaneGroupManager is expected to
            // detect this and either spawn a fresh terminal in the
            // group or close the group entirely. Don't touch focus
            // here; the host owns that decision.
            self.active = 0;
            cx.notify();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn set_active(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.active = idx;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + 1) % self.tabs.len();
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Focus the active tab's content. Called whenever the active tab
    /// changes; also called by the host when activating this group so
    /// keystrokes route correctly. No-op if there are no tabs.
    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            // Empty group — focus the group container so action
            // keystrokes still bubble up to workspace handlers.
            self.focus_handle.focus(window, cx);
            return;
        };
        let handle = tab.content.focus_handle(cx);
        handle.focus(window, cx);
    }

    /// File path of the active editor tab (`None` for terminal/agent
    /// active or empty group). Drives the active-file highlight in the
    /// Files sidebar.
    pub fn active_editor_path(&self, cx: &gpui::App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.active)?;
        match &tab.content {
            PaneContent::Editor(view) => Some(view.read(cx).file_path().to_path_buf()),
            PaneContent::Terminal(_) => None,
        }
    }
}

impl Focusable for PaneGroup {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
