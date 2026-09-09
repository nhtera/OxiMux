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
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState},
};
use oximux_core::{AgentAdapter, Project, Workspace};
use oximux_git::derive_slug;
use oximux_settings::{Density, SetupDecision, Theme, Typography};

use crate::shell::workspace::base_choice::{BaseChoice, BaseMode};
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

/// The built-in agents in dialog order. Mirrors the order in
/// `AdapterRegistry::with_builtin_adapters` so the picker UX stays
/// stable across detection results. (This IS a restatement of the registry —
/// the pi rollout's bug #2 — so the test below locks the two in sync.)
const AGENT_CHOICES: &[AgentAdapter] = &[
    AgentAdapter::ClaudeCode,
    AgentAdapter::Codex,
    AgentAdapter::Pi,
    AgentAdapter::Omp,
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
    /// Per-request answer for the project's `setup` script. Defaults to
    /// [`SetupDecision::Inherit`], which is the project's committed
    /// `auto_setup` — the dropdown exists so a throwaway worktree can skip a
    /// ten-minute install, and a one-off can opt in, without editing a file
    /// the whole team shares.
    pub setup: SetupDecision,
    /// What the worktree is cut from. Defaults to a new branch based on the
    /// project's default branch — the answer that used to be "wherever the
    /// main checkout's HEAD happened to be".
    pub base: BaseChoice,
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
    selected_setup: SetupDecision,
    setup_dropdown_open: bool,
    /// The **From** control's state. See [`BaseChoice`].
    base: BaseChoice,
    /// Local branches, most-recent-first, as `list_branches` already sorts
    /// them. Capped at [`MAX_BRANCH_CHOICES`]: a repo with thousands of refs
    /// makes an uncapped dropdown useless, and the ones a person wants to base
    /// work on are the ones they touched recently.
    branches: Vec<String>,
    from_dropdown_open: bool,
    existing_dropdown_open: bool,
    /// The setup-skip notice for the current base, shown BEFORE the user
    /// commits — the same sentence provisioning would write afterwards.
    base_warning: Option<String>,
    /// Cancel-on-change token for the base-warning lookup, mirroring
    /// `fetch_epoch`: a completed lookup only applies if its epoch is current.
    ///
    /// The branch list has **no** epoch and its own task slot. The two are
    /// independent — the list depends on the project, the warning on the
    /// selection — and sharing either would make choosing a base cancel the
    /// fetch of the list you chose it from, leaving the dropdown empty for the
    /// rest of the dialog's life.
    base_epoch: u64,
    _warning_load: Option<Task<()>>,
    _branch_load: Option<Task<()>>,
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
            selected_setup: SetupDecision::Inherit,
            setup_dropdown_open: false,
            base: BaseChoice::default(),
            branches: Vec::new(),
            from_dropdown_open: false,
            existing_dropdown_open: false,
            base_warning: None,
            base_epoch: 0,
            _warning_load: None,
            _branch_load: None,
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
        self.selected_setup = SetupDecision::Inherit;
        self.setup_dropdown_open = false;
        self.reset_base();
        self.reload_branches(cx);
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
        self.setup_dropdown_open = false;
        self.reset_base();
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
        self.setup_dropdown_open = false;
        self.reset_base();
        cx.notify();
    }

    /// Clear the base selection and cancel anything in flight for it.
    ///
    /// The epoch bump is what makes this a cancel rather than a hope: a lookup
    /// that lands after a close (or into a later open) finds its epoch stale
    /// and applies nothing. Same contract as `fetch_epoch`.
    fn reset_base(&mut self) {
        self.base = BaseChoice::default();
        self.from_dropdown_open = false;
        self.existing_dropdown_open = false;
        self.base_warning = None;
        self.base_epoch += 1;
        self._warning_load = None;
        self._branch_load = None;
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
            // Adopting a branch requires naming one — `is_complete` is the
            // only state that can be half-made.
            Some(WorkspaceDialogMode::Create) => {
                self.selected_project.is_some() && self.base.is_complete()
            }
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
        let setup = self.selected_setup;
        let base = self.base.clone();
        let linked_issue = self.linked_issue.clone();
        self.close(cx);
        (self.on_submit)(
            WorkspaceDialogSubmit {
                mode,
                name,
                project,
                agent,
                setup,
                base,
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
        Some(AgentAdapter::Omp) => "omp",
        Some(AgentAdapter::Custom) => "Custom",
    }
}

impl Render for WorkspaceDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
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
        // The configured prefix, from the same global the create path reads —
        // so this line is a promise the create keeps rather than a second
        // derivation of the same rule.
        let project_root = self.selected_project.as_ref().map(|p| std::path::PathBuf::from(&p.root_path));
        let branch = crate::git_settings::branch_for_slug(&slug, project_root.as_deref(), cx);
        // In existing-branch mode the name field no longer names the branch —
        // the worktree adopts one. Previewing the minted `<prefix>/<slug>`
        // there would promise a branch the create will never make.
        let branch = match (&self.base.mode, self.base.existing.as_deref()) {
            (BaseMode::ExistingBranch, Some(existing)) => existing.to_string(),
            (BaseMode::ExistingBranch, None) => "—".to_string(),
            (BaseMode::NewBranch, _) => branch,
        };
        let slug_line = if self.fetching_title {
            format!("Branch: {branch} · fetching title…")
        } else if let Some(issue) = &self.linked_issue {
            format!("Branch: {branch} · linked {issue}")
        } else {
            format!("Branch: {branch}")
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
                if this.any_dropdown_open() {
                    this.close_dropdowns();
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
            card = card.child(self.render_base_section(cx));
            card = card.child(self.render_agent_section(cx));
            card = card.child(self.render_setup_section(cx));
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
                            this.setup_dropdown_open = false;
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
                                let changed = this.selected_project.as_ref().map(|p| &p.id)
                                    != Some(&p_clone.id);
                                this.selected_project = Some(p_clone.clone());
                                this.project_dropdown_open = false;
                                // Everything under the Name field is keyed to a
                                // project: the branch list, the chosen base, and
                                // the ancestry notice. Carrying any of them across
                                // a switch offers refs the new project does not
                                // have — and in existing-branch mode it would submit
                                // a branch name from the OLD repository, which fails
                                // the create at best. `open_create` runs exactly
                                // these two; a project change is the same event.
                                if changed {
                                    this.reset_base();
                                    this.reload_branches(cx);
                                }
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
                            this.setup_dropdown_open = false;
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


    /// Whether any dropdown in the card is expanded. Escape collapses one
    /// before it dismisses the dialog, so this has to know about all of them —
    /// a new dropdown that forgets to register here makes Escape close the
    /// whole dialog out from under an open list.
    fn any_dropdown_open(&self) -> bool {
        self.project_dropdown_open
            || self.agent_dropdown_open
            || self.setup_dropdown_open
            || self.from_dropdown_open
            || self.existing_dropdown_open
    }

    fn close_dropdowns(&mut self) {
        self.project_dropdown_open = false;
        self.agent_dropdown_open = false;
        self.setup_dropdown_open = false;
        self.from_dropdown_open = false;
        self.existing_dropdown_open = false;
    }

    /// The selected project's default branch, or `""` when there is none to
    /// read. Empty is a real answer here — see [`BaseChoice::resolve`].
    fn default_branch(&self) -> &str {
        self.selected_project
            .as_ref()
            .map(|p| p.default_branch.as_str())
            .unwrap_or_default()
    }

    /// Reload the branch list for the selected project.
    ///
    /// On the background executor because `git branch --list` is a subprocess
    /// and this is called from `open_create` — a synchronous read there would
    /// stall the window on every ⌘N in a cold repository.
    fn reload_branches(&mut self, cx: &mut Context<Self>) {
        self.branches.clear();
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        let project_id = project.id.clone();
        let root = std::path::PathBuf::from(&project.root_path);
        self._branch_load = Some(cx.spawn(async move |this, cx| {
            let names = match oximux_git::Repository::open(&root).await {
                Ok(repo) => repo
                    .list_branches()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|b| b.name)
                    .take(MAX_BRANCH_CHOICES)
                    .collect::<Vec<_>>(),
                // A project that is not a repository has no branches to offer.
                // The From control still works: it falls back to the default
                // branch, which is what an empty list means here.
                Err(_) => Vec::new(),
            };
            let _ = this.update(cx, |d, cx| {
                // Keyed on the project rather than an epoch: the only thing
                // that invalidates a branch list is asking about a different
                // project, and holding the slot means a later reload replaces
                // this task rather than racing it.
                if d.selected_project.as_ref().map(|p| p.id.as_str()) != Some(&project_id) {
                    return;
                }
                d.branches = names;
                cx.notify();
            });
        }));
    }

    /// Recompute the setup-skip notice for the current base.
    ///
    /// Runs on every base change rather than once at submit, because the whole
    /// point is to say so **before** the user commits. One `merge-base` per
    /// change is acceptable on a user-driven control; it is not a polling path.
    ///
    /// **This asks the create path's own function.** An earlier draft
    /// reimplemented the ancestry rule here, which is precisely how a preview
    /// comes to promise one thing while the create does another — the same
    /// failure `branch_name` was extracted to end. `setup_decision` returns the
    /// decision *and* the sentence, so the dialog renders the string
    /// provisioning would have written, not a paraphrase of it.
    fn refresh_base_warning(&mut self, cx: &mut Context<Self>) {
        self.base_warning = None;
        self.base_epoch += 1;
        let epoch = self.base_epoch;
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        // No named base means nothing to warn about, and no subprocess to
        // spend finding that out.
        let Some(base) = self.base.resolve(String::new()) else {
            return;
        };
        if base.named_base().is_none() {
            return;
        }
        let default = project.default_branch.clone();
        let root = std::path::PathBuf::from(&project.root_path);
        self._warning_load = Some(cx.spawn(async move |this, cx| {
            let Ok(repo) = oximux_git::Repository::open(&root).await else {
                return;
            };
            let default = (!default.is_empty()).then_some(default);
            let (_, reason) = oximux_worktree_ops::setup_decision(
                &repo,
                &base,
                oximux_settings::SetupDecision::Inherit,
                default.as_deref(),
            )
            .await;
            let Some(reason) = reason else {
                return;
            };
            let _ = this.update(cx, |d, cx| {
                if d.base_epoch != epoch {
                    return;
                }
                // Future tense here, past tense in the transcript — same rule,
                // same refs, read at the moment each is true.
                d.base_warning = Some(reason.replacen("Setup skipped:", "Setup will be skipped:", 1));
                cx.notify();
            });
        }));
    }

    /// The **From** / **Existing branch** control, and the setup-skip notice
    /// the current base implies.
    ///
    /// Sits directly under Name because it changes what the branch line above
    /// it means: in existing-branch mode the name no longer names the branch.
    fn render_base_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let adopting = self.base.mode == BaseMode::ExistingBranch;

        let segmented = div()
            .flex()
            .flex_row()
            .gap(px(4.0))
            .child(base_mode_tab(BaseMode::NewBranch, "New branch", adopting, theme, density, &typography, cx))
            .child(base_mode_tab(BaseMode::ExistingBranch, "Existing branch", adopting, theme, density, &typography, cx));

        let (label, trigger_id, open) = if adopting {
            (
                self.base
                    .existing
                    .as_deref()
                    .unwrap_or("Choose a branch…")
                    .to_string(),
                "ws-dialog-existing-trigger",
                self.existing_dropdown_open,
            )
        } else {
            (
                self.base.from_label(self.default_branch()).to_string(),
                "ws-dialog-from-trigger",
                self.from_dropdown_open,
            )
        };

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child(if adopting { "Branch" } else { "From" }),
            )
            .child(segmented)
            .child(
                div()
                    .id(trigger_id)
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
                        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                            let was = if adopting {
                                this.existing_dropdown_open
                            } else {
                                this.from_dropdown_open
                            };
                            this.close_dropdowns();
                            if adopting {
                                this.existing_dropdown_open = !was;
                            } else {
                                this.from_dropdown_open = !was;
                            }
                            cx.notify();
                        }),
                    ),
            );

        if open {
            let mut list = div()
                .id("ws-dialog-branch-list")
                .flex()
                .flex_col()
                .max_h(px(BRANCH_LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border_inactive)
                .rounded(px(density.r_xs));
            // "Default branch" is offered only when cutting a new branch:
            // adopting one means naming it, and there is no default to adopt.
            if !adopting {
                list = list.child(branch_option_row(
                    None,
                    self.default_branch(),
                    theme,
                    &typography,
                    cx,
                ));
            }
            for (i, name) in self.branches.iter().enumerate() {
                list = list.child(
                    branch_option_row(Some((i, name.clone())), "", theme, &typography, cx),
                );
            }
            if self.branches.is_empty() {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(FIELD_HEIGHT))
                        .px(px(8.0))
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child("no local branches"),
                );
            }
            col = col.child(list);
        }

        if let Some(warning) = &self.base_warning {
            col = col.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.status_warn)
                    .child(warning.clone()),
            );
        }
        col
    }

    /// The per-request Setup override. Deliberately the last field: the
    /// default answer ("Project default") is right almost always, and putting
    /// it above the agent picker would make every create look like a decision
    /// about setup.
    fn render_setup_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Setup script"),
            )
            .child(
                div()
                    .id("ws-dialog-setup-trigger")
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
                    .child(setup_label(self.selected_setup))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                            this.setup_dropdown_open = !this.setup_dropdown_open;
                            this.project_dropdown_open = false;
                            this.agent_dropdown_open = false;
                            cx.notify();
                        }),
                    ),
            );

        if self.setup_dropdown_open {
            let mut list = div()
                .flex()
                .flex_col()
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border_inactive)
                .rounded(px(density.r_xs));
            for choice in SETUP_CHOICES {
                list = list.child(setup_option_row(*choice, theme, &typography, cx));
            }
            col = col.child(list);
        }
        col
    }
}


/// Cap on the **From** dropdown. A repo with thousands of refs makes an
/// uncapped list useless; `list_branches` sorts by `-committerdate`, so the
/// survivors are the branches someone actually touched.
const MAX_BRANCH_CHOICES: usize = 50;

/// Keeps the branch list a list rather than a second modal.
const BRANCH_LIST_MAX_HEIGHT: f32 = 220.0;

/// One half of the New-branch / Existing-branch segmented control.
fn base_mode_tab(
    mode: BaseMode,
    label: &'static str,
    adopting: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<WorkspaceDialog>,
) -> impl IntoElement {
    let selected = (mode == BaseMode::ExistingBranch) == adopting;
    div()
        .id(("ws-dialog-base-mode", mode as usize))
        .flex()
        .items_center()
        .h(px(FIELD_HEIGHT))
        .px(px(10.0))
        .cursor_pointer()
        .rounded(px(density.r_xs))
        .border_1()
        .border_color(if selected { theme.border_active } else { theme.border_inactive })
        .bg(if selected { theme.bg_panel } else { theme.bg_base })
        .hover(|s| s.bg(theme.hover_overlay))
        .text_size(px(typography.t_body_sm))
        .text_color(if selected { theme.fg_base } else { theme.fg_muted })
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if this.base.mode == mode {
                    return;
                }
                this.base.mode = mode;
                this.close_dropdowns();
                // The other mode's answer is deliberately kept, not cleared: a
                // user toggling to look at the other list and back should find
                // their choice where they left it. `BaseChoice::resolve` reads
                // only the field its mode names, so the stale one cannot leak
                // into the result.
                this.refresh_base_warning(cx);
                cx.notify();
            }),
        )
}

/// One row of the branch dropdown. `None` is the "default branch" row, which
/// exists only in new-branch mode.
fn branch_option_row(
    branch: Option<(usize, String)>,
    default_branch: &str,
    theme: Theme,
    typography: &Typography,
    cx: &mut Context<WorkspaceDialog>,
) -> impl IntoElement {
    let (id, label, value) = match &branch {
        Some((i, name)) => (i + 1, name.clone(), Some(name.clone())),
        None => (
            0,
            if default_branch.is_empty() {
                "Default branch (current checkout)".to_string()
            } else {
                format!("Default branch ({default_branch})")
            },
            None,
        ),
    };
    div()
        .id(("ws-dialog-branch-opt", id))
        .flex()
        .items_center()
        .h(px(FIELD_HEIGHT))
        .px(px(8.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                match this.base.mode {
                    BaseMode::ExistingBranch => this.base.existing = value.clone(),
                    BaseMode::NewBranch => this.base.from = value.clone(),
                }
                this.close_dropdowns();
                this.refresh_base_warning(cx);
                cx.notify();
            }),
        )
}

/// Order matters: `Inherit` first because it is the default and the answer a
/// user should have to actively leave.
const SETUP_CHOICES: &[SetupDecision] =
    &[SetupDecision::Inherit, SetupDecision::Run, SetupDecision::Skip];

/// Human-readable label for the setup dropdown.
pub fn setup_label(decision: SetupDecision) -> &'static str {
    match decision {
        SetupDecision::Inherit => "Project default",
        SetupDecision::Run => "Run setup",
        SetupDecision::Skip => "Skip setup",
    }
}

fn setup_option_row(
    decision: SetupDecision,
    theme: Theme,
    typography: &Typography,
    cx: &mut Context<WorkspaceDialog>,
) -> impl IntoElement {
    let id: usize = match decision {
        SetupDecision::Inherit => 0,
        SetupDecision::Run => 1,
        SetupDecision::Skip => 2,
    };
    div()
        .id(("ws-dialog-setup-opt", id))
        .flex()
        .items_center()
        .h(px(FIELD_HEIGHT))
        .px(px(8.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .child(setup_label(decision))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.selected_setup = decision;
                this.setup_dropdown_open = false;
                cx.notify();
            }),
        )
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
        Some(AgentAdapter::Omp) => 5,
        Some(AgentAdapter::Custom) => 4,
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
            comment: String::new(),
            phase: String::new(),
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
        assert_eq!(agent_label(Some(AgentAdapter::Pi)), "Pi");
        assert_eq!(agent_label(Some(AgentAdapter::Custom)), "Custom");
    }

    #[test]
    fn submit_payload_create_carries_project_and_agent() {
        let payload = WorkspaceDialogSubmit {
            mode: WorkspaceDialogMode::Create,
            name: "fix-login".to_string(),
            project: Some(project("p1", "Acme")),
            agent: Some(AgentAdapter::ClaudeCode),
            setup: SetupDecision::Inherit,
            base: BaseChoice::default(),
            linked_issue: None,
        };
        assert_eq!(payload.mode, WorkspaceDialogMode::Create);
        assert!(payload.project.is_some());
        assert_eq!(payload.agent, Some(AgentAdapter::ClaudeCode));
    }

    /// The dropdown's default must be the project's answer, or opening the
    /// dialog and pressing Enter would silently override what the team
    /// committed — the exact behavior change this phase is careful not to make.
    #[test]
    fn the_setup_dropdown_defaults_to_the_project_answer() {
        assert_eq!(SETUP_CHOICES[0], SetupDecision::Inherit);
        assert_eq!(setup_label(SetupDecision::Inherit), "Project default");
        assert!(SetupDecision::Inherit.resolve(true));
        assert!(!SetupDecision::Inherit.resolve(false));
    }

    #[test]
    fn every_setup_choice_has_a_distinct_label() {
        let labels: std::collections::BTreeSet<_> =
            SETUP_CHOICES.iter().map(|c| setup_label(*c)).collect();
        assert_eq!(labels.len(), SETUP_CHOICES.len());
    }

    #[test]
    fn submit_payload_rename_has_no_project() {
        let payload = WorkspaceDialogSubmit {
            mode: WorkspaceDialogMode::Rename(Box::new(ws())),
            name: "new".to_string(),
            project: None,
            agent: None,
            setup: SetupDecision::Inherit,
            base: BaseChoice::default(),
            linked_issue: None,
        };
        assert!(payload.project.is_none());
        assert!(payload.agent.is_none());
    }

    #[test]
    fn agent_choices_match_registry_order() {
        // Against the REAL registry, not a second literal: this constant is a
        // restatement of `with_builtin_adapters` (the pi rollout's bug #2 was
        // exactly such a list going stale), so the registry is the oracle.
        let registry: Vec<AgentAdapter> = oximux_agents::registry::AdapterRegistry::
            with_builtin_adapters()
            .entries_without_detection()
            .iter()
            .map(|e| e.adapter_enum)
            .collect();
        assert_eq!(AGENT_CHOICES, registry.as_slice());
    }

    #[test]
    fn slug_preview_matches_derive_slug() {
        let name = "  My Feature  ";
        let trimmed = name.trim();
        assert_eq!(derive_slug(trimmed), "my-feature");
    }
}
