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

use std::time::Duration;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled,
    Subscription, Task, Window, div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState},
};
use oximux_core::{AgentAdapter, Project, Workspace};
use oximux_git::derive_slug;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::ref_parse::parse_forge_ref;
use crate::shell::forge::{Forge, fetch_ref_title};
use crate::ui::FloatingSurface;

/// Debounce between the last keystroke that looks like a forge reference
/// and the title fetch — a paste settles in one event, typed digits get a
/// beat to finish.
const TITLE_FETCH_DEBOUNCE_MS: u64 = 350;

const MODAL_WIDTH: f32 = 480.0;
const MODAL_TOP_OFFSET: f32 = 96.0;
const FIELD_HEIGHT: f32 = 32.0;

/// All four built-in agents in dialog order. Mirrors the order in
/// `AdapterRegistry::with_builtin_adapters` so the picker UX stays
/// stable across detection results.
const AGENT_CHOICES: &[AgentAdapter] = &[
    AgentAdapter::ClaudeCode,
    AgentAdapter::Codex,
    AgentAdapter::Pi,
    AgentAdapter::Aider,
    AgentAdapter::Custom,
];

/// Open-state mode. `None` (held in [`WorkspaceDialog::mode`]) is the
/// closed sentinel. `Rename` boxes the `Workspace` payload to keep the
/// enum small — `Workspace` is ~216 bytes and would otherwise force
/// every `Create`-mode dialog to carry a 216-byte tail of zeros (clippy
/// `large_enum_variant` flag).
// `Eq` is intentionally not derived: `Workspace` carries an `f64` rank, so it
// is only `PartialEq`.
#[derive(Clone, Debug, PartialEq)]
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
    /// Forge reference (e.g. `"#42"`) the name was prefilled from, when the
    /// user pasted an issue/PR URL or number. Persisted on the workspace as
    /// its linked issue.
    pub linked_issue: Option<String>,
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
    /// Forge reference the current name was prefilled from (`"#42"`).
    /// Cleared the moment the user edits the name again — their text wins.
    linked_issue: Option<String>,
    /// Monotonic cancellation token for the title fetch: every name change
    /// bumps it, and a completed fetch only applies if its epoch is still
    /// current. Cheap cancel-on-edit without aborting the task.
    fetch_epoch: u64,
    /// True while a title fetch is pending/in flight — drives the inline
    /// "fetching title…" hint.
    fetching_title: bool,
    /// Suppresses the input-change handler while the fetch APPLIES the
    /// fetched title (set_value fires the same Change event a keystroke
    /// does, which would immediately wipe `linked_issue`).
    applying_title: bool,
    /// Holds the in-flight debounce + fetch task.
    _title_fetch: Option<Task<()>>,
    /// Wires `name_input` changes into the forge-reference detection.
    _name_sub: Subscription,
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
        let name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. fix-login, #42, or an issue URL")
        });
        let name_sub = cx.subscribe_in(
            &name_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.on_name_changed(window, cx);
                }
            },
        );
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
            linked_issue: None,
            fetch_epoch: 0,
            fetching_title: false,
            applying_title: false,
            _title_fetch: None,
            _name_sub: name_sub,
            theme,
            density,
            typography,
        }
    }

    /// React to a name edit: a recognizable forge reference kicks off a
    /// debounced title fetch; anything else cancels whatever was pending
    /// and clears the prefill state — the user's text always wins.
    fn on_name_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.applying_title {
            return;
        }
        self.fetch_epoch += 1;
        self.linked_issue = None;
        self.fetching_title = false;
        self._title_fetch = None;
        if !matches!(self.mode, Some(WorkspaceDialogMode::Create)) {
            return;
        }
        let Some(parsed) = parse_forge_ref(&self.current_name(cx)) else {
            cx.notify();
            return;
        };
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        let epoch = self.fetch_epoch;
        let root = std::path::PathBuf::from(&project.root_path);
        self.fetching_title = true;
        self._title_fetch = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(TITLE_FETCH_DEBOUNCE_MS))
                .await;
            // Bail before any network if another edit superseded us.
            let still_current = this
                .read_with(cx, |d, _| d.fetch_epoch == epoch)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let title = match Forge::detect(&root).await {
                Some(forge) => {
                    fetch_ref_title(
                        forge,
                        &root,
                        parsed.kind,
                        parsed.number,
                        parsed.repo.as_deref(),
                    )
                    .await
                }
                None => None,
            };
            let _ = this.update_in(cx, |d, window, cx| {
                if d.fetch_epoch != epoch
                    || !matches!(d.mode, Some(WorkspaceDialogMode::Create))
                {
                    return;
                }
                d.fetching_title = false;
                if let Some(title) = title {
                    // Belt-and-suspenders: set_value currently suppresses
                    // Change events, but if that ever changes a re-fired
                    // Change would instantly cancel the prefill it came
                    // from — the guard keeps this path self-contained.
                    d.applying_title = true;
                    d.name_input
                        .update(cx, |s, cx| s.set_value(&title, window, cx));
                    d.applying_title = false;
                    d.linked_issue = Some(format!("#{}", parsed.number));
                }
                cx.notify();
            });
        }));
        cx.notify();
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
        self.linked_issue = None;
        self.fetch_epoch += 1;
        self.fetching_title = false;
        self._title_fetch = None;
        // Focus the NAME INPUT, not the card: typing must land immediately
        // (the card's `on_key_down` still sees Enter/Escape via the
        // capture-phase action handlers — same contract as the branch
        // picker).
        let input_focus = self.name_input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
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
        let input_focus = self.name_input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.mode = None;
        // Invalidate any in-flight title fetch — without this a fetch
        // completing after close (or into a later Rename open) would
        // overwrite whatever the input holds by then.
        self.fetch_epoch += 1;
        self.fetching_title = false;
        self._title_fetch = None;
        self.linked_issue = None;
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
        let linked_issue = self.linked_issue.clone();
        self.close(cx);
        (self.on_submit)(
            WorkspaceDialogSubmit {
                mode,
                name,
                project,
                agent,
                linked_issue,
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
        Some(AgentAdapter::Pi) => "Pi",
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
        // Inline prefill state rides the slug line: pending fetch shows a
        // quiet hint, a successful prefill shows the linked reference.
        let slug_line = if self.fetching_title {
            format!("Branch: oximux/{slug} · fetching title…")
        } else if let Some(issue) = &self.linked_issue {
            format!("Branch: oximux/{slug} · linked {issue}")
        } else {
            format!("Branch: oximux/{slug}")
        };

        let mut card = div()
            .track_focus(&self.focus_handle)
            // The focused name input converts Enter/Escape into its own
            // ACTIONS before raw key listeners on ancestors run — intercept
            // them at the capture phase (same contract as the branch
            // picker). The raw `on_key_down` stays for the no-input focus
            // path (e.g. after clicking a dropdown).
            .capture_action(cx.listener(|this, _: &InputEscape, _window, cx| {
                cx.stop_propagation();
                // First Escape collapses an open dropdown; only a bare
                // Escape dismisses the whole dialog.
                if this.project_dropdown_open || this.agent_dropdown_open {
                    this.project_dropdown_open = false;
                    this.agent_dropdown_open = false;
                    cx.notify();
                } else {
                    this.close(cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &InputEnter, window, cx| {
                cx.stop_propagation();
                this.try_submit(window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => this.try_submit(window, cx),
                    "escape" => this.close(cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                // Swallow press inside the card so it does not bubble to
                // the overlay dismiss handler. An empty closure does NOT
                // swallow — `stop_propagation` is required, else clicking
                // anywhere in the card dismisses the dialog.
                cx.stop_propagation();
            })
            .flex()
            .flex_col()
            .w(px(MODAL_WIDTH))
            .p(px(density.pad_panel * 2.0))
            .floating_chrome(&theme, &density)
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
            .occlude()
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
            .unwrap_or_else(|| {
                match crate::keymap_registry::display_chord_for("open_project_picker") {
                    Some(chord) => format!("No projects — open one first ({chord})"),
                    None => "No projects — open one first".to_string(),
                }
            });

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
                        .hover(|s| s.bg(theme.hover_overlay))
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
        Some(AgentAdapter::Pi) => 3,
        Some(AgentAdapter::Aider) => 4,
        Some(AgentAdapter::Custom) => 5,
    };
    div()
        .id(("ws-dialog-agent-opt", id))
        .flex()
        .items_center()
        .h(px(FIELD_HEIGHT))
        .px(px(8.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
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
            sort_order: 0.0,
            pinned: false,
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
            sort_order: 0.0,
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
            linked_issue: None,
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
            linked_issue: None,
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
                AgentAdapter::Pi,
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
