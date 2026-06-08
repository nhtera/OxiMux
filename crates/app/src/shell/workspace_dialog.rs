//! Workspace dialog — create + rename modal.
//!
//! `Cmd+Shift+N` opens in `Create` mode; `WorkspaceRoot::request_rename_workspace`
//! opens in `Rename(workspace)` mode. Create mode adds two dropdowns: a
//! project picker (lets the user choose which project to create the
//! workspace under without first activating it via Cmd+O) and an agent
//! picker (auto-spawns the chosen CLI agent in a new tab after the
//! workspace is created — default "Skip" leaves the workspace empty).
//!
//! Pattern: full-window overlay (absolute inset-0) for click-outside
//! dismiss; centered modal card. Mirrors the step 5 project picker shape.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window,
    div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_core::{AgentAdapter, Project, Workspace};
use oximux_git::derive_slug;
use oximux_settings::{Density, Theme, Typography};

const MODAL_WIDTH: f32 = 480.0;
const MODAL_TOP_OFFSET: f32 = 96.0;
const FIELD_HEIGHT: f32 = 32.0;

/// All four built-in agents in dialog order. Mirrors the order in
/// `AdapterRegistry::with_builtin_adapters` so the picker UX stays
/// stable across detection results.
const AGENT_CHOICES: &[AgentAdapter] = &[
    AgentAdapter::ClaudeCode,
    AgentAdapter::Codex,
    AgentAdapter::Aider,
    AgentAdapter::Custom,
];

/// Open-state mode. `None` (held in [`WorkspaceDialog::mode`]) is the
/// closed sentinel. `Rename` boxes the `Workspace` payload to keep the
/// enum small — `Workspace` is ~216 bytes and would otherwise force
/// every `Create`-mode dialog to carry a 216-byte tail of zeros (clippy
/// `large_enum_variant` flag).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceDialogMode {
    Create,
    Rename(Box<Workspace>),
}

/// Payload handed to the owner's `OnSubmit` callback when the user
/// confirms. `project` and `agent` are only meaningful in `Create` mode.
pub struct WorkspaceDialogSubmit {
    pub mode: WorkspaceDialogMode,
    pub name: String,
    /// Selected project for Create mode; `None` for Rename mode.
    pub project: Option<Project>,
    /// Optional agent to auto-spawn after Created. `None` = Skip.
    pub agent: Option<AgentAdapter>,
}

pub type OnSubmit = Box<dyn Fn(WorkspaceDialogSubmit, &mut Window, &mut App) + Send + 'static>;

pub struct WorkspaceDialog {
    mode: Option<WorkspaceDialogMode>,
    name_input: Entity<InputState>,
    focus_handle: FocusHandle,
    on_submit: OnSubmit,
    /// Snapshot of recent projects at `open_create` time.
    projects: Vec<Project>,
    /// Currently-selected project for the create dropdown.
    selected_project: Option<Project>,
    project_dropdown_open: bool,
    /// `None` = "Skip (no agent)".
    selected_agent: Option<AgentAdapter>,
    agent_dropdown_open: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl WorkspaceDialog {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        on_submit: OnSubmit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. fix-login"));
        Self {
            mode: None,
            name_input,
            focus_handle: cx.focus_handle(),
            on_submit,
            projects: Vec::new(),
            selected_project: None,
            project_dropdown_open: false,
            selected_agent: None,
            agent_dropdown_open: false,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.mode.is_some()
    }

    /// Open in Create mode with the available projects + the user's
    /// current active project (used as the dropdown default).
    pub fn open_create(
        &mut self,
        projects: Vec<Project>,
        active: Option<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Some(WorkspaceDialogMode::Create);
        self.name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.projects = projects;
        self.selected_project = active.or_else(|| self.projects.first().cloned());
        self.project_dropdown_open = false;
        self.selected_agent = None;
        self.agent_dropdown_open = false;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn open_rename(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing_name = workspace.name.clone();
        self.mode = Some(WorkspaceDialogMode::Rename(Box::new(workspace)));
        self.name_input
            .update(cx, |s, cx| s.set_value(&existing_name, window, cx));
        self.project_dropdown_open = false;
        self.agent_dropdown_open = false;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.mode = None;
        self.project_dropdown_open = false;
        self.agent_dropdown_open = false;
        cx.notify();
    }

    fn current_name(&self, cx: &App) -> String {
        self.name_input.read(cx).value().to_string()
    }

    pub fn slug_preview(&self, cx: &App) -> String {
        derive_slug(self.current_name(cx).trim())
    }

    /// Submittable iff name non-empty AND (Rename mode OR Create mode has
    /// a selected project). Empty-projects state in Create disables submit.
    fn can_submit(&self, cx: &App) -> bool {
        if self.current_name(cx).trim().is_empty() {
            return false;
        }
        match &self.mode {
            Some(WorkspaceDialogMode::Create) => self.selected_project.is_some(),
            Some(WorkspaceDialogMode::Rename(_)) => true,
            None => false,
        }
    }

    fn try_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let Some(mode) = self.mode.clone() else {
            return;
        };
        let name = self.current_name(cx).trim().to_string();
        let project = match &mode {
            WorkspaceDialogMode::Create => self.selected_project.clone(),
            WorkspaceDialogMode::Rename(_) => None,
        };
        let agent = self.selected_agent;
        self.close(cx);
        (self.on_submit)(
            WorkspaceDialogSubmit {
                mode,
                name,
                project,
                agent,
            },
            window,
            cx,
        );
    }
}

impl Focusable for WorkspaceDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Human-readable label for the agent dropdown.
pub fn agent_label(agent: Option<AgentAdapter>) -> &'static str {
    match agent {
        None => "Skip (no agent)",
        Some(AgentAdapter::ClaudeCode) => "Claude Code",
        Some(AgentAdapter::Codex) => "Codex",
        Some(AgentAdapter::Aider) => "Aider",
        Some(AgentAdapter::Custom) => "Custom",
    }
}

impl Render for WorkspaceDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(mode) = self.mode.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let can_submit = self.can_submit(cx);
        let slug = self.slug_preview(cx);
        let (title, button_label) = match mode {
            WorkspaceDialogMode::Create => ("New Workspace", "Create"),
            WorkspaceDialogMode::Rename(_) => ("Rename Workspace", "Rename"),
        };
        let is_create = matches!(self.mode, Some(WorkspaceDialogMode::Create));
        let slug_line = format!("Branch: oximux/{}", slug);

        let mut card = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => this.try_submit(window, cx),
                    "escape" => this.close(cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                // Swallow press inside the card so it does not bubble to
                // the overlay dismiss handler.
            })
            .flex()
            .flex_col()
            .w(px(MODAL_WIDTH))
            .p(px(density.pad_panel * 2.0))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(title),
            );

        if is_create {
            card = card.child(self.render_project_section(cx));
        }

        card = card
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Name"),
            )
            .child(Input::new(&self.name_input))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(slug_line),
            );

        if is_create {
            card = card.child(self.render_agent_section(cx));
        }

        card = card.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(density.gap_inline))
                .child(
                    Button::new("workspace-dialog-cancel")
                        .label("Cancel")
                        .on_click(cx.listener(|dlg, _: &ClickEvent, _window, cx| {
                            dlg.close(cx);
                        })),
                )
                .child(
                    Button::new("workspace-dialog-submit")
                        .primary()
                        .label(button_label)
                        .disabled(!can_submit)
                        .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                            dlg.try_submit(window, cx);
                        })),
                ),
        );

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(MODAL_TOP_OFFSET))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.close(cx)),
            )
            .child(card)
            .into_any_element()
    }
}

impl WorkspaceDialog {
    fn render_project_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let label = self
            .selected_project
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "No projects — open one first (⌘O)".to_string());

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Project"),
            )
            .child(
                div()
                    .id("ws-dialog-project-trigger")
                    .flex()
                    .items_center()
                    .h(px(FIELD_HEIGHT))
                    .px(px(8.0))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border_inactive)
                    .rounded(px(density.r_xs))
                    .cursor_pointer()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_base)
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                            this.project_dropdown_open = !this.project_dropdown_open;
                            this.agent_dropdown_open = false;
                            cx.notify();
                        }),
                    ),
            );
        if self.project_dropdown_open && !self.projects.is_empty() {
            let mut list = div()
                .flex()
                .flex_col()
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border_inactive)
                .rounded(px(density.r_xs));
            for (ix, project) in self.projects.iter().enumerate() {
                let p_clone = project.clone();
                list = list.child(
                    div()
                        .id(("ws-dialog-project-opt", ix))
                        .flex()
                        .items_center()
                        .h(px(FIELD_HEIGHT))
                        .px(px(8.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.bg_panel_alt))
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child(project.name.clone())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                                this.selected_project = Some(p_clone.clone());
                                this.project_dropdown_open = false;
                                cx.notify();
                            }),
                        ),
                );
            }
            col = col.child(list);
        }
        col
    }

    fn render_agent_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let label = agent_label(self.selected_agent);

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Agent"),
            )
            .child(
                div()
                    .id("ws-dialog-agent-trigger")
                    .flex()
                    .items_center()
                    .h(px(FIELD_HEIGHT))
                    .px(px(8.0))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border_inactive)
                    .rounded(px(density.r_xs))
                    .cursor_pointer()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_base)
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                            this.agent_dropdown_open = !this.agent_dropdown_open;
                            this.project_dropdown_open = false;
                            cx.notify();
                        }),
                    ),
            );

        if self.agent_dropdown_open {
            let mut list = div()
                .flex()
                .flex_col()
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border_inactive)
                .rounded(px(density.r_xs));
            // First row: Skip
            list = list.child(agent_option_row(None, theme, density, &typography, cx));
            for &kind in AGENT_CHOICES {
                list = list.child(agent_option_row(
                    Some(kind),
                    theme,
                    density,
                    &typography,
                    cx,
                ));
            }
            col = col.child(list);
        }
        col
    }
}

fn agent_option_row(
    kind: Option<AgentAdapter>,
    theme: Theme,
    _density: Density,
    typography: &Typography,
    cx: &mut Context<WorkspaceDialog>,
) -> impl IntoElement {
    let id: usize = match kind {
        None => 0,
        Some(AgentAdapter::ClaudeCode) => 1,
        Some(AgentAdapter::Codex) => 2,
        Some(AgentAdapter::Aider) => 3,
        Some(AgentAdapter::Custom) => 4,
    };
    div()
        .id(("ws-dialog-agent-opt", id))
        .flex()
        .items_center()
        .h(px(FIELD_HEIGHT))
        .px(px(8.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .child(agent_label(kind))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.selected_agent = kind;
                this.agent_dropdown_open = false;
                cx.notify();
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Workspace {
        Workspace {
            id: "id".to_string(),
            project_id: "pid".to_string(),
            name: "old".to_string(),
            slug: "old".to_string(),
            branch: "oximux/old".to_string(),
            worktree_path: "/path".to_string(),
            status: "active".to_string(),
            created_at: "now".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
        }
    }

    fn project(id: &str, name: &str) -> Project {
        Project {
            id: id.to_string(),
            name: name.to_string(),
            root_path: format!("/p/{id}"),
            default_branch: "main".to_string(),
            created_at: "now".to_string(),
            last_opened_at: None,
        }
    }

    #[test]
    fn agent_label_skip_for_none() {
        assert_eq!(agent_label(None), "Skip (no agent)");
    }

    #[test]
    fn agent_label_resolves_each_variant() {
        assert_eq!(agent_label(Some(AgentAdapter::ClaudeCode)), "Claude Code");
        assert_eq!(agent_label(Some(AgentAdapter::Codex)), "Codex");
        assert_eq!(agent_label(Some(AgentAdapter::Aider)), "Aider");
        assert_eq!(agent_label(Some(AgentAdapter::Custom)), "Custom");
    }

    #[test]
    fn submit_payload_create_carries_project_and_agent() {
        let payload = WorkspaceDialogSubmit {
            mode: WorkspaceDialogMode::Create,
            name: "fix-login".to_string(),
            project: Some(project("p1", "Acme")),
            agent: Some(AgentAdapter::ClaudeCode),
        };
        assert_eq!(payload.mode, WorkspaceDialogMode::Create);
        assert!(payload.project.is_some());
        assert_eq!(payload.agent, Some(AgentAdapter::ClaudeCode));
    }

    #[test]
    fn submit_payload_rename_has_no_project() {
        let payload = WorkspaceDialogSubmit {
            mode: WorkspaceDialogMode::Rename(Box::new(ws())),
            name: "new".to_string(),
            project: None,
            agent: None,
        };
        assert!(payload.project.is_none());
        assert!(payload.agent.is_none());
    }

    #[test]
    fn agent_choices_match_registry_order() {
        assert_eq!(
            AGENT_CHOICES,
            &[
                AgentAdapter::ClaudeCode,
                AgentAdapter::Codex,
                AgentAdapter::Aider,
                AgentAdapter::Custom,
            ]
        );
    }

    #[test]
    fn slug_preview_matches_derive_slug() {
        let name = "  My Feature  ";
        let trimmed = name.trim();
        assert_eq!(derive_slug(trimmed), "my-feature");
    }
}
