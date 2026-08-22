//! Connection-independent chat-agent roster for the unified "New Agent" composer.
//!
//! The unified composer lets the user pick a coding agent **and** a model before
//! any subprocess exists, so it cannot read the live `AgentConnection::models()`
//! (which only reports after a session is bound). This module assembles that
//! pre-bind vocabulary from the two static sources OxiMux already owns:
//!
//! - the detected adapter registry — built-in Claude/Codex, carrying their
//!   declared static model/effort lists (`RegistryEntry::models`/`::efforts`),
//! - the ACP presets (Cursor/Amp/OpenCode) from settings,
//!
//! filtered to chat-capable agents (terminal-only Aider and the Custom escape
//! hatch are dropped, matching the launcher's chat rows). Availability
//! (which-detection) and the live post-bind model list are layered on by the UI;
//! this module is a pure, connection-independent lookup.
//!
//! Placed in the app crate because `oximux-agents` (model lists) and
//! `oximux-settings` (transport + presets) are sibling crates that don't see
//! each other — the app is the one place both are visible, alongside the sibling
//! resolver `chat_backend_for`.

// A few roster fields (e.g. `efforts`) are carried for completeness and future
// phases but not yet read by the composer's pre-bind pickers (which offer agent
// + model only); allow the staged dead code rather than dropping fields that
// belong on the vocabulary.
#![allow(dead_code)]

use gpui::{
    div, px, AnyElement, App, AppContext, ClickEvent, Context, IntoElement, ParentElement, Styled,
    Window,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants};

use super::composer::WorktreeDraft;
use gpui_component::input::{InputEvent, InputState};
use oximux_agents::thread::{claude_model_choices, ChatImage, ModelChoice};
use oximux_agents::{AdapterRegistry, RegistryEntry};
use oximux_core::AgentAdapter;
use oximux_git::validate_slug;
use oximux_settings::{AgentLaunchSettings, Transport, ACP_PRESETS};

use super::AgentChatView;

/// One pickable coding agent in the unified composer, with its pre-connection
/// vocabulary. Empty `models`/`efforts` hide those composer rows until the live
/// connection reports its real vocabulary after binding (ACP presets have no
/// static list yet; Codex's static list is a stale approximation refreshed once
/// the `codex app-server` handshake returns its `model/list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatRosterEntry {
    /// Adapter id — the routing key into `chat_backend_for`/`AdapterSelection`.
    pub id: String,
    /// Human label for the agent dropdown.
    pub display: String,
    /// Which backend a first-message bind will speak.
    pub transport: Transport,
    /// Static pre-bind model selectors (`[]` = no model row until bound). Carried
    /// as [`ModelChoice`] so the unbound draft shows the same pretty labels and
    /// capability descriptions a bound session does (Claude fills these; agents
    /// whose vocab only arrives post-bind carry bare wire/label, no description).
    pub models: Vec<ModelChoice>,
    /// Static pre-bind reasoning-effort selectors (`[]` = no effort row).
    pub efforts: Vec<String>,
}

impl ChatRosterEntry {
    /// The default model to preselect: the first declared model's wire, if any.
    pub fn default_model(&self) -> Option<&str> {
        self.models.first().map(|m| m.wire.as_str())
    }
}

/// The pre-bind model vocabulary for a built-in adapter. Claude's static list
/// carries pretty labels + capability blurbs (the single source lives in
/// `oximux-agents`).
///
/// Codex offers **no** pre-bind models on purpose: its real catalog only arrives
/// from the `codex app-server` `model/list` handshake, and its terminal-launcher
/// static list (`gpt-5-codex`/`o3`) is stale — drafting one of those would carry
/// an unknown model into the bind, which `codex app-server` rejects (the turn
/// fails). Like the ACP presets, it shows no model row until bound, when the live
/// default is selected. Any other built-in falls back to its registry-declared
/// wires as bare `label == wire`.
fn builtin_chat_models(entry: &RegistryEntry) -> Vec<ModelChoice> {
    match entry.adapter_enum {
        AgentAdapter::ClaudeCode => claude_model_choices(),
        AgentAdapter::Codex => Vec::new(),
        _ => entry
            .models
            .iter()
            .map(|m| ModelChoice { wire: (*m).to_string(), label: (*m).to_string(), description: None })
            .collect(),
    }
}

/// Assemble the chat-agent roster from detected built-in adapters plus the ACP
/// presets, in stable order: built-ins first (registry order — Claude, Codex),
/// then presets (Cursor, Amp, OpenCode). Only chat-capable ids are kept, so a
/// user config that demotes a preset id below chat-capable (e.g. a bare
/// `[agents.cursor] model = "…"`) drops it here exactly as it drops from the
/// launcher — no misrouting to Claude.
///
/// `detected` is the async `AdapterRegistry::detect_available()` result; this
/// function itself is pure so it can be unit-tested without touching the FS.
pub(crate) fn chat_roster(
    detected: &[RegistryEntry],
    settings: &AgentLaunchSettings,
) -> Vec<ChatRosterEntry> {
    let mut roster: Vec<ChatRosterEntry> = Vec::new();

    // Built-in chat-capable adapters, carrying their declared static vocab.
    // `chat_capable` already excludes terminal-only Aider; `custom` is the
    // free-form escape hatch and never a chat provider.
    for entry in detected {
        if entry.adapter_id == "custom" || !settings.chat_capable(entry.adapter_id) {
            continue;
        }
        roster.push(ChatRosterEntry {
            id: entry.adapter_id.to_string(),
            display: entry.display_name.to_string(),
            transport: settings.transport_for(entry.adapter_id),
            models: builtin_chat_models(entry),
            efforts: entry.efforts.iter().map(|e| e.to_string()).collect(),
        });
    }

    // ACP presets (Cursor/Amp/OpenCode). Skip any a built-in already covered or
    // a user config demoted below chat-capable. No static model list yet — the
    // ACP session reports its models after binding.
    for preset in ACP_PRESETS {
        if roster.iter().any(|e| e.id == preset.id) || !settings.chat_capable(preset.id) {
            continue;
        }
        roster.push(ChatRosterEntry {
            id: preset.id.to_string(),
            display: preset.title.to_string(),
            transport: settings.transport_for(preset.id),
            models: Vec::new(),
            efforts: Vec::new(),
        });
    }

    roster
}

/// Assemble the chat roster synchronously from live app state: the built-in
/// adapters (Claude/Codex, carrying their static model vocab) plus the ACP
/// presets, filtered by the current [`AgentLaunchSettings`] global. Built
/// without async which-detection — the unbound composer needs the agent + model
/// choices immediately, and a missing binary surfaces at spawn, not here. Falls
/// back to default settings when the global isn't installed (tests / early boot).
pub(crate) fn chat_roster_from_cx(cx: &App) -> Vec<ChatRosterEntry> {
    let detected = AdapterRegistry::with_builtin_adapters().entries_without_detection();
    match cx.try_global::<AgentLaunchSettings>() {
        Some(settings) => chat_roster(&detected, settings),
        None => chat_roster(&detected, &AgentLaunchSettings::default()),
    }
}

/// State of the last worktree-create attempt for an unbound *New Agent*
/// draft's "Run in a fresh worktree" toggle. `Idle` before any attempt (and
/// after a successful one, since the toggle collapses on success); `Creating`
/// while the async git step runs; `Failed` surfaces an inline error + the
/// "continue without a worktree" fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum WorktreeCreateState {
    #[default]
    Idle,
    Creating,
    Failed(String),
}

/// A default slug suggestion for the worktree toggle, derived from the
/// current time so two drafts opened back-to-back don't collide on the same
/// branch/directory. Format: `agent-<unix-seconds>` — short, always passes
/// `validate_slug`, and recognizable in `git branch`/the worktree list.
pub(crate) fn default_worktree_slug() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("agent-{secs}")
}

/// Turn a raw worktree-create failure into a headline a person can act on, plus
/// an optional second line.
///
/// The raw chain reads e.g. `add_worktree: git exited with code 255: Preparing
/// worktree (new branch 'oximux/x')\nfatal: a branch named 'oximux/x' already
/// exists` — an internal fn name, an exit code, and git's own progress chatter
/// wrapped around the one clause that matters. Showing that verbatim asks the
/// user to parse our stack trace.
///
/// `branch` is passed in rather than scraped from the text: the caller already
/// knows the slug it tried, so the headline can name it without a regex that
/// would drift the moment git rewords its output.
///
/// Recognized cases get a clean headline and a next step. **Anything else keeps
/// git's own `fatal:`/`error:` line** — an unrecognized failure the user cannot
/// see is one they cannot search for or report, so this humanizes what it knows
/// and passes through what it doesn't.
fn humanize_worktree_error(raw: &str, branch: &str) -> (String, Option<String>) {
    const PICK_ANOTHER: &str = "Pick a different branch name, or continue without a worktree.";
    // git: "fatal: a branch named 'oximux/x' already exists"
    if raw.contains("a branch named") && raw.contains("already exists") {
        return (format!("Branch {branch} already exists"), Some(PICK_ANOTHER.to_string()));
    }
    // git: "fatal: '<path>' already exists" — the sibling directory is occupied
    // even though the branch itself is free.
    if raw.contains("already exists") {
        return (format!("A folder for {branch} already exists"), Some(PICK_ANOTHER.to_string()));
    }
    if raw.contains("not a valid object name") || raw.contains("invalid reference") {
        return (
            "Couldn't branch from the current HEAD".to_string(),
            Some("This repository may not have any commits yet.".to_string()),
        );
    }
    // Unrecognized: lead with a plain headline but surface git's own last
    // diagnostic line, which is the part worth searching for.
    let detail = raw
        .lines()
        .rev()
        .find(|l| {
            let t = l.trim_start();
            t.starts_with("fatal:") || t.starts_with("error:")
        })
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| raw.trim().to_string());
    ("Couldn't create the worktree".to_string(), Some(detail))
}

impl AgentChatView {
    /// Flip the *New Agent* draft's "Run in a fresh worktree" toggle. No-op
    /// once bound (a live chat's cwd is fixed), for a non-git project (the
    /// toggle is never rendered there, but this guards a stray dispatch too),
    /// or while a create is in flight / has failed with a message still
    /// staged (`worktree_create_state != Idle`) — unchecking there would
    /// silently discard `pending_worktree_send` (MEDIUM finding). The user
    /// must resolve via the failure banner's Retry / "continue without a
    /// worktree" instead, both of which route the staged message onward.
    /// Set the draft's isolation to `enabled` (the composer pill emits the
    /// DESIRED state, not a flip, so a re-pick of the active row is a no-op).
    /// Delegates the actual flip — and every guard on it — to
    /// [`Self::toggle_worktree_draft`], so there is one rule about when the
    /// choice may change, not two.
    pub(super) fn set_worktree_isolation(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_draft_enabled == enabled {
            return;
        }
        self.toggle_worktree_draft(window, cx);
    }

    pub(super) fn toggle_worktree_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.unbound || !self.is_git_project {
            return;
        }
        if !matches!(self.worktree_create_state, WorktreeCreateState::Idle) {
            return;
        }
        self.worktree_draft_enabled = !self.worktree_draft_enabled;
        self.reconcile_worktree_slug_input(window, cx);
        self.sync_composer(cx);
        cx.notify();
    }

    /// Create (or drop) the slug `InputState` to match `worktree_draft_enabled`
    /// — mirrors `reconcile_env_inputs`' create-on-demand pattern. Seeded with
    /// a timestamp-based default slug so Send works immediately without typing.
    /// Called once up front in `render` (needs `Window` for `InputState::new`,
    /// which the render-time reconcile pattern already establishes elsewhere).
    pub(super) fn reconcile_worktree_slug_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.unbound || !self.is_git_project || !self.worktree_draft_enabled {
            self.worktree_slug_input = None;
            self._worktree_slug_sub = None;
            return;
        }
        if self.worktree_slug_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("slug")
                    .default_value(default_worktree_slug())
            });
            // Re-push on every keystroke so the live `oximux/<slug>` preview /
            // validation error / pill label track what the user is typing. A
            // bare `cx.notify()` is NOT enough now that those render inside the
            // composer: the hint is computed here and pushed across, so without
            // the re-sync the parent would repaint while the popover kept the
            // stale hint. (An embedded `Input` doesn't repaint its owner either,
            // which is why the subscription exists at all.)
            let sub = cx.subscribe(&input, |this, _input, _ev: &InputEvent, cx| {
                this.sync_unbound_composer(cx);
                cx.notify();
            });
            self.worktree_slug_input = Some(input);
            self._worktree_slug_sub = Some(sub);
        }
    }

    /// Commit the worktree-toggle's staged first send: validate the typed
    /// slug, create the worktree (async git op) via `workspace_ops`, and stage
    /// `text`/`images` to resume through `send_text` once it lands. On a
    /// validation or git failure the message stays staged — the failure
    /// banner's Retry re-enters here with the same text, and "continue
    /// without a worktree" ([`Self::send_without_worktree`]) sends it as a
    /// plain (non-worktree) draft.
    pub(super) fn start_worktree_then_send(
        &mut self,
        text: String,
        images: Vec<ChatImage>,
        cx: &mut Context<Self>,
    ) {
        let raw_slug = self
            .worktree_slug_input
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        let slug = if raw_slug.trim().is_empty() {
            default_worktree_slug()
        } else {
            raw_slug.trim().to_string()
        };
        self.pending_worktree_send = Some((text, images));
        if let Err(err) = validate_slug(&slug) {
            self.worktree_create_state = WorktreeCreateState::Failed(err.to_string());
            self.sync_composer(cx);
            cx.notify();
            return;
        }
        // Fold `Creating` into the composer's own `disconnected` NOW (not just
        // on the eventual outcome) — this is what makes a second, distinct
        // Submit a no-op in the composer's `submit()` before it ever reaches
        // `send_text` again (see `sync_composer` and the HIGH finding it
        // documents).
        self.worktree_create_state = WorktreeCreateState::Creating;
        self.sync_composer(cx);
        // Route up: the leaf carries no `WorkspaceRepo`, so ask the host to make
        // the worktree a first-class `Workspace` (DB row + git worktree). The
        // outcome returns via `on_worktree_create_outcome`, which resumes the
        // send staged above. (A thin-leaf shape — no workspace state on this
        // view; the host owns the storage seam.)
        cx.emit(super::AgentChatEvent::WorktreeWorkspaceRequested { slug });
        cx.notify();
    }

    /// Fold the async worktree-create result back onto the draft: on success,
    /// rebind `cwd` to the new worktree, collapse the toggle, and resume the
    /// staged send (which now spawns the agent there via the normal
    /// `bind_now` path); on failure, surface the error and leave the toggle +
    /// staged message in place for Retry / fallback.
    pub(crate) fn on_worktree_create_outcome(
        &mut self,
        outcome: crate::shell::workspace_ops::ChatWorktreeOutcome,
        cx: &mut Context<Self>,
    ) {
        use crate::shell::workspace_ops::ChatWorktreeOutcome;
        match outcome {
            ChatWorktreeOutcome::Created { path, branch } => {
                self.cwd = path;
                self.worktree_branch_label = Some(branch);
                self.worktree_create_state = WorktreeCreateState::Idle;
                self.worktree_draft_enabled = false;
                self.worktree_slug_input = None;
                self._worktree_slug_sub = None;
                match self.pending_worktree_send.take() {
                    // `send_text` re-syncs the composer itself once it lands.
                    Some((text, images)) => self.send_text(text, images, cx),
                    None => {
                        self.sync_composer(cx);
                        cx.notify();
                    }
                }
            }
            ChatWorktreeOutcome::InvalidSlug(msg) | ChatWorktreeOutcome::GitFailed(msg) => {
                self.worktree_create_state = WorktreeCreateState::Failed(msg);
                self.sync_composer(cx);
                cx.notify();
            }
        }
    }

    /// The failure banner's fallback: drop the worktree attempt entirely and
    /// send the staged message as a plain draft (spawns at the original cwd).
    /// Never leaves a half-created worktree — a failed `add_worktree` already
    /// leaves the repo clean (git only creates the worktree on success).
    pub(super) fn send_without_worktree(&mut self, cx: &mut Context<Self>) {
        self.worktree_draft_enabled = false;
        self.worktree_create_state = WorktreeCreateState::Idle;
        self.worktree_slug_input = None;
        self._worktree_slug_sub = None;
        match self.pending_worktree_send.take() {
            // `send_text` re-syncs the composer itself once it lands (its own
            // tail call), so no separate sync is needed on this branch.
            Some((text, images)) => self.send_text(text, images, cx),
            None => self.sync_composer(cx),
        }
    }

    /// The failure banner's Retry: re-attempt worktree creation with the same
    /// staged message (the user may have edited the slug field first).
    pub(super) fn retry_worktree_create(&mut self, cx: &mut Context<Self>) {
        let Some((text, images)) = self.pending_worktree_send.clone() else {
            return;
        };
        self.worktree_create_state = WorktreeCreateState::Idle;
        self.start_worktree_then_send(text, images, cx);
    }

    /// The worktree draft's state, projected for the composer's pill. `None` once
    /// bound or for a non-git project — the pill never appears there.
    ///
    /// The `hint` is computed here rather than in the composer because this view
    /// owns `validate_slug` and the slug's `InputState`; the composer gets a
    /// render-ready snapshot plus a shared handle to the field itself, so the
    /// slug text lives in exactly one place.
    pub(super) fn worktree_draft_for_composer(&self, cx: &Context<Self>) -> Option<WorktreeDraft> {
        if !self.unbound || !self.is_git_project {
            return None;
        }
        let enabled = self.worktree_draft_enabled;
        let (hint, hint_is_error) = match self.worktree_slug_input.as_ref() {
            Some(input) if enabled => {
                let slug_text = input.read(cx).value().to_string();
                let trimmed = slug_text.trim();
                match validate_slug(trimmed) {
                    Ok(()) if !trimmed.is_empty() => (format!("oximux/{trimmed}"), false),
                    Ok(()) => ("oximux/…".to_string(), false),
                    Err(err) => (err.to_string(), true),
                }
            }
            _ => (String::new(), false),
        };
        Some(WorktreeDraft {
            enabled,
            slug_input: self.worktree_slug_input.clone(),
            // Creating OR Failed — anything other than Idle means a message is
            // staged in `pending_worktree_send` and the pick must not flip
            // (`toggle_worktree_draft` itself refuses in this state; this just
            // carries the rule to the pill so it can render disabled).
            busy: !matches!(self.worktree_create_state, WorktreeCreateState::Idle),
            hint,
            hint_is_error,
        })
    }

    /// The worktree create's transient feedback — shown above the composer while
    /// a create is in flight or has failed with a message still staged. `None` at
    /// rest: the isolation *choice* lives in the composer's pill, this is only
    /// the in-flight/failure state, which belongs with the other pinned banners
    /// rather than inside a popover the user has to open to see.
    ///
    /// Laid out in the composer's own centered reading column. It is a sibling of
    /// the composer, not a child, so nothing gives it that column for free — a
    /// bare full-width row here strands the text against the pane's left edge,
    /// hundreds of px from the input it belongs to, and the gap grows with the
    /// window.
    pub(super) fn render_worktree_status_banner(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.unbound || !self.is_git_project {
            return None;
        }
        // At rest there is nothing to say — bail before building anything so a
        // draft doesn't carry an empty padded strip above its composer. (The old
        // toggle always rendered because the checkbox itself lived here.)
        if matches!(self.worktree_create_state, WorktreeCreateState::Idle) {
            return None;
        }
        if !self.worktree_draft_enabled {
            return None;
        }
        let theme = self.theme;
        let typo = self.typography.clone();
        let pad = self.density.pad_panel;

        let body: AnyElement = match &self.worktree_create_state {
            WorktreeCreateState::Creating => div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_subtle)
                .child("Creating worktree…")
                .into_any_element(),
            WorktreeCreateState::Failed(msg) => {
                let branch = self
                    .worktree_slug_input
                    .as_ref()
                    .map(|i| format!("oximux/{}", i.read(cx).value().trim()))
                    .unwrap_or_else(|| "the branch".to_string());
                let (headline, detail) = humanize_worktree_error(msg, &branch);
                // Matches `error_card.rs`'s established failure look rather than
                // inventing a second error style.
                let mut card = div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_w_0()
                    .gap(px(6.0))
                    .rounded(px(self.density.r_card))
                    .border_1()
                    .border_color(theme.status_error.opacity(0.4))
                    .bg(theme.status_error.opacity(0.08))
                    .px(px(12.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                Icon::default()
                                    .path("icons/alert-triangle.svg")
                                    .size(px(13.0))
                                    .text_color(theme.status_error),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(typo.t_body_sm))
                                    .text_color(theme.status_error)
                                    .child(headline),
                            ),
                    );
                if let Some(detail) = detail {
                    card = card.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_size(px(typo.t_body_sm))
                            .text_color(theme.fg_muted)
                            .child(detail),
                    );
                }
                card.child(
                    // Actions sit under their own message rather than flung to
                    // the far edge by a spacer — at this column's width that put
                    // them ~600px from the text explaining them.
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(6.0))
                        .child(
                            Button::new("worktree-create-fallback")
                                .ghost()
                                .small()
                                .label("Continue without worktree")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.send_without_worktree(cx);
                                })),
                        )
                        .child(
                            Button::new("worktree-create-retry")
                                .outline()
                                .small()
                                .label("Retry")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.retry_worktree_create(cx);
                                })),
                        ),
                )
                .into_any_element()
            }
            WorktreeCreateState::Idle => return None,
        };

        Some(
            div()
                .flex()
                .flex_col()
                .items_center()
                .w_full()
                .px(px(pad))
                .pb(px(6.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .min_w_0()
                        .max_w(px(super::CONTENT_MAX_W))
                        .child(body),
                )
                .into_any_element(),
        )
    }

    /// Footer for an import-bridge tab (OpenCode / Pi opened as chat): a short
    /// note that this is an imported, backend-less session + a **Resume in
    /// terminal** button that re-dispatches the provider's PTY resume via
    /// [`crate::actions::ResumeAgentSession`]. Swaps in for the live composer so
    /// there is no fake chat-send. Rendered only when `import_bridge` is set.
    pub(super) fn render_import_bridge_footer(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = self.theme;
        let typo = &self.typography;
        let Some(bridge) = self.import_bridge.clone() else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .px(px(10.0))
            .py(px(8.0))
            .gap(px(6.0))
            .border_t_1()
            .border_color(theme.border_inactive)
            .child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(format!(
                        "Imported {} session — this provider has no in-app chat; resume continues it in a terminal.",
                        bridge.provider_display
                    )),
            )
            .child(
                div().flex().flex_row().child(
                    Button::new("import-bridge-resume")
                        .label("Resume in terminal")
                        .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
                            // Emit an event (not `window.dispatch_action` from this
                            // render closure — it doesn't reach the host's action
                            // handlers); the pane group turns it into the provider's
                            // PTY resume. Same seam as `OpenLoginTerminalRequested`.
                            cx.emit(super::AgentChatEvent::ResumeInTerminalRequested {
                                preset_id: bridge.preset_id.clone(),
                                resume_handle: bridge.resume_handle.clone(),
                                session_id: bridge.session_id.clone(),
                                cwd: bridge.cwd.clone(),
                            });
                        })),
                ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::AgentAdapter;

    /// The real failure a user hits: the branch already exists. The headline must
    /// name the branch and say nothing about `add_worktree` or exit codes.
    #[test]
    fn humanize_branch_exists_names_the_branch_and_drops_the_git_chatter() {
        let raw = "add_worktree: git exited with code 255: Preparing worktree (new branch \
                   'oximux/xxx')\nfatal: a branch named 'oximux/xxx' already exists";
        let (headline, detail) = humanize_worktree_error(raw, "oximux/xxx");
        assert_eq!(headline, "Branch oximux/xxx already exists");
        assert_eq!(
            detail.as_deref(),
            Some("Pick a different branch name, or continue without a worktree.")
        );
        // The whole point: none of our plumbing reaches the user.
        for leak in ["add_worktree", "exited with code", "Preparing worktree", "fatal:"] {
            assert!(!headline.contains(leak), "headline leaks {leak:?}: {headline}");
            assert!(
                !detail.as_deref().unwrap().contains(leak),
                "detail leaks {leak:?}"
            );
        }
    }

    /// A path collision reports the folder, not the branch — the branch is free,
    /// so "branch already exists" would send the user to the wrong fix.
    #[test]
    fn humanize_path_collision_reports_the_folder() {
        let raw = "add_worktree: git exited with code 128: fatal: '/tmp/oximux-wt-x' already exists";
        let (headline, _) = humanize_worktree_error(raw, "oximux/x");
        assert_eq!(headline, "A folder for oximux/x already exists");
    }

    /// An UNRECOGNIZED failure must still surface git's own diagnostic. Swallowing
    /// it would leave the user with a headline they can neither act on nor search
    /// for — worse than the raw dump this humanizer replaces.
    #[test]
    fn humanize_unknown_error_passes_through_gits_own_line() {
        let raw = "add_worktree: git exited with code 128: some progress noise\n\
                   fatal: could not create work tree dir 'x': Permission denied";
        let (headline, detail) = humanize_worktree_error(raw, "oximux/x");
        assert_eq!(headline, "Couldn't create the worktree");
        assert_eq!(
            detail.as_deref(),
            Some("fatal: could not create work tree dir 'x': Permission denied"),
            "the last fatal: line is the part worth searching for"
        );
    }

    /// No `fatal:`/`error:` line to find — fall back to the raw text rather than
    /// showing an empty detail.
    #[test]
    fn humanize_unknown_error_without_a_fatal_line_keeps_the_raw_text() {
        let (headline, detail) = humanize_worktree_error("something strange happened", "oximux/x");
        assert_eq!(headline, "Couldn't create the worktree");
        assert_eq!(detail.as_deref(), Some("something strange happened"));
    }

    /// The built-in registry as `detect_available()` would report it (all
    /// available), so tests exercise `chat_roster` without touching the FS.
    fn builtin_entries() -> Vec<RegistryEntry> {
        vec![
            RegistryEntry {
                adapter_id: "claude-code",
                display_name: "Claude Code",
                adapter_enum: AgentAdapter::ClaudeCode,
                available: true,
                models: &["opus", "sonnet", "haiku"],
                efforts: &["high", "medium", "low"],
            },
            RegistryEntry {
                adapter_id: "codex",
                display_name: "Codex",
                adapter_enum: AgentAdapter::Codex,
                available: true,
                models: &["gpt-5-codex", "o3"],
                efforts: &[],
            },
            RegistryEntry {
                adapter_id: "pi",
                display_name: "Pi",
                adapter_enum: AgentAdapter::Pi,
                available: true,
                models: &[],
                efforts: &[],
            },
            RegistryEntry {
                adapter_id: "aider",
                display_name: "Aider",
                adapter_enum: AgentAdapter::Aider,
                available: true,
                models: &[],
                efforts: &[],
            },
            RegistryEntry {
                adapter_id: "custom",
                display_name: "Custom",
                adapter_enum: AgentAdapter::Custom,
                available: true,
                models: &[],
                efforts: &[],
            },
        ]
    }

    #[test]
    fn roster_lists_chat_capable_builtins_then_presets() {
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&builtin_entries(), &s);
        let ids: Vec<&str> = roster.iter().map(|e| e.id.as_str()).collect();
        // Built-ins first (Claude, Codex, Pi), then the three ACP presets. Aider
        // (terminal-only) and Custom are absent.
        assert_eq!(ids, ["claude-code", "codex", "pi", "cursor", "amp", "opencode"]);
    }

    /// The roster is built by filtering the registry through `chat_capable`, so a
    /// chat backend the registry omits vanishes from every agent menu while every
    /// unit test still passes — `chat_capable` is simply never asked about it.
    /// Pi shipped in exactly that state; this pins the whole path, not the flag.
    #[test]
    fn a_chat_capable_builtin_missing_from_the_registry_is_unreachable() {
        let s = AgentLaunchSettings::default();
        assert!(s.chat_capable("pi"), "pi is chat-capable by the gate's own reckoning");

        // Registry without pi — what shipped before this was caught by driving
        // the real app.
        let without_pi: Vec<RegistryEntry> =
            builtin_entries().into_iter().filter(|e| e.adapter_id != "pi").collect();
        let ids: Vec<String> =
            chat_roster(&without_pi, &s).into_iter().map(|e| e.id).collect();
        assert!(
            !ids.iter().any(|id| id == "pi"),
            "the gate says yes, yet no menu can offer pi — being chat_capable is not enough"
        );

        // With the entry, it lands, carrying its own transport.
        let roster = chat_roster(&builtin_entries(), &s);
        let pi = roster.iter().find(|e| e.id == "pi").expect("pi is offered");
        assert_eq!(pi.transport, Transport::Rpc);
    }

    #[test]
    fn builtins_carry_their_static_transport_and_vocab() {
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&builtin_entries(), &s);
        let claude = roster.iter().find(|e| e.id == "claude-code").unwrap();
        assert_eq!(claude.transport, Transport::StreamJson);
        // Claude's pre-bind vocab is the rich shared list: pretty labels + blurbs,
        // not the bare registry wires.
        let claude_wires: Vec<&str> = claude.models.iter().map(|m| m.wire.as_str()).collect();
        assert_eq!(claude_wires, ["opus", "fable", "sonnet", "haiku"]);
        assert_eq!(claude.models[0].label, "Opus");
        // The blurb is where the version shows, so the pre-bind draft names the
        // same model the bound session will report.
        assert_eq!(
            claude.models[0].description.as_deref(),
            Some("Opus 5 · Best for everyday, complex tasks"),
        );
        assert_eq!(claude.efforts, ["high", "medium", "low"]);
        assert_eq!(claude.default_model(), Some("opus"));
        // Codex offers no pre-bind models: its stale terminal-launcher list would
        // draft an unknown model that `codex app-server` rejects on bind, so the
        // real catalog only loads post-handshake (mirrors the ACP presets).
        let codex = roster.iter().find(|e| e.id == "codex").unwrap();
        assert_eq!(codex.transport, Transport::AppServer);
        assert!(codex.models.is_empty(), "Codex has no static pre-bind models");
        assert_eq!(codex.default_model(), None);
        assert!(codex.efforts.is_empty());
    }

    #[test]
    fn acp_presets_are_acp_with_no_static_model_row_yet() {
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&builtin_entries(), &s);
        for id in ["cursor", "amp", "opencode"] {
            let e = roster.iter().find(|e| e.id == id).unwrap();
            assert_eq!(e.transport, Transport::Acp, "{id} should be ACP");
            assert!(e.models.is_empty(), "{id} has no static models yet");
            assert_eq!(e.default_model(), None);
        }
    }

    #[test]
    fn a_user_demoted_preset_drops_from_the_roster() {
        // A bare `[agents.cursor] model = "…"` (no transport/command) suppresses
        // the preset and is not chat-capable — it must not appear, mirroring the
        // launcher's guard against misrouting a preset click to Claude.
        let s = AgentLaunchSettings::from_toml_str("[agents.cursor]\nmodel = \"foo\"\n")
            .expect("parse");
        let roster = chat_roster(&builtin_entries(), &s);
        assert!(roster.iter().all(|e| e.id != "cursor"));
        // The other presets are unaffected.
        assert!(roster.iter().any(|e| e.id == "opencode"));
    }

    #[test]
    fn empty_detection_still_yields_the_presets() {
        // Even if the built-in binaries aren't detected, the ACP presets (their
        // own which-detection happens in the UI layer) still populate the roster.
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&[], &s);
        let ids: Vec<&str> = roster.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["cursor", "amp", "opencode"]);
    }

    #[test]
    fn default_worktree_slug_passes_validate_slug() {
        // Whatever the current time produces must always be a legal branch
        // component — the toggle relies on this being usable without editing.
        let slug = default_worktree_slug();
        assert!(validate_slug(&slug).is_ok(), "{slug:?} should validate");
        assert!(slug.starts_with("agent-"));
    }

    #[test]
    fn worktree_create_state_defaults_to_idle() {
        assert_eq!(WorktreeCreateState::default(), WorktreeCreateState::Idle);
    }

    #[test]
    fn worktree_create_state_failed_carries_the_message() {
        let state = WorktreeCreateState::Failed("slug is empty".to_string());
        match state {
            WorktreeCreateState::Failed(msg) => assert_eq!(msg, "slug is empty"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
