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

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, AnyElement, App, AppContext, ClickEvent, Context, IntoElement, ParentElement, Styled,
    Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
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

impl AgentChatView {
    /// Flip the *New Agent* draft's "Run in a fresh worktree" toggle. No-op
    /// once bound (a live chat's cwd is fixed), for a non-git project (the
    /// toggle is never rendered there, but this guards a stray dispatch too),
    /// or while a create is in flight / has failed with a message still
    /// staged (`worktree_create_state != Idle`) — unchecking there would
    /// silently discard `pending_worktree_send` (MEDIUM finding). The user
    /// must resolve via the failure banner's Retry / "continue without a
    /// worktree" instead, both of which route the staged message onward.
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
            // Repaint on every keystroke so the live `oximux/<slug>` preview /
            // validation error tracks what the user is typing (an embedded
            // `Input` doesn't self-repaint its owner).
            let sub = cx.subscribe(&input, |_this, _input, _ev: &InputEvent, cx| cx.notify());
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
        // send staged above. (The Orca thin-leaf shape — no workspace state on
        // this view; the host owns the storage seam.)
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

    /// Render the *New Agent* draft's "Run in a fresh worktree" toggle +
    /// (while on) the slug field and any create-state feedback. `None` once
    /// bound or for a non-git project — the toggle never appears there.
    pub(super) fn render_worktree_toggle(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.unbound || !self.is_git_project {
            return None;
        }
        let theme = self.theme;
        let typo = self.typography.clone();
        let enabled = self.worktree_draft_enabled;
        let creating = matches!(self.worktree_create_state, WorktreeCreateState::Creating);
        // Creating OR Failed — anything other than Idle means a message is
        // staged in `pending_worktree_send` and the toggle must not flip
        // (`toggle_worktree_draft` itself refuses in this state; the dim here
        // is just the visual cue, since this widget has no `disabled` prop).
        let busy = !matches!(self.worktree_create_state, WorktreeCreateState::Idle);

        let mut col = div()
            .flex()
            .flex_col()
            .px(px(10.0))
            .pb(px(6.0))
            .gap(px(4.0))
            .child(
                div().when(busy, |d| d.opacity(0.5)).child(
                    Checkbox::new("worktree-draft-toggle")
                        .checked(enabled)
                        .label("Run in a fresh worktree")
                        .on_click(cx.listener(|this, _checked: &bool, window, cx| {
                            this.toggle_worktree_draft(window, cx);
                        })),
                ),
            );

        if enabled {
            if let Some(input) = self.worktree_slug_input.clone() {
                let slug_text = input.read(cx).value().to_string();
                let trimmed = slug_text.trim();
                let validity = validate_slug(trimmed);
                let hint = match &validity {
                    Ok(()) if !trimmed.is_empty() => format!("oximux/{trimmed}"),
                    Ok(()) => "oximux/…".to_string(),
                    Err(err) => err.to_string(),
                };
                let hint_color = if validity.is_err() { theme.status_error } else { theme.fg_subtle };
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().w(px(160.0)).child(Input::new(&input)))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(typo.t_body_sm))
                                .text_color(hint_color)
                                .child(hint),
                        ),
                );
            }
            if creating {
                col = col.child(
                    div()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child("Creating worktree…"),
                );
            }
            if let WorktreeCreateState::Failed(msg) = &self.worktree_create_state {
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(typo.t_body_sm))
                                .text_color(theme.status_error)
                                .child(format!("Couldn't create worktree: {msg}")),
                        )
                        .child(
                            Button::new("worktree-create-retry")
                                .ghost()
                                .label("Retry")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.retry_worktree_create(cx);
                                })),
                        )
                        .child(
                            Button::new("worktree-create-fallback")
                                .ghost()
                                .label("Continue without worktree")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.send_without_worktree(cx);
                                })),
                        ),
                );
            }
        }

        Some(col.into_any_element())
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
        // Built-ins first (Claude, Codex), then the three ACP presets. Aider
        // (terminal-only) and Custom are absent.
        assert_eq!(ids, ["claude-code", "codex", "cursor", "amp", "opencode"]);
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
        assert_eq!(claude_wires, ["opus", "sonnet", "haiku"]);
        assert_eq!(claude.models[0].label, "Opus");
        assert!(claude.models[0].description.is_some(), "Claude models carry a blurb pre-bind");
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
