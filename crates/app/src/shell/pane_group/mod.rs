//! `PaneGroup` — one tab-strip leaf in the workspace's group layout tree.
//!
//! Each `PaneGroup` is a single tab strip with one active tab + its
//! content. The workspace owns a tree of these via `PaneGroupManager`;
//! splitting creates a new sibling group beside (or above/below) the
//! focused one. Each group is independent: opening a file in one group
//! does NOT affect any other group's tab list.

mod render;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gpui::{
    AppContext, Context, FocusHandle, Focusable, SharedString, Subscription, Task, Window,
};
use oximux_agents::{AgentRuntime, AgentStatusStream, CliRuntime, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{CloseTab, NewAgent, NewTab, NextTab, PrevTab, RequestOpenAdapterPicker};
use crate::notifier::{Notifier, TabId};
use crate::shell::agent_status_task::spawn_status_task;
use crate::shell::agent_tab_label;
use crate::shell::pane_content::PaneContent;
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};

/// Discriminator for `PaneGroupTab` carrying any per-kind metadata.
pub enum PaneGroupTabKind {
    Terminal,
    Agent {
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
    },
    Editor { path: PathBuf },
}

pub struct PaneGroupTab {
    pub label: SharedString,
    pub content: PaneContent,
    pub kind: PaneGroupTabKind,
    pub _observer: Option<Subscription>,
    pub _status_task: Option<Task<()>>,
}

pub struct PaneGroup {
    tabs: Vec<PaneGroupTab>,
    active: usize,
    focus_handle: FocusHandle,
    /// Monotonic counter for default terminal labels, scoped per group.
    next_terminal_n: u64,
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) typography: Typography,
    pub(crate) cwd: PathBuf,
    pub(crate) cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    /// Shared with the owning `ProjectPanes` window-activation observer
    /// so per-tab status watchers read the same flag.
    window_active: Arc<AtomicBool>,
    /// Chrome width in window pixels (rail + sidebar) — forwarded by the
    /// workspace so terminal grid dispatch can compute target area.
    chrome_w_px: f32,
}

impl PaneGroup {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window_active: Arc<AtomicBool>,
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
            notifier,
            window_active,
            chrome_w_px: density.w_left_rail,
        }
    }

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

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn agent_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.kind, PaneGroupTabKind::Agent { .. }))
            .count()
    }

    pub(crate) fn chrome_w_px(&self) -> f32 {
        self.chrome_w_px
    }

    pub(crate) fn focus_handle_clone(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_chrome_width(&mut self, new_chrome: f32, cx: &mut Context<Self>) {
        if (self.chrome_w_px - new_chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = new_chrome;
        cx.notify();
    }

    /// Append a freshly-spawned shell terminal as a new tab. Returns the
    /// index of the new tab; `None` if PTY spawn failed.
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
        let label = match label_override {
            Some(s) => SharedString::from(s),
            None => {
                let current_labels: Vec<SharedString> =
                    self.tabs.iter().map(|t| t.label.clone()).collect();
                agent_tab_label::next_label_for(adapter_id, &current_labels)
            }
        };
        let status_task = spawn_status_task(
            status_rx.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            TabId::from(session_id),
            label.clone(),
            cx,
        );
        self.tabs.push(PaneGroupTab {
            label,
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
            _status_task: Some(status_task),
        });
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
        self.active
    }

    /// Append a pre-built terminal tab (used by the restore path).
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        view: gpui::Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        self.tabs.push(PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(view),
            kind: PaneGroupTabKind::Terminal,
            _observer: observer,
            _status_task: None,
        });
        cx.notify();
    }

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
            self.active = 0;
            self.focus_handle.focus(window, cx);
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
        if idx == self.active {
            return;
        }
        self.active = idx;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| match &t.kind {
            PaneGroupTabKind::Agent { session_id, .. } => TabId::from(*session_id) == tab_id,
            PaneGroupTabKind::Terminal | PaneGroupTabKind::Editor { .. } => false,
        }) else {
            return false;
        };
        self.set_active(idx, window, cx);
        true
    }

    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = (self.active + 1) % self.tabs.len();
        self.set_active(next, window, cx);
    }

    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let prev = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.set_active(prev, window, cx);
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.focus_handle.focus(window, cx);
            return;
        };
        let handle = tab.content.focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn active_editor_path(&self, cx: &gpui::App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.active)?;
        match &tab.content {
            PaneContent::Editor(view) => Some(view.read(cx).file_path().to_path_buf()),
            PaneContent::Terminal(_) => None,
        }
    }

    /// Walk every tab (active + inactive) and yield the captured PTY
    /// scrollback bytes for the terminal kinds. Editor tabs contribute
    /// an empty buffer so the ordinal counter stays aligned with the
    /// tab vector.
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &gpui::App) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(view) => {
                    out.push(view.read(cx).serialize_buffer(max_bytes));
                }
                PaneContent::Editor(_) => out.push(Vec::new()),
            }
        }
        out
    }

    pub fn collect_pane_external_ids(&self, cx: &gpui::App) -> Vec<Option<String>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(view) => out.push(view.read(cx).external_id()),
                PaneContent::Editor(_) => out.push(None),
            }
        }
        out
    }

    pub(crate) fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_terminal_tab(window, cx);
    }

    pub(crate) fn on_close_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.active;
        self.close_tab(ix, window, cx);
    }

    pub(crate) fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.next_tab(window, cx);
    }

    pub(crate) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.prev_tab(window, cx);
    }

    pub(crate) fn on_new_agent(
        &mut self,
        _: &NewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(Box::new(RequestOpenAdapterPicker), cx);
    }
}

impl Focusable for PaneGroup {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
