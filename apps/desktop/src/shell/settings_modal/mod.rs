//! Settings modal — minimal panes wiring settings that already round-trip
//! to disk (terminal + AI commit-message), plus read-only reference panes
//! (keybindings, appearance, about). Opened via the left-rail cog or
//! `Cmd+,`. Mirrors the `project_picker` modal pattern: open/close +
//! click-outside dismiss + `track_focus` for keyboard handling.
//!
//! Editable panes apply immediately — each control mutates a working copy
//! and writes the TOML; the existing file watcher reloads + repaints. The
//! modal never sets the global itself (that would race the debouncer).
//! Rendering lives in [`view`]; this file owns state + persistence.

pub(crate) mod controls;
mod layout;
mod nav;
mod segmented;
mod pane_about;
mod pane_agents;
#[cfg(any(target_os = "macos", windows))]
mod pane_computer_use;
/// The Windows half of the same pane. Separate module rather than a fork of
/// `pane_computer_use` because almost none of that pane transfers: there is no
/// signature to report, no in-app installer to drive, and the master switch and
/// project list gate driver *tools*, which Windows does not yet declare. What is
/// left is a decision macOS never has to make.
#[cfg(windows)]
mod pane_driver_trust;
mod pane_agents_launch;
mod pane_integrations;
mod pane_keybindings;
mod pane_notifications;
mod pairing_qr;
mod pane_remote;
mod pane_schedules;
mod pane_terminal;
mod pane_voice;
mod view;

/// The driver status type is shared with the onboarding wizard's driver step —
/// one resolve/labels implementation, two surfaces.
#[cfg(target_os = "macos")]
pub(crate) use pane_computer_use::DriverStatus;

pub use nav::SettingsPane;

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Point,
    Subscription, Window, point, px,
};
use gpui_component::input::{InputEvent, InputState};
use oximux_settings::{
    AgentLaunchSettings, CommitMessageAiSettings, ComputerUseSettings, Density, DictationSettings,
    TerminalSettings, Theme, Typography,
};
use oximux_storage::SettingsRepo;

use crate::notifier::{AgentNotifySettings, Notifier};

/// Emitted when the modal closes (any path: ×, Esc, click-outside, toggle).
/// `WorkspaceRoot` listens and returns keyboard focus to itself so global
/// key bindings keep dispatching.
pub enum SettingsModalEvent {
    Closed,
}

/// Modal card dimensions.
const CARD_WIDTH: f32 = 960.0;
const CARD_HEIGHT: f32 = 640.0;
/// Vertical offset from the top of the viewport (matches project_picker).
const MODAL_TOP_OFFSET: f32 = 64.0;

pub struct SettingsModal {
    pub(super) open: bool,
    pub(super) selected: SettingsPane,
    pub(super) focus_handle: FocusHandle,
    pub(super) theme: Theme,
    pub(super) density: Density,
    pub(super) typography: Typography,
    /// Working copy of the terminal settings; reseeded from the live global
    /// at each `open()`. Edits mutate this, then write `terminal.toml`.
    pub(crate) terminal: TerminalSettings,
    /// Working copy of the AI commit-message settings; same contract.
    pub(crate) ai: CommitMessageAiSettings,
    /// Working copy of the per-agent launch defaults; reseeded from the live
    /// global at each `open()`. Edits mutate this, then write
    /// `agent_launch.toml`; the watcher reloads + swaps the global.
    pub(crate) agent_launch: AgentLaunchSettings,
    /// Working copy of the voice-dictation settings; reseeded from the live
    /// global at each `open()`. Edits mutate this, then write `dictation.toml`.
    pub(crate) dictation: DictationSettings,
    /// Live notification prefs shared with the notifier. The Agents pane
    /// toggles flip these atomics directly (interior mutability) so the
    /// change takes effect on the next dispatch without a reload.
    pub(crate) notify: Arc<AgentNotifySettings>,
    /// Rendered pairing QR, cached by the deep link it encodes. `RefCell` because
    /// the pane renders from `&self`. Without this the PNG would be re-encoded on
    /// every repaint AND a fresh `Arc<Image>` each frame would defeat gpui's
    /// texture cache, re-uploading the bitmap continuously.
    pub(super) qr_cache: std::cell::RefCell<Option<(String, Arc<gpui::Image>)>>,
    /// The Remote pane's pairing sub-view, when the user has opened it. `None` is
    /// the pane's normal state — a live pairing code exists only while this is
    /// `Some`, which is what makes opening the view the act that mints one.
    pub(super) remote_pairing: Option<pane_remote::PairingState>,
    /// Which pairing window is current. Bumped every time one opens or closes, so
    /// a watcher spawned for an earlier window recognises itself as superseded
    /// and exits instead of acting.
    ///
    /// Needed because a watcher parks on `recv()` for a pairing that its own
    /// window may never see — closing the view does not wake it. Without this,
    /// every window opened during a session left a listener alive, and the first
    /// device to actually pair woke all of them at once: one toast per window
    /// ever opened.
    pub(super) remote_pairing_epoch: u64,
    /// Whether the watcher that reports devices leaving is running. One per modal,
    /// not per pairing window: a device un-enrols on its own schedule, with no
    /// code on screen and often no Remote pane open.
    ///
    /// Set only once the subscription actually succeeds, so opening settings
    /// before remote access is on doesn't consume the single attempt — the next
    /// open tries again, by which time the host may have bound.
    pub(super) remote_unpair_watch: bool,
    /// Working copy of the screen-control settings, reseeded from the global at
    /// each `open()` and written straight back on every edit.
    pub(super) computer_use: ComputerUseSettings,
    /// Result of the last driver check. Held rather than recomputed per frame:
    /// verification spawns `codesign`, and the pane repaints constantly.
    #[cfg(target_os = "macos")]
    pub(super) driver_status: pane_computer_use::DriverStatus,
    /// Windows' equivalent: where the installed driver stands with the user.
    /// Held rather than recomputed per frame for the same reason — resolving it
    /// hashes the binary, and once approved also runs `--version`.
    #[cfg(windows)]
    pub(super) driver_trust: pane_driver_trust::TrustState,
    /// The in-flight driver install this modal started, if any. `None` while
    /// idle — and also while merely *observing* an install another surface
    /// owns (that state lives in `driver_install_ui`, fed by the backend's
    /// pull-style status).
    ///
    /// Not platform-gated: the installer runs on every platform the desktop app
    /// ships on. What differs is the row that renders it and the gate it waits
    /// on — see `oximux_computer_use::install::platform`.
    pub(super) driver_install: Option<crate::shell::driver_install::InstallHandle>,
    /// What the Driver row renders for the install affordance.
    pub(super) driver_install_ui: crate::shell::driver_install::DriverInstallUi,
    /// Guards against stacking poll timers across repaints/reopens.
    pub(super) driver_poll_running: bool,
    /// Whether a background driver re-check is in flight. Guards against
    /// stacking `codesign` sweeps when the modal is reopened faster than one
    /// resolve takes — the same shape as `agent_detect_running`.
    #[cfg(any(target_os = "macos", windows))]
    pub(super) driver_status_running: bool,
    /// Set when the install replaced an existing driver — gates the one-line
    /// "old version until the daemon respawns" note.
    pub(super) driver_upgraded: bool,
    /// Flat KV store the notification toggles persist into (keys in
    /// [`crate::notifier::keys`]), so prefs survive a restart.
    pub(crate) notify_repo: SettingsRepo,
    /// Live dispatch sink for the test-notification button (and the
    /// availability hint next to it).
    pub(crate) notifier: Arc<dyn Notifier>,
    /// Top-left of the card. `None` until the user drags (or the first frame
    /// resolves it), at which point it holds the live, viewport-clamped
    /// position. Reset to `None` on each `open()` so the modal re-centers.
    pub(super) pos: Option<Point<Pixels>>,
    /// Cursor offset within the title bar captured on drag start, so the grab
    /// point stays under the cursor instead of snapping the card to a corner.
    pub(super) drag_grab: Option<Point<Pixels>>,
    /// Live filter input shown at the top of the nav. Lazily built on each
    /// `open()` (needs a `Window`); panes read its value to hide rows that
    /// don't match. `None` before the first open.
    pub(super) search_input: Option<Entity<InputState>>,
    /// Keeps the `InputEvent::Change` → repaint subscription alive.
    _search_sub: Option<Subscription>,
    /// Voice pane's custom-words editor input. Lazily built on `open()` (needs a
    /// `Window`), seeded from `dictation.custom_words`. `None` before first open.
    pub(super) custom_words_input: Option<Entity<InputState>>,
    /// Keeps the custom-words `InputEvent::Change` → parse+persist subscription
    /// alive.
    _custom_words_sub: Option<Subscription>,
    /// The custom-words value at `open()`, so `close()` can flush a pending edit
    /// that was typed but never committed via blur/Enter (clicking dead space
    /// doesn't blur a gpui input) — otherwise closing the modal would silently
    /// drop the typed dictionary.
    custom_words_seed: Vec<String>,
    /// Agents pane, environment editor: which adapter is being edited, and
    /// which of its named launch profiles. `None` profile = the adapter's plain
    /// entry (`default`). Reset on each `open()` so the editor never reopens
    /// pointing at a profile the user deleted from another surface.
    /// A catalog id, not a `&'static str`: a hand-configured ACP agent's id
    /// comes out of `agent_launch.toml` at runtime. Empty means the catalog
    /// resolved to nothing and there is no agent to edit.
    pub(super) env_agent: String,
    pub(super) env_profile: Option<String>,
    /// The `KEY=value` editor for `(env_agent, env_profile)`. Lazily built on
    /// `open()` and rebuilt whenever the selection changes — an `InputState`'s
    /// text is owned by the state, so re-seeding it is a rebuild, not a set.
    pub(super) env_input: Option<Entity<InputState>>,
    _env_sub: Option<Subscription>,
    /// The env map the editor was seeded with, so `close()` can flush an edit
    /// typed but never committed via blur/Enter — same hazard, and same fix, as
    /// [`Self::custom_words_seed`].
    env_seed: BTreeMap<String, String>,
    /// The profile list's single name field, shared by add / rename /
    /// duplicate — see [`pane_agents_launch::ProfileNameMode`]. Enter commits
    /// whichever the mode names. `None` mode hides the field entirely.
    pub(super) profile_name_input: Option<Entity<InputState>>,
    pub(super) profile_name_mode: Option<pane_agents_launch::ProfileNameMode>,
    _profile_name_sub: Option<Subscription>,
    /// Detected agent availability for the Agents pane, carrying the same
    /// tri-state the launcher's picker models: `None` until the first detection
    /// answers, `Some(vec![])` when it timed out, `Some(list)` otherwise.
    /// `agent_detect_running` drives the in-flight label.
    pub(super) agent_detect: Option<Vec<oximux_agents::registry::RegistryEntry>>,
    /// PATH availability of each `ACP_PRESETS` entry, positionally parallel to
    /// it — the launcher's convention, detected under the same timeout.
    pub(super) preset_detect: Option<Vec<bool>>,
    pub(super) agent_detect_running: bool,
    /// Whether the environment editor is showing its values rather than a
    /// masked stand-in. Off on every open and on every selection change: a
    /// reveal is an act about one profile's values, and carrying it forward
    /// would un-mask the next one without being asked.
    pub(super) env_revealed: bool,
    /// The profile whose Delete has been pressed once. The second press
    /// removes it. View state, so an armed delete never survives a reopen.
    pub(super) pending_profile_delete: Option<String>,
    /// The environment card's transient acknowledgment or refusal — see
    /// [`pane_agents_launch::Notice`]. Cleared on the next keystroke in either
    /// field and on every selection change, so the card never answers a
    /// question the user has moved on from. View state only: never persisted,
    /// and dropped with the modal.
    pub(super) env_notice: Option<pane_agents_launch::Notice>,
    /// Working copy of the `keybindings.toml` override map; reseeded from
    /// disk at each `open()`. Edits persist + apply to the live keymap
    /// immediately (see `pane_keybindings`).
    pub(crate) keybind_overrides: BTreeMap<String, String>,
    /// Action id currently capturing a new chord, if any. While set, a
    /// keystroke interceptor swallows every key press app-wide so a bound
    /// chord can be recorded without dispatching its action.
    pub(crate) recording_action: Option<&'static str>,
    /// The interceptor subscription — alive exactly while recording.
    pub(super) recording_sub: Option<Subscription>,
    /// Shared schedule store — the same connection the scheduler ticker reads,
    /// so a schedule created or removed here is visible to it within one tick.
    pub(super) schedule_store: oximux_agents::schedule::ScheduleStore,
    /// Schedules + their recent run history, reloaded from the store at each
    /// `open()` and after every create/delete/toggle so the pane never reads
    /// SQLite mid-paint.
    pub(super) schedule_rows: Vec<pane_schedules::ScheduleRow>,
    /// The in-progress "new schedule" recurrence choice; the text fields live in
    /// the three inputs below. Reset to defaults on each `open()`.
    pub(super) schedule_draft: pane_schedules::ScheduleDraft,
    /// Create-form text inputs, lazily built on `open()` (they need a `Window`).
    /// `None` before first open.
    pub(super) sched_name_input: Option<Entity<InputState>>,
    pub(super) sched_cwd_input: Option<Entity<InputState>>,
    pub(super) sched_prompt_input: Option<Entity<InputState>>,
    /// Validation message shown under the create form, or `None` when clean.
    pub(super) schedule_form_error: Option<String>,

    // ----- Integrations -----
    /// One row per catalogued external CLI. Seeded `Checking` and re-probed on
    /// every modal open, for the same reason `driver_status` is: "not
    /// installed" is the answer most likely to have gone stale, because seeing
    /// it is what sends the user off to install the thing.
    pub(super) integrations: Vec<crate::shell::integrations::IntegrationRow>,
    /// Per-row install state, keyed by index into `integrations`. Absent means
    /// idle — the common case, so it is the one that costs nothing to store.
    pub(super) integration_install:
        std::collections::HashMap<usize, crate::shell::integrations::install::InstallUi>,
    /// Live install handles. Kept apart from the UI state because a handle
    /// cannot be cloned and the render only ever needs the state.
    pub(super) integration_handles:
        std::collections::HashMap<usize, crate::shell::integrations::install::InstallHandle>,
    /// Row whose command was last copied, for the inline acknowledgement.
    pub(super) integration_copied: Option<usize>,
    /// Guards against stacking one poll loop per install click.
    pub(super) integration_poll_running: bool,
}

/// Parse the custom-words editor field into a de-duplicated dictionary. Splits
/// on commas and newlines only (NOT spaces) so a multi-word entry like
/// "New York" can be one dictionary unit — the matcher collapses spaces when
/// scoring. Trims, drops blanks, keeps first-seen order (case-insensitive
/// dedup). `sanitized()` re-applies the same cleanup on load.
pub(super) fn parse_custom_words(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .filter(|w| seen.insert(w.to_lowercase()))
        .map(str::to_string)
        .collect()
}

impl SettingsModal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        notify: Arc<AgentNotifySettings>,
        notify_repo: SettingsRepo,
        notifier: Arc<dyn Notifier>,
        schedule_store: oximux_agents::schedule::ScheduleStore,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            open: false,
            selected: SettingsPane::Terminal,
            qr_cache: std::cell::RefCell::new(None),
            remote_pairing: None,
            remote_pairing_epoch: 0,
            remote_unpair_watch: false,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            terminal: TerminalSettings::default(),
            ai: CommitMessageAiSettings::default(),
            agent_launch: AgentLaunchSettings::default(),
            dictation: DictationSettings::default(),
            computer_use: ComputerUseSettings::default(),
            #[cfg(target_os = "macos")]
            driver_status: pane_computer_use::DriverStatus::Unknown,
            #[cfg(windows)]
            driver_trust: pane_driver_trust::TrustState::Unknown,
            driver_install: None,
            driver_install_ui: crate::shell::driver_install::DriverInstallUi::Idle,
            driver_poll_running: false,
            #[cfg(any(target_os = "macos", windows))]
            driver_status_running: false,
            integrations: Vec::new(),
            integration_install: std::collections::HashMap::new(),
            integration_handles: std::collections::HashMap::new(),
            integration_copied: None,
            integration_poll_running: false,
            driver_upgraded: false,
            notify,
            notify_repo,
            notifier,
            pos: None,
            drag_grab: None,
            search_input: None,
            _search_sub: None,
            custom_words_input: None,
            _custom_words_sub: None,
            custom_words_seed: Vec::new(),
            env_agent: String::new(),
            env_profile: None,
            env_input: None,
            _env_sub: None,
            env_seed: BTreeMap::new(),
            profile_name_input: None,
            profile_name_mode: None,
            _profile_name_sub: None,
            pending_profile_delete: None,
            env_revealed: false,
            agent_detect: None,
            preset_detect: None,
            agent_detect_running: false,
            env_notice: None,
            keybind_overrides: BTreeMap::new(),
            recording_action: None,
            recording_sub: None,
            schedule_store,
            schedule_rows: Vec::new(),
            schedule_draft: pane_schedules::ScheduleDraft::default(),
            sched_name_input: None,
            sched_cwd_input: None,
            sched_prompt_input: None,
            schedule_form_error: None,
        }
    }

    /// Current filter text (empty when no input exists or it's blank).
    pub(super) fn search_text(&self, cx: &App) -> String {
        self.search_input
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the modal, seeding the working copies from the live globals so
    /// the panes reflect what is currently on disk and applied.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal = cx
            .try_global::<TerminalSettings>()
            .cloned()
            .unwrap_or_default();
        self.ai = cx
            .try_global::<CommitMessageAiSettings>()
            .cloned()
            .unwrap_or_default();
        self.agent_launch = cx
            .try_global::<AgentLaunchSettings>()
            .cloned()
            .unwrap_or_default();
        self.dictation = cx
            .try_global::<DictationSettings>()
            .cloned()
            .unwrap_or_default();
        self.computer_use = cx
            .try_global::<ComputerUseSettings>()
            .cloned()
            .unwrap_or_default();
        // Same reasoning as the driver resolve below, for the four CLIs the
        // Integrations pane reports on. Off the UI thread: this is a PATH walk
        // plus a couple of short spawns per tool.
        self.refresh_integrations(cx);
        // Re-checked per open rather than once at boot: the user may have
        // installed or updated the driver since, and "not installed" is the
        // status most likely to be stale. Off the UI thread, for the reason
        // spelled out on `refresh_driver_status`.
        #[cfg(any(target_os = "macos", windows))]
        self.refresh_driver_status(cx);
        // A modal reopened mid-install shows live progress: attach to the
        // backend's pull-style status (cheap, unlike a resolve per tick) and
        // restart the poll loop. A stale failure from a previous open is
        // cleared — the fresh resolve above is the truth now.
        if !self.driver_install_ui.is_running() {
            self.driver_install_ui = match oximux_computer_use::install::status() {
                Some(stage) => crate::shell::driver_install::DriverInstallUi::Running { stage },
                None => crate::shell::driver_install::DriverInstallUi::Idle,
            };
            // The post-upgrade note belongs to the session that upgraded; a
            // fresh open starts from the resolved truth alone.
            self.driver_upgraded = false;
        }
        self.spawn_driver_install_poll(cx);
        // Start reporting devices that drop themselves, if the host is up. Not in
        // the constructor: the remote host binds later than the shell is built, so
        // subscribing there would find nothing to subscribe to.
        pane_remote::watch_for_unpair(self, cx);
        // Reseed the keybinding overrides from disk (hand edits since boot
        // show up here; load problems were already toasted at boot).
        self.keybind_overrides = crate::keybindings_settings::load_overrides().0;
        self.recording_action = None;
        self.recording_sub = None;
        // Build a fresh (empty) search input each open, and repaint the panes
        // whenever its text changes so the filter is live.
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings"));
        self._search_sub = Some(cx.subscribe(&input, |_this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        }));
        let input_focus = input.read(cx).focus_handle(cx);
        self.search_input = Some(input);

        // Voice pane's custom-words editor: a comma/space separated field seeded
        // from the working copy. On every edit, reparse into `custom_words` and
        // persist (the watcher reloads the global) so the dictionary applies to
        // the next dictation without a modal round-trip.
        self.custom_words_seed = self.dictation.custom_words.clone();
        let seed = self.dictation.custom_words.join(", ");
        let cw_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. OxiMux, ChargeBee, ChatGPT")
                .default_value(seed)
        });
        // Update the in-memory working copy on every keystroke, but only PERSIST
        // (disk write + watcher reload) when the field is committed — on blur or
        // Enter — so typing a dictionary entry isn't a per-character fs::write on
        // the main thread. This matches the modal's persist-on-discrete-action
        // convention (the search field, three lines up, likewise does no I/O).
        self._custom_words_sub =
            Some(
                cx.subscribe(&cw_input, |this, input, ev: &InputEvent, cx| match ev {
                    InputEvent::Change => {
                        let raw = input.read(cx).value().to_string();
                        this.dictation.custom_words = parse_custom_words(&raw);
                    }
                    InputEvent::Blur | InputEvent::PressEnter { .. } => {
                        this.persist_voice(cx);
                    }
                    _ => {}
                }),
            );
        self.custom_words_input = Some(cw_input);

        // Agents pane's environment editor. Reset the selection first: a reopen
        // must not land on a profile deleted since, which would silently edit
        // the default entry under the deleted profile's label.
        // Seeded from the resolved catalog rather than from a hard-coded first
        // element: the set is dynamic now, and an empty one has to select
        // nothing and render an empty state instead of indexing into it.
        self.env_agent = self.agent_catalog().first().map(|a| a.id.to_string()).unwrap_or_default();
        self.env_profile = None;
        self.env_notice = None;
        self.env_revealed = false;
        self.env_seed = self.selected_env();
        let placeholder = pane_agents_launch::env_placeholder(&self.env_agent);
        self.detect_agents(window, cx);
        let env_input = cx.new(|cx| {
            InputState::new(window, cx)
                // `auto_grow`, not `multi_line(true).rows(4)`: `rows()` only
                // drives the field's height in auto-grow mode. For a plain
                // multi-line input the element hard-codes `min_size.height` to
                // ONE line and sizes the rest from its parent, so the field
                // collapsed to a single row and clipped everything below it —
                // including lines 2 and 3 of the worked example below.
                .auto_grow(4, 10)
                .placeholder(placeholder)
                .default_value(pane_agents_launch::format_env_lines(&self.env_seed))
        });
        // Same split as the custom-words field: sync the working copy on every
        // keystroke, but only WRITE the file on a discrete commit (blur/Enter),
        // so typing an endpoint isn't a per-character `fs::write` on the main
        // thread.
        self._env_sub = Some(cx.subscribe(
            &env_input,
            |this, input, ev: &InputEvent, cx| match ev {
                InputEvent::Change => {
                    let raw = input.read(cx).value().to_string();
                    // An oversized draft is not synced at all, so it cannot
                    // reach the file through the close-flush either. The
                    // working copy keeps its last good value until the draft
                    // comes back under the cap.
                    if raw.len() <= pane_agents_launch::MAX_ENV_DRAFT {
                        let agent = this.env_agent.clone();
                        let profile = this.env_profile.clone();
                        this.agent_launch.profile_entry_mut(&agent, profile.as_deref()).env =
                            pane_agents_launch::parse_env_lines(&raw);
                    }
                    // Typing is not a commit: the previous answer is now stale,
                    // so retire it rather than let it hang over new input.
                    // Diagnostics deliberately do NOT run here — this fires on
                    // every keystroke, and it would flag `ANTHROPIC_BASE_UR`
                    // as malformed while it is still being typed.
                    this.env_notice = None;
                }
                // Blur is the write, so it is also where the draft is judged.
                InputEvent::Blur => {
                    let raw = input.read(cx).value().to_string();
                    this.commit_env_draft(&raw, cx);
                }
                _ => {}
            },
        ));
        self.env_input = Some(env_input);

        // The profile list's shared name field. Hidden until an affordance sets
        // a mode; the subscription needs the `Window` to re-seed the env editor
        // after a commit, hence `subscribe_in`.
        self.profile_name_mode = None;
        self.pending_profile_delete = None;
        let np_input = cx.new(|cx| InputState::new(window, cx).placeholder("Profile name"));
        self._profile_name_sub = Some(cx.subscribe_in(
            &np_input,
            window,
            |this, input, ev: &InputEvent, window, cx| {
                use pane_agents_launch::{Notice, NoticeSlot, ProfileNameMode};
                // Typing retires the previous answer; only Enter produces one.
                if matches!(ev, InputEvent::Change) {
                    this.env_notice = None;
                    return;
                }
                if !matches!(ev, InputEvent::PressEnter { .. }) {
                    return;
                }
                // No mode means no field on screen; nothing to commit.
                let Some(mode) = this.profile_name_mode.clone() else {
                    return;
                };
                let agent = this.env_agent.clone();
                let raw = input.read(cx).value().to_string();
                // Each of blank / `default` / already-taken used to be a silent
                // early return, which read as a broken button. All three now
                // say which one it was, and none of them changes anything.
                let name = match pane_agents_launch::validate_profile_name(
                    &raw,
                    &this.agent_launch.profile_names(&agent),
                ) {
                    Ok(name) => name,
                    Err(msg) => {
                        this.env_notice = Some(Notice::err(NoticeSlot::Profile, msg));
                        cx.notify();
                        return;
                    }
                };
                // The settings-crate call can still refuse — the source may
                // have gone (a hand edit to the file, a delete in between).
                let gone = |from: &str| {
                    Notice::err(
                        NoticeSlot::Profile,
                        format!("“{from}” is no longer there — reopen the pane and try again."),
                    )
                };
                let ack = match &mode {
                    ProfileNameMode::Add => {
                        this.agent_launch.profile_entry_mut(&agent, Some(&name));
                        format!("Created “{name}” — editing it now.")
                    }
                    ProfileNameMode::Rename(from) => {
                        if !this.agent_launch.rename_profile(&agent, from, &name) {
                            this.env_notice = Some(gone(from));
                            cx.notify();
                            return;
                        }
                        format!("Renamed “{from}” to “{name}”.")
                    }
                    ProfileNameMode::Duplicate(from) => {
                        if !this.agent_launch.duplicate_profile(&agent, from, &name) {
                            this.env_notice = Some(gone(from));
                            cx.notify();
                            return;
                        }
                        format!("Duplicated “{from}” as “{name}” — editing it now.")
                    }
                };
                this.persist_agent_launch(cx);
                this.profile_name_mode = None;
                input.update(cx, |s, cx| s.set_value("", window, cx));
                match &mode {
                    // A rename of the profile being edited keeps editing it:
                    // assigning the selection directly (rather than going
                    // through `select_env_profile`) avoids a flush against a
                    // name that no longer resolves.
                    ProfileNameMode::Rename(from) if this.env_profile.as_deref() == Some(from) => {
                        this.env_profile = Some(name.clone());
                        this.reseed_env_editor(window, cx);
                    }
                    ProfileNameMode::Rename(_) => {}
                    _ => this.select_env_profile(Some(name.clone()), window, cx),
                }
                // Set AFTER any selection change, which clears the slot.
                this.env_notice = Some(Notice::ok(NoticeSlot::Profile, ack));
                cx.notify();
            },
        ));
        self.profile_name_input = Some(np_input);

        // Schedules pane: reload the list + run history from the shared store,
        // and build a fresh empty create form (reset draft, clear any error).
        self.reload_schedules();
        self.schedule_draft = pane_schedules::ScheduleDraft::default();
        self.schedule_form_error = None;
        self.sched_name_input =
            Some(cx.new(|cx| InputState::new(window, cx).placeholder("Nightly test run")));
        self.sched_cwd_input =
            Some(cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/project")));
        self.sched_prompt_input = Some(cx.new(|cx| {
            InputState::new(window, cx).placeholder("Run the test suite and summarize failures")
        }));

        self.open = true;
        self.pos = None;
        self.drag_grab = None;
        // Focus the search field so typing filters immediately. Esc / the per-
        // pane controls still work (key events bubble to the card handler).
        window.focus(&input_focus, cx);
        cx.notify();
    }

    /// Move the card's top-left to `(x, y)`, clamped so it stays within the
    /// viewport. Used by the title-bar drag.
    pub(super) fn set_pos(&mut self, x: f32, y: f32, window: &Window, cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        let max_x = (f32::from(vp.width) - CARD_WIDTH).max(0.0);
        let max_y = (f32::from(vp.height) - CARD_HEIGHT).max(0.0);
        self.pos = Some(point(px(x.clamp(0.0, max_x)), px(y.clamp(0.0, max_y))));
        cx.notify();
    }

    /// The card's effective top-left: the dragged position if any, else
    /// horizontally centered at the standard top offset.
    pub(super) fn resolved_pos(&self, window: &Window) -> Point<Pixels> {
        self.pos.unwrap_or_else(|| {
            let vp = window.viewport_size();
            let x = ((f32::from(vp.width) - CARD_WIDTH) / 2.0).max(0.0);
            point(px(x), px(MODAL_TOP_OFFSET))
        })
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        // Emit Closed only on a real open→closed transition. `close_modal_overlays`
        // closes this modal before opening any overlay; an unconditional emit
        // queues a workspace root-refocus that steals focus from a just-opened
        // modal. See the matching guard in `command_palette::PaletteModal::close`.
        let was_open = self.open;
        self.open = false;
        // Flush a custom-words edit that was typed but never committed (blur/Enter)
        // before dropping the input — the working copy is synced on every
        // keystroke, so persist only when it actually changed since open. Without
        // this, typing a dictionary then closing via ✕/Esc would drop the edit.
        if self.custom_words_input.is_some() && self.dictation.custom_words != self.custom_words_seed
        {
            self.persist_voice(cx);
        }
        // Drop the search input so its focus handle can't keep window focus
        // orphaned after the modal is hidden.
        self.search_input = None;
        self._search_sub = None;
        self.custom_words_input = None;
        self._custom_words_sub = None;
        // Same flush-before-drop hazard as custom words: the working copy is
        // synced on every keystroke, but only blur/Enter writes the file, and
        // clicking dead space doesn't blur a gpui input. Without this, typing an
        // endpoint then closing via ✕/Esc would silently drop it.
        if self.env_input.is_some() && self.selected_env() != self.env_seed {
            self.persist_agent_launch(cx);
        }
        self.env_input = None;
        self._env_sub = None;
        self.profile_name_input = None;
        self._profile_name_sub = None;
        self.profile_name_mode = None;
        self.pending_profile_delete = None;
        self.env_notice = None;
        self.env_revealed = false;
        // Drop the schedule create-form inputs so their focus handles can't keep
        // window focus orphaned after the modal is hidden.
        self.sched_name_input = None;
        self.sched_cwd_input = None;
        self.sched_prompt_input = None;
        // A close mid-recording must release the keystroke interceptor or
        // every subsequent key press in the app would be swallowed.
        self.recording_action = None;
        self.recording_sub = None;
        // Closing the modal is leaving the pairing view: retire the live code with
        // it rather than letting a window the user can no longer see stay
        // redeemable until it times out.
        pane_remote::close_pairing(self, cx);
        if was_open {
            cx.emit(SettingsModalEvent::Closed);
        }
        cx.notify();
    }

    /// Open straight to `pane` — for callers that are already answering a
    /// specific question, like the status-bar update pill.
    pub fn open_to_pane(&mut self, pane: SettingsPane, window: &mut Window, cx: &mut Context<Self>) {
        self.open(window, cx);
        self.selected = pane;
        cx.notify();
    }

    pub(super) fn select_pane(&mut self, pane: SettingsPane, cx: &mut Context<Self>) {
        self.selected = pane;
        cx.notify();
    }

    /// Persist the terminal working copy to `terminal.toml`. The watcher
    /// reloads + repaints; we never set the global here.
    pub(super) fn persist_terminal(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::terminal_settings::save(&self.terminal) {
            tracing::warn!(%err, "settings modal: failed to write terminal.toml");
        }
        cx.notify();
    }

    /// Persist the AI working copy to `commit_message_ai.toml`.
    pub(super) fn persist_ai(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::commit_message_ai_settings::save(&self.ai) {
            tracing::warn!(%err, "settings modal: failed to write commit_message_ai.toml");
        }
        cx.notify();
    }

    /// Persist the per-agent launch working copy to `agent_launch.toml`. The
    /// watcher reloads + swaps the global; we never set the global here.
    /// The live notice, if it belongs under `slot`. The environment card asks
    /// per row, so a message raised under one row cannot render under another.
    pub(super) fn notice_for(
        &self,
        slot: pane_agents_launch::NoticeSlot,
    ) -> Option<&pane_agents_launch::Notice> {
        self.env_notice.as_ref().filter(|n| n.slot == slot)
    }

    /// The agents this pane can configure, in catalog order.
    ///
    /// Reads the same composition the launcher's picker does, so the two
    /// cannot disagree about which agents exist — the disagreement is what let
    /// `Custom` and every ACP agent fall out of the settings pane entirely.
    ///
    /// The registry is rebuilt per call rather than held: it is a list of
    /// stateless adapter objects, the call sites are render-rate at worst, and
    /// threading one through the modal's constructor would put a launch
    /// concern in the settings modal's signature.
    pub(super) fn agent_catalog(&self) -> Vec<crate::shell::agent_ui::agent_catalog::CatalogAgent> {
        use crate::shell::agent_ui::agent_catalog::{AdapterDetection, agent_catalog};
        let registered;
        let adapters = match self.agent_detect.as_deref() {
            Some(entries) => AdapterDetection::Done(entries),
            None => {
                registered =
                    oximux_agents::registry::AdapterRegistry::with_builtin_adapters()
                        .entries_without_detection();
                AdapterDetection::Pending(&registered)
            }
        };
        agent_catalog(adapters, self.preset_detect.as_deref(), &self.agent_launch)
    }

    /// Re-check the driver's health on a background thread.
    ///
    /// Inline, this was by far the most expensive thing `open()` did:
    /// `prepare()` runs two `codesign` subprocesses, executes the driver to
    /// read its version, and SHA-256s the whole ~50 MB binary — measured at
    /// ~206 ms in a release build and ~1.5 s in a debug one, all of it on the
    /// UI thread and all of it *before* `self.open = true`, so the modal could
    /// not paint until it finished. That is the delay between pressing the cog
    /// and seeing the card.
    ///
    /// Nothing gates on the answer at open time — the pane renders it, and
    /// renders `Unknown` as "Checking…" — so the resolve is exactly the shape
    /// `refresh_integrations` already uses two lines above: spawn, await,
    /// assign, notify. The previous verdict stays on screen meanwhile rather
    /// than flickering back to "Checking…", because a reopen's most likely
    /// answer is the one already shown.
    #[cfg(any(target_os = "macos", windows))]
    pub(super) fn refresh_driver_status(&mut self, cx: &mut Context<Self>) {
        if self.driver_status_running {
            return;
        }
        self.driver_status_running = true;
        cx.spawn(async move |weak, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move {
                    #[cfg(target_os = "macos")]
                    {
                        pane_computer_use::DriverStatus::resolve()
                    }
                    #[cfg(windows)]
                    {
                        pane_driver_trust::TrustState::resolve()
                    }
                })
                .await;
            let _ = weak.update(cx, |modal, cx| {
                #[cfg(target_os = "macos")]
                {
                    modal.driver_status = resolved;
                }
                #[cfg(windows)]
                {
                    modal.driver_trust = resolved;
                }
                modal.driver_status_running = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Probe which agent CLIs are actually installed.
    ///
    /// The same call, the same 500 ms budget, and the same tri-state the
    /// launcher's picker uses — a second detector would be a second answer to
    /// a question that already has one. A timeout leaves `Some(vec![])`, which
    /// is what the pane renders as "detection failed" with a retry.
    pub(super) fn detect_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_detect_running {
            return;
        }
        self.agent_detect_running = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // `detect_available` and the timeout below are Tokio-bound. Under a
            // GPUI test context there is no reactor, and calling them there
            // aborts the whole test rather than failing a probe — so ask first
            // and leave the pane in its Pending state, which is what an
            // undetected catalog is supposed to look like anyway.
            if tokio::runtime::Handle::try_current().is_err() {
                let _ = this.update(cx, |m, cx| {
                    m.agent_detect_running = false;
                    cx.notify();
                });
                tracing::debug!("settings: no Tokio runtime; skipping agent detection");
                return;
            }
            let registry = oximux_agents::registry::AdapterRegistry::with_builtin_adapters();
            // Adapters and presets under ONE timeout: both are `which`-style
            // PATH probes, so a slow mount caps them together rather than
            // twice over.
            let detect = async {
                let entries = registry.detect_available().await;
                let mut presets = Vec::with_capacity(oximux_settings::ACP_PRESETS.len());
                for preset in oximux_settings::ACP_PRESETS {
                    presets.push(oximux_agents::cli::which_on_path(preset.command).await);
                }
                (entries, presets)
            };
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(500), detect).await;
            let applied = this.update(cx, |m, cx| {
                match result {
                    Ok((entries, presets)) => {
                        m.agent_detect = Some(entries);
                        m.preset_detect = Some(presets);
                    }
                    Err(_timeout) => {
                        // Empty rather than left unknown: unknown renders
                        // neutral forever, and the user would never learn that
                        // detection is what failed.
                        m.agent_detect.get_or_insert_with(Vec::new);
                        m.preset_detect.get_or_insert_with(|| {
                            vec![false; oximux_settings::ACP_PRESETS.len()]
                        });
                        tracing::warn!(
                            "settings: agent detection timed out after 500ms; \
                             PATH may be on a slow mount"
                        );
                    }
                }
                m.agent_detect_running = false;
                cx.notify();
            });
            if applied.is_err() {
                tracing::debug!("settings: modal dropped before detection completed");
            }
        })
        .detach();
    }

    /// Re-run detection from the pane's Refresh affordance, discarding the
    /// cached answer so the failure state can be retried rather than stuck.
    pub(super) fn refresh_agent_detection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_detect_running {
            return;
        }
        self.agent_detect = None;
        self.preset_detect = None;
        self.detect_agents(window, cx);
    }

    /// The launch entry the environment card is currently editing — the plain
    /// `[agents.<id>]` entry when the selection is `default`, that profile's
    /// own entry otherwise.
    ///
    /// One write path so no control has to remember which entry it is on. Every
    /// flag and model chip in the launch card above calls `entry_mut` and so
    /// always writes the default; the card below has a profile selected, and
    /// writing the default from there is exactly the hole this closes.
    pub(super) fn selected_launch_mut(&mut self) -> &mut oximux_settings::PerAgentLaunch {
        let agent = self.env_agent.clone();
        let profile = self.env_profile.clone();
        self.agent_launch.profile_entry_mut(&agent, profile.as_deref())
    }

    /// The env map of the currently-selected `(agent, profile)`, or empty when
    /// that pair has no entry yet. The comparison basis for the close-flush.
    fn selected_env(&self) -> BTreeMap<String, String> {
        self.agent_launch
            .for_agent_in(&self.env_agent, self.env_profile.as_deref())
            .map(|l| l.env.clone())
            .unwrap_or_default()
    }

    /// Point the environment editor at `profile` of the current agent, flushing
    /// any pending edit to the profile being left first.
    ///
    /// The text belongs to the `InputState`, so switching selection has to push
    /// the new value in rather than re-render from `self` — a settings pane
    /// renders from `&self` with no `Window`, which `set_value` requires.
    pub(super) fn select_env_profile(
        &mut self,
        profile: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_env() != self.env_seed {
            self.persist_agent_launch(cx);
        }
        self.env_profile = profile;
        self.pending_profile_delete = None;
        self.reseed_env_editor(window, cx);
    }

    /// Point the environment editor at `agent`, resetting to its `default`
    /// profile — a profile name is per-adapter, so carrying the current one
    /// across a switch would land on an unrelated (or absent) profile.
    pub(super) fn select_env_agent(
        &mut self,
        agent: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_env() != self.env_seed {
            self.persist_agent_launch(cx);
        }
        self.env_agent = agent;
        self.env_profile = None;
        // Both are per-adapter, so neither survives the switch.
        self.pending_profile_delete = None;
        self.profile_name_mode = None;
        self.reseed_env_editor(window, cx);
    }

    /// Push the selected `(agent, profile)`'s env into the editor and re-baseline
    /// the close-flush comparison.
    fn reseed_env_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.env_seed = self.selected_env();
        // A message about the pair being left would read as being about the
        // pair being arrived at.
        self.env_notice = None;
        self.env_revealed = false;
        let text = pane_agents_launch::format_env_lines(&self.env_seed);
        let placeholder = pane_agents_launch::env_placeholder(&self.env_agent);
        if let Some(input) = self.env_input.clone() {
            input.update(cx, |s, cx| {
                s.set_value(&text, window, cx);
                // The example set is per-agent, so it follows the selection —
                // Anthropic variables in the Codex editor teach the wrong thing.
                s.set_placeholder(placeholder, window, cx);
            });
        }
        cx.notify();
    }

    /// Judge and write the environment draft. The one place a commit happens,
    /// so blur, the reveal toggle, and any future Enter path all say the same
    /// thing about the same text.
    ///
    /// Three outcomes, all of them spoken:
    ///
    /// - Over the size cap: nothing is written and the field says so. A cap
    ///   that truncated would be the same silent-loss failure as the dropped
    ///   line below, one order of magnitude larger.
    /// - Lines that cannot become variables: reported, naming the first and
    ///   counting the rest. Reported even when the map did not change, which is
    ///   the case that matters — a line with no `=` produces no map change, so
    ///   a plain "did anything change?" test calls it a no-op and stays quiet.
    /// - A clean write: acknowledged with the count, as before.
    ///
    /// A blur that changed nothing and had nothing to report stays silent:
    /// clicking through the card blurs this field constantly, and "Saved" on a
    /// no-op is the noise that teaches users to stop reading these lines.
    pub(super) fn commit_env_draft(&mut self, raw: &str, cx: &mut Context<Self>) {
        use pane_agents_launch::{MAX_ENV_DRAFT, Notice, NoticeSlot};

        if raw.len() > MAX_ENV_DRAFT {
            self.env_notice = Some(Notice::err(
                NoticeSlot::Environment,
                format!(
                    "Too large to save — {} characters, limit {MAX_ENV_DRAFT}. \
                     Nothing was written; shorten it and it will save.",
                    raw.len()
                ),
            ));
            cx.notify();
            return;
        }

        // Sync from `raw` here rather than trusting the keystroke handler to
        // have run: the reveal toggle commits from a mouse click on another
        // element, and a commit that silently depended on a prior `Change`
        // would strand the last thing typed.
        let (map, rejects) = pane_agents_launch::parse_env_draft(raw);
        let agent = self.env_agent.clone();
        let profile = self.env_profile.clone();
        self.agent_launch.profile_entry_mut(&agent, profile.as_deref()).env = map;
        let changed = self.selected_env() != self.env_seed;
        if !changed && rejects.is_empty() {
            return;
        }
        if changed {
            self.persist_agent_launch(cx);
            self.env_seed = self.selected_env();
        }
        // Resolution drops reserved keys, so the saved count is the count that
        // will actually reach a launch — not the number of lines typed.
        let applied = self
            .agent_launch
            .env_for(&self.env_agent, self.env_profile.as_deref())
            .len();
        self.env_notice = Some(match pane_agents_launch::reject_message(&rejects) {
            // Part of the draft was refused, so this reads as a refusal even
            // though the rest of it saved.
            Some(msg) => Notice::err(NoticeSlot::Environment, msg),
            None => Notice::ok(
                NoticeSlot::Environment,
                match applied {
                    0 => "Saved — no variables set.".to_string(),
                    1 => "Saved 1 variable.".to_string(),
                    n => format!("Saved {n} variables."),
                },
            ),
        });
        cx.notify();
    }

    /// Show or hide the environment editor's values.
    ///
    /// Hiding commits first: the editor is unmounted while masked, so its blur
    /// — the write — would never fire, and a value typed and then hidden would
    /// be lost.
    pub(super) fn toggle_env_reveal(&mut self, cx: &mut Context<Self>) {
        if self.env_revealed && let Some(input) = self.env_input.clone() {
            let raw = input.read(cx).value().to_string();
            self.commit_env_draft(&raw, cx);
        }
        self.env_revealed = !self.env_revealed;
        cx.notify();
    }

    /// Open the profile list's name field in `mode`, seeded and focused.
    ///
    /// Rename and duplicate also select the profile they act on, so the
    /// confirmation they eventually produce names the profile the card is
    /// visibly pointing at.
    pub(super) fn begin_profile_name(
        &mut self,
        mode: pane_agents_launch::ProfileNameMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use pane_agents_launch::ProfileNameMode;
        self.pending_profile_delete = None;
        self.env_notice = None;
        if let ProfileNameMode::Rename(name) | ProfileNameMode::Duplicate(name) = &mode {
            let name = name.clone();
            let target = (name != oximux_settings::DEFAULT_PROFILE).then_some(name);
            self.select_env_profile(target, window, cx);
        }
        let seed = mode.seed();
        self.profile_name_mode = Some(mode);
        if let Some(input) = self.profile_name_input.clone() {
            input.update(cx, |s, cx| s.set_value(&seed, window, cx));
            // Focus set synchronously inside a mouse-down handler is clobbered
            // by GPUI's post-click focus dispatch — the field would open blurred
            // and the user's first keystroke would go nowhere.
            let handle = input.read(cx).focus_handle(cx);
            window.defer(cx, move |window, app| handle.focus(window, app));
        }
        cx.notify();
    }

    /// Dismiss the name field without committing.
    pub(super) fn cancel_profile_name(&mut self, cx: &mut Context<Self>) {
        self.profile_name_mode = None;
        self.env_notice = None;
        cx.notify();
    }

    /// First press of a profile's Delete: arm it, and select it so the
    /// consequence is read against the profile the card is pointing at.
    pub(super) fn arm_profile_delete(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_env_profile(Some(name.clone()), window, cx);
        self.profile_name_mode = None;
        self.pending_profile_delete = Some(name);
        cx.notify();
    }

    /// Second thoughts.
    pub(super) fn cancel_profile_delete(&mut self, cx: &mut Context<Self>) {
        self.pending_profile_delete = None;
        cx.notify();
    }

    /// Second press: remove `name` and fall back to `default`. A no-op on
    /// `default` itself, which is the adapter's plain entry.
    pub(super) fn confirm_profile_delete(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_profile_delete = None;
        let agent = self.env_agent.clone();
        if !self.agent_launch.remove_profile(&agent, name) {
            cx.notify();
            return;
        }
        if self.env_profile.as_deref() == Some(name) {
            self.env_profile = None;
        }
        // Re-baseline BEFORE persisting so a pending edit to the profile just
        // removed can't be written back under the default entry.
        self.env_seed = self.selected_env();
        self.persist_agent_launch(cx);
        self.reseed_env_editor(window, cx);
        // Set AFTER the reseed, which clears the slot.
        self.env_notice = Some(pane_agents_launch::Notice::ok(
            pane_agents_launch::NoticeSlot::Profile,
            format!("Deleted “{name}”."),
        ));
        cx.notify();
    }

    pub(super) fn persist_agent_launch(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::agent_launch_settings::save(&self.agent_launch) {
            tracing::warn!(%err, "settings modal: failed to write agent_launch.toml");
        }
        cx.notify();
    }

    /// Persist the voice working copy to `dictation.toml`. The watcher reloads +
    /// swaps the global; we never set the global here.
    pub(super) fn persist_voice(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::dictation_settings::save(&self.dictation) {
            tracing::warn!(%err, "settings modal: failed to write dictation.toml");
        }
        cx.notify();
    }

    /// Persist the screen-control working copy to `computer_use.toml`. The
    /// watcher reloads + swaps the global; we never set the global here.
    #[cfg(any(target_os = "macos", windows))]
    pub(super) fn persist_computer_use(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::computer_use_settings::save(&self.computer_use) {
            tracing::warn!(%err, "settings modal: failed to write computer_use.toml");
        }
        cx.notify();
    }

    /// Persist one notification pref to the flat settings store as
    /// `"true"`/`"false"`. The live atomic is flipped by the caller before
    /// this runs; persistence only makes the choice survive a restart.
    /// Persist a boolean pref to the flat settings store. Not notification-specific
    /// despite the repo's name — the remote master switch rides the same table.
    pub(super) fn persist_flag(&mut self, key: &str, value: bool, cx: &mut Context<Self>) {
        let v = if value { "true" } else { "false" };
        if let Err(err) = self.notify_repo.set(key, v) {
            tracing::warn!(%err, key, "settings modal: failed to persist pref");
        }
        cx.notify();
    }
}

impl Focusable for SettingsModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<SettingsModalEvent> for SettingsModal {}

#[cfg(test)]
mod env_editor_tests {
    //! Live-behaviour cover for the Agents pane's environment editor.
    //!
    //! These drive the real GPUI element tree and the real `InputState`, which
    //! is the half plain unit tests cannot reach: a settings pane renders from
    //! `&self` with no `Window`, so every selection change has to push text into
    //! the input from a handler that has one. A regression there is invisible to
    //! logic tests — the map is right and the box shows the previous profile.

    use super::*;
    use crate::notifier::null::NullNotifier;
    use gpui::TestAppContext;
    use oximux_settings::DEFAULT_PROFILE;

    /// A modal mounted the way the real app mounts one: inside a
    /// `gpui_component::Root`. Not a formality — `InputState` reaches for the
    /// window's Root layer, so a window whose root view IS the modal panics in
    /// `Root::update` before any assertion runs.
    ///
    /// Returns both handles because the window is what carries a `Window` into
    /// a closure, and the modal is what the assertions are about.
    fn modal(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<gpui_component::Root>, Entity<SettingsModal>) {
        cx.update(gpui_component::init);
        let db = oximux_storage::open_memory().expect("in-memory db");
        let repo = SettingsRepo::new(db.clone());
        let schedules = oximux_agents::schedule::ScheduleStore::new(db.conn());
        let built: std::cell::RefCell<Option<Entity<SettingsModal>>> =
            std::cell::RefCell::new(None);
        let window = cx.add_window(|window, cx| {
            let m = cx.new(|cx| {
                SettingsModal::new(
                    Theme::default(),
                    Density::default(),
                    Typography::default(),
                    Arc::new(AgentNotifySettings::default()),
                    repo,
                    Arc::new(NullNotifier),
                    schedules,
                    cx,
                )
            });
            *built.borrow_mut() = Some(m.clone());
            let any: gpui::AnyView = m.into();
            gpui_component::Root::new(any, window, cx)
        });
        (window, built.into_inner().expect("modal built inside the Root"))
    }

    /// The editor's current text, as the user would see it.
    fn editor_text(m: &SettingsModal, cx: &App) -> String {
        m.env_input.as_ref().expect("env editor built").read(cx).value().to_string()
    }

    /// The Agents pane paints with the new card in it, through the window's
    /// real draw cycle. A GPUI layout or borrow fault in the card is a runtime
    /// panic that no `cargo check` and no logic test reports.
    ///
    /// Painted via `VisualTestContext` rather than by calling the pane's render
    /// fn directly: an element built outside a frame keeps its `InputState`
    /// handles registered with the window, and gpui's leak check then fires at
    /// teardown for a card that rendered perfectly well.
    #[gpui::test]
    fn the_agents_pane_paints_with_the_environment_card(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.selected = SettingsPane::Agents;
                assert!(m.env_input.is_some(), "open() builds the env editor");
                assert!(m.profile_name_input.is_some(), "open() builds the name field");
                // Two profiles and a live env, so the card paints its full
                // shape — both segmented rows populated, the editor non-empty —
                // rather than the empty-state that would hide a layout fault.
                m.env_agent = "claude-code".into();
                m.agent_launch
                    .entry_mut("claude-code")
                    .env
                    .insert("ANTHROPIC_BASE_URL".into(), "https://first/v1".into());
                m.agent_launch
                    .profile_entry_mut("claude-code", Some("proxy"))
                    .env
                    .insert("ANTHROPIC_BASE_URL".into(), "https://second/v1".into());
                m.select_env_profile(Some("proxy".into()), window, cx);
            });
        })
        .expect("open on the Agents pane");

        let mut vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(gpui::size(px(1100.0), px(800.0)));
        vcx.run_until_parked();

        // Reaching here means the pane laid out and painted. Close through the
        // real path so the inputs are released before teardown.
        w.update(&mut vcx.cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert_eq!(m.env_profile.as_deref(), Some("proxy"), "selection survived paint");
                m.close(cx);
                assert!(m.env_input.is_none(), "close() drops the env editor");
            });
        })
        .expect("close");
    }

    /// Switching profiles must move the TEXT, not just the selection. The bug
    /// this pins: editing profile B while the box still shows A's env, so the
    /// first keystroke rewrites B with A's content.
    #[gpui::test]
    fn switching_profile_swaps_the_editor_text(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                m.agent_launch
                    .entry_mut("claude-code")
                    .env
                    .insert("BASE".into(), "https://first/v1".into());
                m.agent_launch
                    .profile_entry_mut("claude-code", Some("proxy"))
                    .env
                    .insert("BASE".into(), "https://second/v1".into());

                m.select_env_profile(None, window, cx);
                assert_eq!(editor_text(m, cx), "BASE=https://first/v1", "default's env");

                m.select_env_profile(Some("proxy".into()), window, cx);
                assert_eq!(editor_text(m, cx), "BASE=https://second/v1", "profile's env");

                m.select_env_profile(None, window, cx);
                assert_eq!(editor_text(m, cx), "BASE=https://first/v1", "and back again");
                m.close(cx);
            });
        })
        .expect("profile switch");
    }

    /// Switching agent resets to that agent's `default`: a profile name is
    /// per-adapter, so carrying "proxy" across would land on an unrelated (or
    /// absent) profile of the new agent.
    #[gpui::test]
    fn switching_agent_resets_to_its_default_profile(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.agent_launch.profile_entry_mut("claude-code", Some("proxy"));
                m.agent_launch
                    .entry_mut("codex")
                    .env
                    .insert("CODEX_HOST".into(), "h".into());
                m.select_env_agent("claude-code".into(), window, cx);
                m.select_env_profile(Some("proxy".into()), window, cx);
                assert_eq!(m.env_profile.as_deref(), Some("proxy"));

                m.select_env_agent("codex".into(), window, cx);
                assert_eq!(m.env_profile, None, "profile resets with the agent");
                assert_eq!(editor_text(m, cx), "CODEX_HOST=h", "editor shows the new agent");
                m.close(cx);
            });
        })
        .expect("agent switch");
    }

    /// The three ways this editor used to lose input without saying so: a line
    /// it could not parse, a key it will never apply, and a draft too big to
    /// hold. All three are now spoken, and none of them writes silently.
    #[gpui::test]
    fn the_env_editor_refuses_what_breaks_a_launch_and_says_which_line(cx: &mut TestAppContext) {
        use pane_agents_launch::{MAX_ENV_DRAFT, NoticeSlot};

        let (w, m) = modal(cx);
        let commit = |m: &Entity<SettingsModal>,
                      cx: &mut gpui::Context<'_, gpui_component::Root>,
                      window: &mut Window,
                      text: &str| {
            m.update(cx, |m, cx| {
                let input = m.env_input.clone().expect("env editor");
                input.update(cx, |s, cx| s.set_value(text, window, cx));
                input.update(cx, |_, cx| cx.emit(InputEvent::Change));
                input.update(cx, |_, cx| cx.emit(InputEvent::Blur));
            });
        };
        let notice = |m: &Entity<SettingsModal>, cx: &mut gpui::Context<'_, gpui_component::Root>| {
            m.update(cx, |m, _| {
                m.notice_for(NoticeSlot::Environment)
                    .map(|n| (n.ok, n.text.to_string()))
            })
        };

        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                let _ = cx;
            });
        })
        .expect("open");

        // 1. A line with no `=`. The map does not change, so a plain
        // "did anything change?" check would call this a no-op and stay quiet —
        // which is exactly how the line used to vanish.
        w.update(cx, |_root, window, cx| commit(&m, cx, window, "ANTHROPIC_BASE_URL https://p/v1"))
            .expect("malformed");
        let (ok, msg) = w
            .update(cx, |_root, _window, cx| notice(&m, cx))
            .expect("read")
            .expect("a malformed line is answered");
        assert!(!ok, "it is a refusal");
        assert!(msg.contains("Line 1") && msg.contains("="), "{msg}");

        // 2. A reserved key: reported, and filtered out of what resolution
        // hands a spawn even though the line stays in the draft.
        w.update(cx, |_root, window, cx| {
            commit(&m, cx, window, "PATH=/nowhere\nANTHROPIC_BASE_URL=https://p/v1")
        })
        .expect("reserved");
        let (ok, msg) = w
            .update(cx, |_root, _window, cx| notice(&m, cx))
            .expect("read")
            .expect("a reserved key is answered");
        assert!(!ok && msg.contains("PATH"), "{msg}");
        m.read_with(cx, |m, _| {
            assert_eq!(
                m.agent_launch.env_for("claude-code", None),
                vec![("ANTHROPIC_BASE_URL".to_string(), "https://p/v1".to_string())],
                "the reserved key never reaches a spawn",
            );
            assert_eq!(
                m.agent_launch.for_agent("claude-code").expect("entry").env.len(),
                2,
                "but the line the user typed is still there to be corrected",
            );
        });

        // 3. A clean draft is acknowledged with the count that will actually
        // apply, and the notice goes green.
        w.update(cx, |_root, window, cx| {
            commit(&m, cx, window, "ANTHROPIC_BASE_URL=https://p/v1\nANTHROPIC_AUTH_TOKEN=x")
        })
        .expect("clean");
        let (ok, msg) = w
            .update(cx, |_root, _window, cx| notice(&m, cx))
            .expect("read")
            .expect("a clean write is acknowledged");
        assert!(ok && msg.contains('2'), "{msg}");

        // 4. Over the cap: nothing is written, and the last good value stands.
        let huge = format!("BIG={}", "x".repeat(MAX_ENV_DRAFT));
        assert!(huge.len() > MAX_ENV_DRAFT);
        w.update(cx, |_root, window, cx| commit(&m, cx, window, &huge)).expect("oversized");
        let (ok, msg) = w
            .update(cx, |_root, _window, cx| notice(&m, cx))
            .expect("read")
            .expect("an oversized draft is answered");
        assert!(!ok && msg.contains("Too large"), "{msg}");
        m.read_with(cx, |m, _| {
            assert!(
                !m.agent_launch.for_agent("claude-code").expect("entry").env.contains_key("BIG"),
                "an oversized draft must not reach the working copy at all — \
                 the close-flush would otherwise write it",
            );
            assert_eq!(m.agent_launch.env_for("claude-code", None).len(), 2);
        });

        w.update(cx, |_root, _window, cx| m.update(cx, |m, cx| m.close(cx))).expect("close");
    }

    /// Masking is display-only, and hiding has to commit first: the editor is
    /// unmounted while masked, so its blur — the write — never fires.
    #[gpui::test]
    fn hiding_the_values_commits_the_draft_first(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                assert!(!m.env_revealed, "values start masked");

                m.toggle_env_reveal(cx);
                assert!(m.env_revealed);

                let input = m.env_input.clone().expect("env editor");
                input.update(cx, |s, cx| s.set_value("ANTHROPIC_AUTH_TOKEN=secret", window, cx));
                input.update(cx, |_, cx| cx.emit(InputEvent::Change));
                // Hide WITHOUT blurring — the hazard.
                m.toggle_env_reveal(cx);
                assert!(!m.env_revealed);
                assert_eq!(
                    m.agent_launch.env_for("claude-code", None),
                    vec![("ANTHROPIC_AUTH_TOKEN".to_string(), "secret".to_string())],
                    "hiding wrote the draft rather than stranding it",
                );

                // Selecting elsewhere re-masks: a reveal is about one profile's
                // values and must not carry to the next.
                m.toggle_env_reveal(cx);
                assert!(m.env_revealed);
                m.select_env_profile(Some("proxy".into()), window, cx);
                assert!(!m.env_revealed);
                m.close(cx);
            });
        })
        .expect("reveal, edit, hide");
    }

    /// The functional hole phase 3 closes, and the consistency requirement that
    /// falls out of it.
    ///
    /// Painted through `VisualTestContext` with both cards live, not asserted
    /// off the map alone: with `default` selected the flags/model controls in
    /// the environment card and the agent row above address ONE entry, and
    /// "the row above didn't update until I reopened the modal" is exactly the
    /// class of defect a logic test reports as passing.
    #[gpui::test]
    fn flags_and_model_write_the_selected_profile_and_both_cards_agree(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);

        // The agent row's subtitle, read through the same entry list the pane
        // renders from — so this asserts what the row says, not a private fn.
        let row_subtitle = |m: &Entity<SettingsModal>, cx: &mut gpui::Context<'_, gpui_component::Root>| {
            m.update(cx, |m, cx| {
                let theme = m.theme;
                let density = m.density;
                let typography = m.typography.clone();
                pane_agents_launch::entries(m, theme, density, &typography, cx)
                    .into_iter()
                    .find(|e| e.label == "Claude Code")
                    .expect("the Claude Code agent row")
                    .description
                    .to_string()
            })
        };

        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.selected = SettingsPane::Agents;
                m.env_agent = "claude-code".into();
                m.agent_launch.entry_mut("claude-code").model = "opus".into();
                m.agent_launch.profile_entry_mut("claude-code", Some("proxy"));
                m.select_env_profile(Some("proxy".into()), window, cx);

                // 1. With a profile selected, the write lands on the PROFILE.
                let e = m.selected_launch_mut();
                e.model = "haiku".into();
                e.args = "--quiet".into();
                m.persist_agent_launch(cx);
                assert_eq!(
                    m.agent_launch.model_for_in("claude-code", Some("proxy")).as_deref(),
                    Some("haiku"),
                );
                assert_eq!(
                    m.agent_launch.model_for("claude-code").as_deref(),
                    Some("opus"),
                    "the agent's default entry must be untouched",
                );
                assert!(
                    m.agent_launch.args_for("claude-code").is_empty(),
                    "and so must its flags",
                );
            });
        })
        .expect("edit the selected profile");

        let vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(gpui::size(px(1100.0), px(800.0)));
        vcx.run_until_parked();

        // 2. The agent row now carries a profile count, so it does not read as
        // a description of every profile.
        let before = w
            .update(cx, |_root, _window, cx| row_subtitle(&m, cx))
            .expect("read the row");
        assert!(
            before.contains("model opus") && before.contains("1 profile"),
            "the row describes the default and admits the profile: {before}",
        );

        // 3. Switch to `default` and write again: now the SAME entry the row
        // above shows, and the row must reflect it in the next paint.
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.select_env_profile(None, window, cx);
                let e = m.selected_launch_mut();
                e.model = "sonnet".into();
                m.persist_agent_launch(cx);
            });
        })
        .expect("edit the default");
        vcx.run_until_parked();

        let after = w
            .update(cx, |_root, _window, cx| row_subtitle(&m, cx))
            .expect("read the row again");
        assert!(
            after.contains("model sonnet"),
            "the agent row must follow a write made from the card below: {after}",
        );
        assert_eq!(
            m.read_with(cx, |m, _| m.agent_launch.model_for_in("claude-code", Some("proxy"))),
            Some("haiku".to_string()),
            "and the profile must not have been dragged along",
        );

        w.update(cx, |_root, _window, cx| m.update(cx, |m, cx| m.close(cx)))
            .expect("close");
    }

    /// Rename and duplicate are the two operations that did not exist, and both
    /// have to keep the editor pointing somewhere real: a rename keeps editing
    /// the profile under its new name, a duplicate moves to the copy.
    #[gpui::test]
    fn renaming_keeps_the_editor_on_it_and_duplicating_moves_to_the_copy(cx: &mut TestAppContext) {
        use pane_agents_launch::{NoticeSlot, ProfileNameMode};

        let (w, m) = modal(cx);
        // Drive through the `InputState` event: the dispatch on mode lives in
        // the subscription, which no logic test reaches.
        let commit = |m: &Entity<SettingsModal>,
                      cx: &mut gpui::Context<'_, gpui_component::Root>,
                      window: &mut Window,
                      mode: ProfileNameMode,
                      text: &str| {
            m.update(cx, |m, cx| {
                m.begin_profile_name(mode, window, cx);
                let field = m.profile_name_input.clone().expect("name field");
                field.update(cx, |s, cx| s.set_value(text, window, cx));
                field.update(cx, |_, cx| cx.emit(InputEvent::PressEnter { secondary: false }));
            });
        };

        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                m.agent_launch.profile_entry_mut("claude-code", Some("proxy")).env
                    .insert("BASE".into(), "https://second/v1".into());
                m.select_env_profile(Some("proxy".into()), window, cx);
            });
        })
        .expect("seed a profile");
        cx.run_until_parked();

        // Rename the profile being edited.
        w.update(cx, |_root, window, cx| {
            commit(&m, cx, window, ProfileNameMode::Rename("proxy".into()), "staging")
        })
        .expect("rename");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert_eq!(
                    m.agent_launch.profile_names("claude-code"),
                    vec![DEFAULT_PROFILE, "staging"],
                );
                assert_eq!(
                    m.env_profile.as_deref(),
                    Some("staging"),
                    "the editor follows the profile through its rename",
                );
                assert_eq!(editor_text(m, cx), "BASE=https://second/v1");
                let n = m.notice_for(NoticeSlot::Profile).expect("a rename is answered");
                assert!(n.ok && n.text.contains("proxy") && n.text.contains("staging"));
                // The field closes on a commit: it is revealed by an action,
                // not permanently open.
                assert!(m.profile_name_mode.is_none());
            });
        })
        .expect("assert rename");

        // Duplicate it. The copy is independent of its source.
        w.update(cx, |_root, window, cx| {
            commit(&m, cx, window, ProfileNameMode::Duplicate("staging".into()), "staging-copy")
        })
        .expect("duplicate");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert_eq!(
                    m.agent_launch.profile_names("claude-code"),
                    vec![DEFAULT_PROFILE, "staging", "staging-copy"],
                );
                assert_eq!(
                    m.env_profile.as_deref(),
                    Some("staging-copy"),
                    "a duplicate moves the editor to the copy, which is the one to edit",
                );
                assert_eq!(editor_text(m, cx), "BASE=https://second/v1", "the copy carries the env");
                let n = m.notice_for(NoticeSlot::Profile).expect("a duplicate is answered");
                assert!(n.ok && n.text.contains("staging-copy"));
                m.close(cx);
            });
        })
        .expect("assert duplicate");
    }

    /// Deleting must be two presses, and the second must say what it removed.
    /// Cancelling after the first must leave the profile exactly as it was.
    #[gpui::test]
    fn deleting_a_profile_asks_first_and_then_says_what_went(cx: &mut TestAppContext) {
        use pane_agents_launch::NoticeSlot;

        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                m.agent_launch.profile_entry_mut("claude-code", Some("proxy"));

                // Armed, then cancelled: nothing is removed.
                m.arm_profile_delete("proxy".into(), window, cx);
                assert_eq!(m.pending_profile_delete.as_deref(), Some("proxy"));
                m.cancel_profile_delete(cx);
                assert_eq!(m.pending_profile_delete, None);
                assert_eq!(
                    m.agent_launch.profile_names("claude-code"),
                    vec![DEFAULT_PROFILE, "proxy"],
                    "cancelling must leave the profile alone",
                );

                // Selecting somewhere else also disarms, so an armed row can't
                // follow the user around the card.
                m.arm_profile_delete("proxy".into(), window, cx);
                m.select_env_profile(None, window, cx);
                assert_eq!(m.pending_profile_delete, None);

                // Confirmed: removed, deselected, and acknowledged by name.
                m.arm_profile_delete("proxy".into(), window, cx);
                m.confirm_profile_delete("proxy", window, cx);
                assert_eq!(m.agent_launch.profile_names("claude-code"), vec![DEFAULT_PROFILE]);
                assert_eq!(m.env_profile, None);
                let n = m.notice_for(NoticeSlot::Profile).expect("a delete is answered");
                assert!(n.ok && n.text.contains("proxy"), "{}", n.text);
                m.close(cx);
            });
        })
        .expect("delete a profile");
    }

    /// Removing a profile falls back to `default` and must not carry the removed
    /// profile's env onto the default entry via the flush-on-leave.
    #[gpui::test]
    fn removing_a_profile_falls_back_without_leaking_its_env(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                m.agent_launch
                    .entry_mut("claude-code")
                    .env
                    .insert("BASE".into(), "https://first/v1".into());
                m.agent_launch
                    .profile_entry_mut("claude-code", Some("proxy"))
                    .env
                    .insert("BASE".into(), "https://second/v1".into());
                m.select_env_profile(Some("proxy".into()), window, cx);

                // Two steps: arm, then confirm. Arming alone must not remove.
                m.arm_profile_delete("proxy".into(), window, cx);
                assert_eq!(m.pending_profile_delete.as_deref(), Some("proxy"));
                assert_eq!(m.agent_launch.profile_names("claude-code").len(), 2);
                m.confirm_profile_delete("proxy", window, cx);
                assert_eq!(m.env_profile, None);
                assert_eq!(m.pending_profile_delete, None);
                assert_eq!(
                    m.agent_launch.profile_names("claude-code"),
                    vec![DEFAULT_PROFILE]
                );
                // The default entry keeps ITS value — the removed profile's env
                // must not have been flushed onto it on the way out.
                assert_eq!(
                    m.agent_launch.env_for("claude-code", None),
                    vec![("BASE".to_string(), "https://first/v1".to_string())]
                );
                assert_eq!(editor_text(m, cx), "BASE=https://first/v1");
                m.close(cx);
            });
        })
        .expect("profile removal");
    }

    /// Typing parses into the working copy on every keystroke, so a close
    /// without blur still has something to flush.
    ///
    /// Uses `insert` — the path a keystroke takes — not `set_value`, which
    /// deliberately suppresses events (see the next test).
    #[gpui::test]
    fn typing_env_text_updates_the_working_copy(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                let input = m.env_input.clone().expect("env editor built");
                input.update(cx, |s, cx| {
                    s.insert("ANTHROPIC_BASE_URL=https://typed/v1", window, cx)
                });
            });
        })
        .expect("type");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert_eq!(
                    m.agent_launch.env_for("claude-code", None),
                    vec![(
                        "ANTHROPIC_BASE_URL".to_string(),
                        "https://typed/v1".to_string()
                    )],
                    "keystrokes reach the working copy"
                );
                m.close(cx);
            });
        })
        .expect("assert");
    }

    /// Re-seeding the editor on a profile switch must NOT be mistaken for the
    /// user editing the newly-selected profile.
    ///
    /// `InputState::set_value` clears `emit_events` for exactly this reason, so
    /// the `Change` subscription doesn't fire and immediately write the loaded
    /// text back. If that ever changed, switching profiles would silently stamp
    /// one profile's env onto another — this pins the assumption the selection
    /// handlers are built on.
    #[gpui::test]
    fn reseeding_the_editor_does_not_write_back_as_an_edit(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                m.agent_launch
                    .profile_entry_mut("claude-code", Some("proxy"))
                    .env
                    .insert("ONLY_ON_PROXY".into(), "1".into());
                // Land on proxy (editor now shows its env), then leave.
                m.select_env_profile(Some("proxy".into()), window, cx);
                m.select_env_profile(None, window, cx);
            });
        })
        .expect("switch");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert!(
                    m.agent_launch.env_for("claude-code", None).is_empty(),
                    "the default entry must not have absorbed the profile's env"
                );
                assert_eq!(
                    m.agent_launch.env_for("claude-code", Some("proxy")),
                    vec![("ONLY_ON_PROXY".to_string(), "1".to_string())],
                    "and the profile keeps its own"
                );
                m.close(cx);
            });
        })
        .expect("assert");
    }

    /// Typing a name and pressing Enter is the ONLY way to create a profile,
    /// so the subscription that turns that keystroke into a profile is the
    /// whole feature. It runs on an `InputState` event, which no logic test
    /// reaches: a broken wire leaves the picker showing `default` forever with
    /// nothing in the log to say why.
    #[gpui::test]
    fn enter_in_the_name_field_creates_and_selects_the_profile(cx: &mut TestAppContext) {
        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                assert_eq!(
                    m.agent_launch.profile_names("claude-code"),
                    vec![DEFAULT_PROFILE],
                    "a fresh config reports only the implicit default"
                );
                // The `+` affordance is what reveals the field; without a mode
                // there is nothing on screen to press Enter in.
                m.begin_profile_name(pane_agents_launch::ProfileNameMode::Add, window, cx);
                let field = m.profile_name_input.clone().expect("name field");
                field.update(cx, |s, cx| s.set_value("proxy", window, cx));
                // The event the Input emits on Enter, not a direct call to the
                // creation code — the subscription is what is under test.
                field.update(cx, |_, cx| {
                    cx.emit(InputEvent::PressEnter { secondary: false })
                });
            });
        })
        .expect("type a profile name and press Enter");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert_eq!(
                    m.agent_launch.profile_names("claude-code"),
                    vec![DEFAULT_PROFILE, "proxy"],
                    "Enter created the profile"
                );
                assert_eq!(
                    m.env_profile.as_deref(),
                    Some("proxy"),
                    "and selected it, so the editor below now edits the new profile"
                );
                assert!(
                    m.profile_name_input
                        .as_ref()
                        .expect("name field")
                        .read(cx)
                        .value()
                        .is_empty(),
                    "the name field clears, so the next name starts blank"
                );
                m.close(cx);
            });
        })
        .expect("assert");
    }

    /// Every commit path through the name field must ANSWER. Creating says
    /// which profile it made; each of the three refusals says which rule was
    /// broken. All four used to be silent — the reported confusion ("seem only
    /// default? and how to save new profile?") was exactly this.
    ///
    /// Driven through the `InputState` event, like the creation test: the
    /// notices are raised inside the subscription, which no logic test reaches.
    #[gpui::test]
    fn every_profile_commit_answers_the_user(cx: &mut TestAppContext) {
        use pane_agents_launch::NoticeSlot;

        let (w, m) = modal(cx);
        // Each case: what to type, and whether it should be accepted.
        let press = |m: &Entity<SettingsModal>,
                     cx: &mut gpui::Context<'_, gpui_component::Root>,
                     window: &mut Window,
                     text: &str| {
            m.update(cx, |m, cx| {
                // A commit closes the field, so each press re-opens it.
                m.begin_profile_name(pane_agents_launch::ProfileNameMode::Add, window, cx);
                let field = m.profile_name_input.clone().expect("name field");
                field.update(cx, |s, cx| s.set_value(text, window, cx));
                field.update(cx, |_, cx| cx.emit(InputEvent::PressEnter { secondary: false }));
            });
        };

        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
            });
        })
        .expect("open");
        cx.run_until_parked();

        // 1. A real name is created AND acknowledged by name.
        w.update(cx, |_root, window, cx| press(&m, cx, window, "proxy")).expect("create");
        cx.run_until_parked();
        let created = w
            .update(cx, |_root, _window, cx| {
                m.update(cx, |m, _| {
                    let n = m.notice_for(NoticeSlot::Profile).expect("a create is answered").clone();
                    assert!(n.ok, "creating is not a failure");
                    n.text.to_string()
                })
            })
            .expect("read notice");
        assert!(created.contains("proxy"), "the confirmation names it: {created}");

        // 2-4. Each refusal answers, creates nothing, and says something
        // different from the others.
        let mut refusals = Vec::new();
        for typed in ["   ", DEFAULT_PROFILE, "proxy"] {
            w.update(cx, |_root, window, cx| press(&m, cx, window, typed)).expect("refused");
            cx.run_until_parked();
            let text = w
                .update(cx, |_root, _window, cx| {
                    m.update(cx, |m, _| {
                        let n = m
                            .notice_for(NoticeSlot::Profile)
                            .unwrap_or_else(|| panic!("“{typed}” was refused silently"))
                            .clone();
                        assert!(!n.ok, "“{typed}” must read as a refusal: {}", n.text);
                        assert_eq!(
                            m.agent_launch.profile_names("claude-code"),
                            vec![DEFAULT_PROFILE, "proxy"],
                            "“{typed}” must not have created anything"
                        );
                        n.text.to_string()
                    })
                })
                .expect("read refusal");
            refusals.push(text);
        }
        refusals.sort();
        refusals.dedup();
        assert_eq!(refusals.len(), 3, "the three refusals must be distinguishable");

        // Typing again retires the answer, so the card never shows a stale one.
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                let field = m.profile_name_input.clone().expect("name field");
                field.update(cx, |s, cx| s.set_value("stag", window, cx));
                field.update(cx, |_, cx| cx.emit(InputEvent::Change));
            });
        })
        .expect("type");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                assert!(m.env_notice.is_none(), "typing must clear the previous answer");
                m.close(cx);
            });
        })
        .expect("assert");
    }

    /// The env editor writes on blur and used to say nothing at all. It must
    /// acknowledge a write — and, just as importantly, stay quiet on a blur
    /// that changed nothing, or the acknowledgment becomes noise nobody reads.
    #[gpui::test]
    fn the_env_editor_acknowledges_a_write_but_not_a_no_op(cx: &mut TestAppContext) {
        use pane_agents_launch::NoticeSlot;

        let (w, m) = modal(cx);
        w.update(cx, |_root, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.env_agent = "claude-code".into();
                let input = m.env_input.clone().expect("env editor");
                input.update(cx, |s, cx| {
                    s.set_value("ANTHROPIC_BASE_URL=https://proxy.internal/v1", window, cx)
                });
                input.update(cx, |_, cx| cx.emit(InputEvent::Change));
                input.update(cx, |_, cx| cx.emit(InputEvent::Blur));
            });
        })
        .expect("write then blur");
        cx.run_until_parked();
        w.update(cx, |_root, _window, cx| {
            m.update(cx, |m, cx| {
                let n = m
                    .notice_for(NoticeSlot::Environment)
                    .expect("a write is acknowledged")
                    .clone();
                assert!(n.ok, "a successful write is not a failure: {}", n.text);
                assert!(n.text.contains('1'), "it says how much it saved: {}", n.text);

                // A second blur with nothing changed must NOT re-announce.
                m.env_notice = None;
                let input = m.env_input.clone().expect("env editor");
                input.update(cx, |_, cx| cx.emit(InputEvent::Blur));
                assert!(
                    m.env_notice.is_none(),
                    "a blur that committed nothing must stay silent"
                );
                m.close(cx);
            });
        })
        .expect("assert");
    }
}
