//! First-run onboarding — the gate deciding whether the welcome wizard opens.
//!
//! The wizard itself (entity + views) lives in sibling modules; this file owns
//! the persisted completion flag and the boot-time decision. The decision is
//! communicated to the first workspace window through a one-shot pending
//! mailbox (same idiom as `window_registry::consume_pending_tearoff`) so the
//! shared window factory keeps its signature: Cmd+N windows and restored
//! windows never consume it, only the fresh-boot window armed by `main.rs`.

mod agent_step;
#[cfg(target_os = "macos")]
mod driver_step;
mod view;
mod view_step;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, EventEmitter, FocusHandle, Subscription, Window};
use gpui_component::searchable_list::SearchableVec;
use gpui_component::select::{SelectEvent, SelectState};
use oximux_agents::AdapterRegistry;
use oximux_settings::{AgentLaunchSettings, Density, OpenMode, Theme, Typography};
use oximux_storage::SettingsRepo;

use agent_step::{AgentRow, ModelSource, OnboardModelItem};

/// SettingsRepo key marking onboarding as completed (or deliberately skipped).
/// Presence is what matters, not the value; versioned so a future richer
/// onboarding can re-trigger by minting a `.v2` key.
pub const COMPLETED_SETTING: &str = "onboarding.completed.v1";

/// The boot-time gate: show the wizard only on a true fresh install — no
/// windows to restore AND the completion flag has never been written. Existing
/// installs (non-empty manifest) are backfilled by the caller instead, so an
/// upgrade never shows the wizard over a working setup.
pub fn should_show_onboarding(manifest_empty: bool, flag_present: bool) -> bool {
    manifest_empty && !flag_present
}

/// One-shot "open the wizard in the next workspace window" mailbox, armed by
/// `main.rs` before the fresh-boot `open_workspace_window` call and consumed
/// exactly once by `WorkspaceRoot` construction.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Arm the mailbox (fresh-boot path in `main.rs` only).
pub fn set_pending() {
    PENDING.store(true, Ordering::SeqCst);
}

/// Consume the mailbox. Returns `true` at most once per arming.
pub fn take_pending() -> bool {
    PENDING.swap(false, Ordering::SeqCst)
}

/// Emitted when the wizard closes (Finish or Skip). `WorkspaceRoot` listens
/// and returns keyboard focus to itself so global bindings keep dispatching —
/// the wizard grabs focus on open, and an orphaned focus handle after close
/// would silently kill every chord.
pub enum OnboardingEvent {
    Closed,
}

/// Which wizard screen is showing. Agent and chat-view always; the driver
/// step only when the computer-use driver is missing or stale at open (a
/// machine that already has it verified never sees the step). The "You're
/// set" summary exists only in the review mockup, not in the product: Finish
/// closes straight onto the welcome empty-state card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Agent,
    ChatView,
    Driver,
}

/// The first-run welcome wizard: a full-window occluding overlay, deliberately
/// NOT dismissable by clicking the backdrop — Skip (or Esc) is the only way
/// out without finishing, so a stray click can't half-complete onboarding.
pub struct OnboardingWizard {
    pub(super) open: bool,
    pub(super) step: OnboardingStep,
    pub(super) focus_handle: FocusHandle,
    pub(super) theme: Theme,
    pub(super) density: Density,
    pub(super) typography: Typography,
    /// Flat KV store the completion flag persists into. Finish and Skip both
    /// write it — presence of the key is what the boot gate checks.
    settings_repo: SettingsRepo,
    /// Adapter inventory shared with the launch picker — the roster and the
    /// PATH detection both come from it.
    registry: Arc<AdapterRegistry>,
    /// The picker rows, assembled once per open; availability lands as one
    /// async update (rows grey out in place, never reshuffle mid-interaction).
    pub(self) rows: Vec<AgentRow>,
    /// Whether the "+N more" group is revealed.
    pub(self) expanded: bool,
    /// The currently highlighted agent (adapter id).
    pub(self) selected_agent: Option<String>,
    /// Step 2's choice. Preselected to `Chat` (the differentiated surface) —
    /// a deliberate product call; the CODE default stays `Terminal`, so a Skip
    /// changes nothing for users who never saw or wanted the wizard.
    pub(self) open_mode: OpenMode,
    /// Explicit model picks per agent id (wire values). Only explicit picks
    /// persist on Finish — an untouched dropdown writes nothing.
    pub(self) chosen_models: BTreeMap<String, String>,
    /// Static model slugs per adapter id (from the registry), the Codex
    /// fallback when the catalog cache is cold.
    static_models: HashMap<String, &'static [&'static str]>,
    /// The searchable model dropdown. Built lazily on first open (needs a
    /// `Window`); re-seeded whenever the selected agent changes.
    pub(self) model_select: Option<Entity<SelectState<SearchableVec<OnboardModelItem>>>>,
    /// The `(wire, label, description)` set last pushed into the select, so
    /// re-renders don't reset an open dropdown (composer's signature guard).
    model_select_sig: Vec<(String, String, Option<String>)>,
    _model_select_sub: Option<Subscription>,
    /// Driver check made once at open (it spawns `codesign`); decides whether
    /// the Driver step exists at all and what its body says.
    #[cfg(target_os = "macos")]
    pub(self) driver_status: crate::shell::settings_modal::DriverStatus,
    /// Frozen at open: whether this run includes the Driver step. Deliberately
    /// NOT recomputed from live state — a successful install mid-step must not
    /// change the step count (dots) under the user's feet.
    pub(self) driver_step_planned: bool,
    /// Set when the install started from this wizard replaced an existing
    /// driver — gates the "old version until the daemon respawns" note.
    #[cfg(target_os = "macos")]
    pub(self) driver_upgraded: bool,
    /// The install this wizard started, plus what its step renders — the same
    /// state machine the settings pane uses (`driver_install`).
    #[cfg(target_os = "macos")]
    pub(self) driver_install: Option<crate::shell::driver_install::InstallHandle>,
    #[cfg(target_os = "macos")]
    pub(self) driver_install_ui: crate::shell::driver_install::DriverInstallUi,
    #[cfg(target_os = "macos")]
    pub(self) driver_poll_running: bool,
}

impl OnboardingWizard {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        settings_repo: SettingsRepo,
        registry: Arc<AdapterRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            open: false,
            step: OnboardingStep::Agent,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            settings_repo,
            registry,
            rows: Vec::new(),
            expanded: false,
            selected_agent: None,
            open_mode: OpenMode::Chat,
            chosen_models: BTreeMap::new(),
            static_models: HashMap::new(),
            model_select: None,
            model_select_sig: Vec::new(),
            _model_select_sub: None,
            #[cfg(target_os = "macos")]
            driver_status: crate::shell::settings_modal::DriverStatus::Unknown,
            driver_step_planned: false,
            #[cfg(target_os = "macos")]
            driver_upgraded: false,
            #[cfg(target_os = "macos")]
            driver_install: None,
            #[cfg(target_os = "macos")]
            driver_install_ui: crate::shell::driver_install::DriverInstallUi::Idle,
            #[cfg(target_os = "macos")]
            driver_poll_running: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open (or reopen, via the palette action) at step 1. Takes focus so
    /// Esc/Enter dispatch here instead of leaking to the pane below.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.step = OnboardingStep::Agent;
        self.expanded = false;
        // Once per open, not per transition: the step count (and dot row)
        // must be stable for the whole run of the wizard.
        // No screen-control driver to install off macOS, so the wizard is one
        // step shorter there rather than showing a step that cannot succeed.
        #[cfg(target_os = "macos")]
        {
            self.driver_status = crate::shell::settings_modal::DriverStatus::resolve();
            self.driver_step_planned = self.driver_status.install_label().is_some();
            self.driver_upgraded = false;
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.driver_step_planned = false;
        }
        self.build_roster(cx);
        self.ensure_model_select(window, cx);
        self.sync_model_select(window, cx);
        self.spawn_detection(window, cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Assemble the picker rows from the registry + ACP presets. Runs on every
    /// open (cheap; no I/O) so a reopen reflects current chat-capability.
    fn build_roster(&mut self, cx: &mut Context<Self>) {
        let entries = self.registry.entries_without_detection();
        self.static_models =
            entries.iter().map(|e| (e.adapter_id.to_string(), e.models)).collect();
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.adapter_id.to_string(), e.display_name.to_string()))
            .collect();
        let launch = cx.global::<AgentLaunchSettings>();
        self.rows = agent_step::assemble_roster(&pairs, |id| launch.chat_capable(id));
        // Preselect the first chat-capable row (mirrors default_chat_agent's
        // builtins-first order); detection may move it if that CLI is missing.
        if self.selected_agent.is_none() {
            self.selected_agent =
                self.rows.iter().find(|r| r.chat_capable).map(|r| r.id.clone());
        }
    }

    /// One-shot PATH detection per open: registry adapters + ACP preset
    /// commands under a shared 500ms cap (same contract as the launch picker).
    /// Applies as a single state update — rows grey in place, no reshuffle.
    fn spawn_detection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let registry = self.registry.clone();
        cx.spawn_in(window, async move |this, cx| {
            let detect = async {
                let entries = registry.detect_available().await;
                let mut presets = Vec::with_capacity(oximux_settings::ACP_PRESETS.len());
                for preset in oximux_settings::ACP_PRESETS {
                    presets.push((preset.id, oximux_agents::cli::which_on_path(preset.command).await));
                }
                (entries, presets)
            };
            let Ok((entries, presets)) =
                tokio::time::timeout(Duration::from_millis(500), detect).await
            else {
                tracing::warn!("onboarding: agent detection timed out after 500ms");
                return;
            };
            let _ = this.update_in(cx, |wizard, window, cx| {
                for row in &mut wizard.rows {
                    let hit = entries
                        .iter()
                        .find(|e| e.adapter_id == row.id)
                        .map(|e| e.available)
                        .or_else(|| {
                            presets.iter().find(|(id, _)| *id == row.id).map(|(_, ok)| *ok)
                        });
                    row.available = hit;
                }
                // If the optimistic preselection turned out to be missing,
                // move to the first installed chat-capable row.
                let selected_missing = wizard
                    .selected_row()
                    .is_some_and(|r| !r.selectable());
                if selected_missing {
                    wizard.selected_agent = wizard
                        .rows
                        .iter()
                        .find(|r| r.chat_capable && r.selectable())
                        .map(|r| r.id.clone());
                    wizard.sync_model_select(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn selected_row(&self) -> Option<&AgentRow> {
        let id = self.selected_agent.as_deref()?;
        self.rows.iter().find(|r| r.id == id)
    }

    /// Click / arrow-key selection of an agent row. Re-seeds the model select
    /// for the newly selected agent.
    pub(self) fn select_agent(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_agent.as_deref() == Some(id) {
            return;
        }
        self.selected_agent = Some(id.to_string());
        self.sync_model_select(window, cx);
        cx.notify();
    }

    /// Arrow-key navigation across selectable rows, auto-expanding the "+N
    /// more" group when the target sits inside it.
    pub(self) fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let selectable: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.selectable())
            .map(|r| r.id.clone())
            .collect();
        if selectable.is_empty() {
            return;
        }
        let current = self
            .selected_agent
            .as_deref()
            .and_then(|id| selectable.iter().position(|s| s == id))
            .unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(selectable.len() - 1);
        let id = selectable[next].clone();
        if self.rows.iter().any(|r| r.id == id && r.more) {
            self.expanded = true;
        }
        self.select_agent(&id, window, cx);
    }

    /// The model source for the currently selected agent. Never probes.
    fn selected_model_source(&self, cx: &Context<Self>) -> ModelSource {
        let Some(id) = self.selected_agent.as_deref() else {
            return ModelSource::Deferred;
        };
        let cached = cx
            .try_global::<crate::catalog_cache::CatalogCache>()
            .and_then(|cache| cache.get(id))
            .map(|catalog| (catalog.models, catalog.default_model));
        let codex_static = self
            .static_models
            .get("codex")
            .copied()
            .unwrap_or(&[]);
        agent_step::resolve_model_source(id, cached, codex_static)
    }

    pub(self) fn selected_model_source_is_rich(&self, cx: &Context<Self>) -> bool {
        matches!(self.selected_model_source(cx), ModelSource::Rich { .. })
    }

    /// Build the model select entity once (needs a `Window`) and keep the
    /// Confirm subscription alive. Programmatic seeding never emits Confirm,
    /// so the handler only fires on real user picks.
    fn ensure_model_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_select.is_some() {
            return;
        }
        let select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::<OnboardModelItem>::new()), None, window, cx)
                .searchable(true)
        });
        self._model_select_sub = Some(cx.subscribe(
            &select,
            |this, _state, ev: &SelectEvent<SearchableVec<OnboardModelItem>>, cx| {
                if let SelectEvent::Confirm(Some(wire)) = ev
                    && let Some(id) = this.selected_agent.clone()
                {
                    this.chosen_models.insert(id, wire.clone());
                    cx.notify();
                }
            },
        ));
        self.model_select = Some(select);
    }

    /// Re-seed the select's items + selection for the current agent. The
    /// signature guard makes this a no-op when nothing changed, so calling it
    /// from render never resets an open dropdown.
    pub(self) fn sync_model_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(select) = self.model_select.clone() else {
            return;
        };
        let (choices, default_model) = match self.selected_model_source(cx) {
            ModelSource::Rich { choices, default_model } => (choices, default_model),
            ModelSource::Deferred => (Vec::new(), None),
        };
        let sig: Vec<(String, String, Option<String>)> = choices
            .iter()
            .map(|m| (m.wire.clone(), m.label.clone(), m.description.clone()))
            .collect();
        if self.model_select_sig != sig {
            self.model_select_sig = sig;
            let items: Vec<OnboardModelItem> = choices
                .iter()
                .map(|m| OnboardModelItem {
                    wire: m.wire.clone(),
                    label: m.label.clone(),
                    description: m.description.clone(),
                })
                .collect();
            select.update(cx, |s, cx| s.set_items(SearchableVec::new(items), window, cx));
        }
        let current = self
            .selected_agent
            .as_deref()
            .and_then(|id| self.chosen_models.get(id).cloned())
            .or(default_model);
        if let Some(wire) = current {
            select.update(cx, |s, cx| s.set_selected_value(&wire, window, cx));
        }
    }

    /// Whether the Driver step is part of this run — a machine whose driver is
    /// already installed and verified never sees it. Frozen at open (dots,
    /// navigation, and button labels must all agree for the whole run, even
    /// after an install flips the live status to Ready mid-step).
    pub(super) fn driver_step_needed(&self) -> bool {
        self.driver_step_planned
    }

    /// Unreachable off macOS — `driver_step_planned` is never set there, so the
    /// wizard never navigates to `OnboardingStep::Driver`. The body exists only
    /// because the step enum and its match arm are platform-neutral; keeping
    /// the variant costs one empty div and keeps step navigation in one shape.
    #[cfg(not(target_os = "macos"))]
    pub(super) fn render_driver_step(&mut self, _cx: &mut Context<Self>) -> gpui::Div {
        gpui::div()
    }

    pub(super) fn next(&mut self, cx: &mut Context<Self>) {
        match self.step {
            OnboardingStep::Agent => {
                self.step = OnboardingStep::ChatView;
                cx.notify();
            }
            OnboardingStep::ChatView if self.driver_step_needed() => {
                self.step = OnboardingStep::Driver;
                cx.notify();
            }
            OnboardingStep::ChatView | OnboardingStep::Driver => self.finish(cx),
        }
    }

    pub(super) fn back(&mut self, cx: &mut Context<Self>) {
        match self.step {
            OnboardingStep::Driver => {
                self.step = OnboardingStep::ChatView;
                cx.notify();
            }
            OnboardingStep::ChatView => {
                self.step = OnboardingStep::Agent;
                cx.notify();
            }
            OnboardingStep::Agent => {}
        }
    }

    /// Skip: mark onboarding done WITHOUT touching any settings — the
    /// `default_chat_agent()` fallback chain covers a user who never chose.
    pub(super) fn skip(&mut self, cx: &mut Context<Self>) {
        self.close_completed(cx);
    }

    /// Finish: persist the user's choices in ONE `agent_launch.toml` write
    /// (the file watcher reloads + swaps the global — we never set the global
    /// directly, same contract as the settings modal), then mark onboarding
    /// done. A failed save keeps the wizard open with a toast rather than
    /// closing with the choices silently dropped.
    pub(super) fn finish(&mut self, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected_agent.clone() {
            let mut launch = cx.global::<AgentLaunchSettings>().clone();
            let model = self.chosen_models.get(&selected).cloned();
            apply_finish(&mut launch, &selected, model.as_deref(), self.open_mode);
            if let Err(err) = crate::agent_launch_settings::save(&launch) {
                tracing::warn!(%err, "onboarding: failed to write agent_launch.toml");
                crate::shell::toast::toast(
                    cx,
                    crate::shell::toast::ToastKind::Error,
                    "Could not save onboarding choices — check disk space and try again",
                );
                return;
            }
        }
        self.close_completed(cx);
    }

    /// Shared close path: write the completion flag (non-fatal on error — a
    /// failed write only means the wizard shows again next launch), hide, and
    /// tell the host to take focus back.
    fn close_completed(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = self.settings_repo.set(COMPLETED_SETTING, "1") {
            tracing::warn!(?err, "failed to persist onboarding completion flag");
        }
        self.open = false;
        cx.emit(OnboardingEvent::Closed);
        cx.notify();
    }
}

impl EventEmitter<OnboardingEvent> for OnboardingWizard {}

/// The Finish write, as a pure mutation so the field matrix unit-tests
/// without GPUI: set the default agent and open mode; write a model only when
/// the user explicitly picked one (an untouched dropdown must not pin today's
/// default and silently hold the agent back from future defaults).
fn apply_finish(
    launch: &mut AgentLaunchSettings,
    selected_agent: &str,
    model: Option<&str>,
    open_mode: OpenMode,
) {
    launch.default_agent = selected_agent.to_string();
    if let Some(wire) = model {
        // entry_mut seeds a preset's ACP wiring when it mints a fresh entry,
        // so holding the model here can't flip Cursor/Amp/OpenCode
        // terminal-only (see finish_model_pick_keeps_preset_agent_chat_capable).
        launch.entry_mut(selected_agent).model = wire.to_string();
    }
    launch.default_open_mode = open_mode;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_matrix() {
        // Fresh install, never onboarded → show.
        assert!(should_show_onboarding(true, false));
        // Fresh-looking manifest but flag present (e.g. wizard finished, then
        // user quit before any window persisted) → never show again.
        assert!(!should_show_onboarding(true, true));
        // Existing install without the flag (pre-onboarding upgrade) → the
        // caller backfills; the wizard stays hidden.
        assert!(!should_show_onboarding(false, false));
        // Existing install, flag present → hidden.
        assert!(!should_show_onboarding(false, true));
    }

    #[test]
    fn pending_mailbox_is_one_shot() {
        assert!(!take_pending());
        set_pending();
        assert!(take_pending());
        assert!(!take_pending());
    }

    #[test]
    fn finish_write_matrix() {
        // Full pick: agent + model + chat mode.
        let mut launch = AgentLaunchSettings::default();
        apply_finish(&mut launch, "claude-code", Some("sonnet"), OpenMode::Chat);
        assert_eq!(launch.default_agent, "claude-code");
        assert_eq!(launch.model_for("claude-code").as_deref(), Some("sonnet"));
        assert_eq!(launch.default_open_mode, OpenMode::Chat);

        // No explicit model pick → no model written, no entry minted.
        let mut launch = AgentLaunchSettings::default();
        apply_finish(&mut launch, "codex", None, OpenMode::Terminal);
        assert_eq!(launch.default_agent, "codex");
        assert_eq!(launch.model_for("codex"), None);
        assert!(launch.for_agent("codex").is_none());
        assert_eq!(launch.default_open_mode, OpenMode::Terminal);

        // A model pick must not clobber the agent's other configured fields.
        let mut launch = AgentLaunchSettings::default();
        launch.entry_mut("codex").args = "--flag".to_string();
        apply_finish(&mut launch, "codex", Some("gpt-5-codex"), OpenMode::Chat);
        let entry = launch.for_agent("codex").unwrap();
        assert_eq!(entry.args, "--flag");
        assert_eq!(entry.model, "gpt-5-codex");
    }

    #[test]
    fn finish_model_pick_keeps_preset_agent_chat_capable() {
        // Regression: entry_mut on a preset id used to mint a bare default
        // (StreamJson, no ACP command) entry, flipping Cursor/Amp/OpenCode
        // terminal-only the moment a model was picked in onboarding. The guard
        // now lives in AgentLaunchSettings::entry_mut; this test pins the
        // Finish path end to end.
        for id in ["opencode", "cursor", "amp"] {
            let mut launch = AgentLaunchSettings::default();
            assert!(launch.chat_capable(id), "{id} chat-capable via preset fallback");
            apply_finish(&mut launch, id, Some("some-model"), OpenMode::Chat);
            assert!(launch.chat_capable(id), "{id} must stay chat-capable after model pick");
            assert!(launch.opens_as_chat(id), "{id} must still open as chat");
            assert_eq!(launch.model_for(id).as_deref(), Some("some-model"));
        }
        // A user-configured entry is NOT overwritten with preset wiring.
        let mut launch = AgentLaunchSettings::default();
        launch.entry_mut("opencode").acp_command = "my-custom-opencode".to_string();
        launch.entry_mut("opencode").transport = oximux_settings::Transport::Acp;
        apply_finish(&mut launch, "opencode", Some("m"), OpenMode::Chat);
        assert_eq!(launch.for_agent("opencode").unwrap().acp_command, "my-custom-opencode");
    }
}
