//! Agent-launch section of the Agents pane — edits the `AgentLaunchSettings`
//! working copy (default agent, per-agent enabled / skip-permissions / model).
//! Applies immediately: mutate the copy, write `agent_launch.toml`, watcher
//! reloads + swaps the global. These are the defaults the one-click launcher
//! applies when the user picks an agent.

use gpui::{
    AnyElement, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px, svg,
};
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use std::collections::BTreeMap;

use oximux_settings::{
    DEFAULT_PROFILE, Density, OpenMode, PerAgentLaunch, Theme, Typography, split_args,
};

use super::SettingsModal;
use super::controls::{icon_button, toggle_chip, toggle_switch, value_chip};
use super::layout::{
    SettingEntry, card_surface, entry, hint_text, list_row, notice_text, section_card,
    setting_row_action_hint, setting_row_desc, setting_row_desc_hint,
};
use super::segmented::{Segment, segmented};
use crate::shell::agent_presentation::adapter_icon_path;

/// Which row of the environment card a [`Notice`] belongs under.
///
/// One notice is live at a time: a notice is the answer to a *discrete commit*,
/// and the card has no way to commit two things at once. Carrying the slot on
/// the notice rather than keeping one `Option` per row means a message can
/// never be left stranded under a row the user has since moved away from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::shell) enum NoticeSlot {
    /// Under the "Add a profile" row — the result of a create attempt.
    Profile,
    /// Under the environment editor — the result of a write.
    Environment,
}

/// A transient message acknowledging a commit or explaining a refusal.
///
/// Not persisted and not a toast: it is view state on the modal, cleared the
/// moment the user types again, so the card never shows a stale answer to a
/// question that has moved on.
#[derive(Clone, Debug)]
pub(in crate::shell) struct Notice {
    pub(in crate::shell) slot: NoticeSlot,
    /// `true` acknowledges a commit, `false` explains a refusal — drives the
    /// colour in [`notice_text`].
    pub(in crate::shell) ok: bool,
    pub(in crate::shell) text: SharedString,
}

impl Notice {
    pub(in crate::shell) fn ok(slot: NoticeSlot, text: impl Into<SharedString>) -> Self {
        Self { slot, ok: true, text: text.into() }
    }

    pub(in crate::shell) fn err(slot: NoticeSlot, text: impl Into<SharedString>) -> Self {
        Self { slot, ok: false, text: text.into() }
    }
}

/// What the card's single name field is currently naming.
///
/// One field, not three: add, rename, and duplicate are all "type a name,
/// press Enter", they share [`validate_profile_name`] and its three refusals,
/// and three live `InputState`s in one card is three focus handles and three
/// subscriptions to keep in step. The mode is what the Enter handler
/// dispatches on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::shell) enum ProfileNameMode {
    /// Create a new profile, seeded from the agent's current configuration.
    Add,
    /// Rename the named profile.
    Rename(String),
    /// Copy the named profile (which may be `default`) under a new name.
    Duplicate(String),
}

impl ProfileNameMode {
    /// The text the field opens with. Rename starts from the current name
    /// because a rename is usually an edit of it; duplicate offers a name that
    /// is already valid, so Enter alone is a complete answer.
    pub(in crate::shell) fn seed(&self) -> String {
        match self {
            Self::Add => String::new(),
            Self::Rename(name) => name.clone(),
            Self::Duplicate(name) => format!("{name}-copy"),
        }
    }

    /// What the field is asking for, shown under it while nothing has been
    /// refused or confirmed yet.
    fn prompt(&self) -> SharedString {
        match self {
            Self::Add => "Type a name for the new profile and press Enter.".into(),
            Self::Rename(name) => {
                SharedString::from(format!("Rename “{name}” — type the new name and press Enter."))
            }
            Self::Duplicate(name) => SharedString::from(format!(
                "Copy “{name}” — type a name for the copy and press Enter."
            )),
        }
    }
}

/// Validate a typed profile name against the names an agent already has
/// (`existing` is [`AgentLaunchSettings::profile_names`], so it always leads
/// with `default`). `Ok` carries the trimmed name to create.
///
/// The three rejections exist because all three were previously silent. Blank
/// and `default` returned early from the Enter handler, and a duplicate was
/// worse than an error: `profile_entry_mut` *reuses* an entry of the same name,
/// so retyping an existing name looked like a button that did nothing while
/// actually re-selecting the profile the user already had.
///
/// Names are compared exactly (after trimming), matching how `for_agent_in`
/// resolves them — a check that folded case would refuse a name the launcher
/// would happily treat as distinct.
///
/// [`AgentLaunchSettings::profile_names`]: oximux_settings::AgentLaunchSettings::profile_names
pub(super) fn validate_profile_name(raw: &str, existing: &[String]) -> Result<String, SharedString> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Type a name for the profile, then press Enter.".into());
    }
    if name == DEFAULT_PROFILE {
        return Err(SharedString::from(format!(
            "“{DEFAULT_PROFILE}” is this agent's plain configuration — pick another name."
        )));
    }
    if existing.iter().any(|n| n.trim() == name) {
        return Err(SharedString::from(format!(
            "This agent already has a profile named “{name}”."
        )));
    }
    Ok(name.to_string())
}

/// The environment editor's placeholder for `adapter_id`: a worked example of
/// the shape the field wants, not a single variable.
///
/// Per-agent because a single example implies it is the only thing worth
/// setting, and Anthropic variables in the Codex editor are actively
/// misleading. Every line here is a variable the named CLI actually reads, and
/// none of them is a key the field refuses.
pub(super) fn env_placeholder(adapter_id: &str) -> &'static str {
    match adapter_id {
        "codex" => {
            "OPENAI_BASE_URL=https://proxy.internal/v1\n\
             OPENAI_API_KEY=sk-...\n\
             # blank lines and # comments are ignored"
        }
        // Pi-family agents route to several providers, so one provider's
        // variables would under-describe the field.
        "pi" | "omp" => {
            "ANTHROPIC_API_KEY=sk-ant-...\n\
             OPENAI_API_KEY=sk-...\n\
             # blank lines and # comments are ignored"
        }
        _ => {
            "ANTHROPIC_BASE_URL=https://proxy.internal/v1\n\
             ANTHROPIC_AUTH_TOKEN=sk-ant-...\n\
             # blank lines and # comments are ignored"
        }
    }
}
/// The largest environment draft the editor will commit, in bytes.
///
/// A cap rather than a truncation: silently keeping the first 8 KB of a paste
/// is the same class of failure as silently dropping a malformed line. Well
/// past any real set of variables — the longest plausible entry is a token,
/// and 8 KB is dozens of them.
pub(super) const MAX_ENV_DRAFT: usize = 8 * 1024;

/// A line of the environment draft that will not become a variable, and why.
///
/// Every one of these used to be a `continue`: the line disappeared on reopen
/// and the field read as having accepted it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::shell) enum EnvReject {
    /// No `=` anywhere — the line names nothing to set.
    NoAssignment { line: usize },
    /// `=value` with nothing before the `=`.
    BlankKey { line: usize },
    /// A key resolution refuses to apply (see `is_reserved_env_key`).
    Reserved { line: usize, key: String },
}

impl EnvReject {
    /// What the user is told. Names the line number in every case, because the
    /// draft is free text and "one of your lines is wrong" is not actionable.
    fn message(&self) -> String {
        match self {
            Self::NoAssignment { line } => {
                format!("Line {line} has no “=” — write it as KEY=value.")
            }
            Self::BlankKey { line } => format!("Line {line} has no name before its “=”."),
            Self::Reserved { line, key } => format!(
                "Line {line}: “{key}” is set by OxiMux and can't be overridden here — \
                 it would break the launch."
            ),
        }
    }
}

/// Sum up what a draft's rejects should say: the first one in full, plus a
/// count of the rest, so one message stays one line however many lines are
/// wrong.
pub(super) fn reject_message(rejects: &[EnvReject]) -> Option<String> {
    let first = rejects.first()?;
    Some(match rejects.len() {
        1 => first.message(),
        n => format!("{} (+{} more)", first.message(), n - 1),
    })
}

/// Parse the environment editor's text into the map [`PerAgentLaunch::env`]
/// holds, together with the lines that did not become variables.
///
/// One `KEY=value` per line, `.env`-shaped because that is the form a proxy or
/// alternate-endpoint configuration is already pasted from.
///
/// - The FIRST `=` splits; a value may contain further `=` (a URL query, a
///   base64 token) without quoting.
/// - Blank lines and `#` comments are skipped silently — they are annotation,
///   not failed input.
/// - A line with no `=`, an empty key, or a reserved key is REPORTED rather
///   than guessed at. A reserved key is still kept in the map: resolution
///   filters it, and deleting the user's line would be the silent drop this
///   exists to stop.
/// - Key and value are both trimmed. A trailing space in a key is a *different*
///   variable than the one the user meant, which is the failure this prevents.
///
/// [`PerAgentLaunch::env`]: oximux_settings::PerAgentLaunch::env
pub(super) fn parse_env_draft(raw: &str) -> (BTreeMap<String, String>, Vec<EnvReject>) {
    let mut out = BTreeMap::new();
    let mut rejects = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        // 1-based: the editor shows lines the way a person counts them.
        let n = idx + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            rejects.push(EnvReject::NoAssignment { line: n });
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            rejects.push(EnvReject::BlankKey { line: n });
            continue;
        }
        if oximux_settings::is_reserved_env_key(k) {
            rejects.push(EnvReject::Reserved { line: n, key: k.to_string() });
        }
        out.insert(k.to_string(), v.trim().to_string());
    }
    (out, rejects)
}

/// The map half of [`parse_env_draft`], for the keystroke-rate sync that has
/// no use for diagnostics.
pub(super) fn parse_env_lines(raw: &str) -> BTreeMap<String, String> {
    parse_env_draft(raw).0
}

/// Render an env map back into editor text: one `KEY=value` per line, in the
/// map's key order (it is a `BTreeMap`, so sorted and stable across reopens).
pub(super) fn format_env_lines(env: &BTreeMap<String, String>) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n")
}

/// The masked stand-in for the editor: the keys, with every value replaced by
/// a fixed run of bullets.
///
/// Fixed-width on purpose — a mask that tracks the value's length leaks it,
/// and a token's length is a meaningful hint. Keys are shown in full: the key
/// is what makes the row identifiable, and it is not the secret.
pub(super) fn masked_env_preview(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| if v.is_empty() { format!("{k}=") } else { format!("{k}=••••••••") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Built-in agents exposed in the launch settings, in picker order. The
/// custom adapter is excluded — its command is fully user-supplied, so a
/// "default flags" override has no meaning.
pub(super) const LAUNCH_AGENTS: [(&str, &str); 4] = [
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("pi", "Pi"),
    ("omp", "omp"),
];

/// The skip-permissions ("YOLO") flag for an agent — toggled in and out of
/// that agent's free-text args by the skip-perms chip. Each CLI spells it
/// differently; `None` means the agent has no approval gate to skip (pi runs
/// tools ungated already), so the row renders no skip-perms chip at all.
/// An unknown id falls back to the most common spelling.
fn yolo_flag(adapter_id: &str) -> Option<&'static str> {
    match adapter_id {
        "codex" => Some("--dangerously-bypass-approvals-and-sandbox"),
        "pi" => None,
        // omp's skip-everything is its own approval mode, not a bare flag.
        // The `_` fallback would hand it Claude's flag, which omp rejects at
        // parse — locked by test (red-team F8).
        "omp" => Some("--approval-mode yolo"),
        // claude-code and any future addition default to claude's spelling.
        _ => Some("--dangerously-skip-permissions"),
    }
}

/// Model presets the per-agent model chip cycles through. The leading empty
/// string is the "Default" option (no `--model` flag). Mirrors the slugs the
/// adapters accept. Pi offers only "Default": its catalog is per-user and a
/// bare model id is fuzzy-matched across providers, so a static list would
/// misresolve — a hand-edited `provider/id` still survives the cycle.
fn model_presets(adapter_id: &str) -> &'static [&'static str] {
    match adapter_id {
        "codex" => &["", "gpt-5-codex", "o3"],
        // Pi-family catalogs are per-user and bare ids fuzzy-match across
        // providers — only "Default" is safe to offer statically. The `_`
        // fallback would hand omp Claude's opus/sonnet/haiku (F8).
        "pi" | "omp" => &[""],
        _ => &["", "opus", "sonnet", "haiku"],
    }
}

/// Whether `args` already contains `flag` — a single token, or a multi-token
/// phrase matched as a contiguous token subsequence (omp's skip-everything is
/// `--approval-mode yolo`, two tokens that round-trip through the free-text
/// args as two tokens).
fn has_flag(args: &str, flag: &str) -> bool {
    let toks = split_args(args);
    let flag_toks: Vec<&str> = flag.split_whitespace().collect();
    !flag_toks.is_empty()
        && toks.windows(flag_toks.len()).any(|w| w.iter().map(String::as_str).eq(flag_toks.iter().copied()))
}

/// Add or remove `flag` (token phrase, see [`has_flag`]) from a free-text
/// args string, preserving any other tokens the user configured. Re-joined
/// with single spaces.
fn toggle_flag(args: &str, flag: &str) -> String {
    let mut toks = split_args(args);
    let flag_toks: Vec<&str> = flag.split_whitespace().collect();
    let mut removed = false;
    if !flag_toks.is_empty() {
        let mut i = 0;
        while i + flag_toks.len() <= toks.len() {
            if toks[i..i + flag_toks.len()].iter().map(String::as_str).eq(flag_toks.iter().copied())
            {
                toks.drain(i..i + flag_toks.len());
                removed = true;
            } else {
                i += 1;
            }
        }
    }
    if !removed {
        // Turning a `--name value` phrase ON must first clear any OTHER value
        // the user configured for the same `--name` — appending alongside it
        // would ship `--approval-mode always-ask --approval-mode yolo`, and
        // although omp resolves repeats last-wins (its parser reassigns per
        // occurrence), an argv that says two things is wrong to write.
        if flag_toks.len() == 2 && flag_toks[0].starts_with("--") {
            let name = flag_toks[0];
            let mut i = 0;
            while i < toks.len() {
                if toks[i] == name {
                    // Remove the name and, when present, its value token.
                    let take = if i + 1 < toks.len() { 2 } else { 1 };
                    toks.drain(i..i + take);
                } else {
                    i += 1;
                }
            }
        }
        toks.extend(flag_toks.iter().map(|t| t.to_string()));
    }
    toks.join(" ")
}

/// Next preset after `current`, wrapping. A non-preset value is anchored
/// (prepended) so a hand-edited model survives a cycle and is reachable.
fn cycle_model(presets: &[&str], current: &str) -> String {
    let mut list: Vec<&str> = Vec::with_capacity(presets.len() + 1);
    if !presets.contains(&current) {
        list.push(current);
    }
    list.extend_from_slice(presets);
    let idx = list.iter().position(|m| *m == current).unwrap_or(0);
    list[(idx + 1) % list.len()].to_string()
}

fn model_label(model: &str) -> String {
    if model.trim().is_empty() {
        "Model: Default".to_string()
    } else {
        format!("Model: {model}")
    }
}

/// The agent-launch settings as reusable entries (also fed to global search).
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut out = Vec::with_capacity(LAUNCH_AGENTS.len() + 2);
    out.push(entry(
        "Agent status hooks",
        "Show the live tool on agent dashboard cards (Claude Code).",
        status_hooks_toggle(modal, theme, cx),
    ));
    out.push(entry(
        "Open new agents as chat",
        "Open Claude launches as a structured chat thread instead of a terminal.",
        default_open_mode_toggle(modal, theme, cx),
    ));
    out.push(entry(
        "Default agent",
        "Surfaced first in the launcher.",
        default_agent_control(modal, theme, density, typography, cx),
    ));
    for (adapter_id, display) in LAUNCH_AGENTS {
        out.push(entry(
            display,
            agent_summary(modal, adapter_id),
            agent_control(modal, adapter_id, theme, density, typography, cx),
        ));
    }
    out
}

/// The environment + profiles card: pick an agent, pick (or add) one of its
/// launch profiles, and edit that profile's `KEY=value` overrides.
///
/// A separate card from the per-agent rows above because it is a *different
/// selection model*: those rows show every agent at once, while this one edits
/// one `(agent, profile)` pair at a time — the text editor holds exactly one
/// pair's content, so showing four of them would mean four live inputs.
pub(super) fn render_env_card(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::with_capacity(4);

    // Which agent's environment is being edited.
    let current_agent = modal.env_agent;
    let agent_segs: Vec<Segment> = LAUNCH_AGENTS
        .iter()
        .map(|(id, display)| {
            let id = *id;
            Segment::new(*display, current_agent == id, move |this, window, cx| {
                this.select_env_agent(id, window, cx);
            })
        })
        .collect();
    let agent_display = LAUNCH_AGENTS
        .iter()
        .find(|(id, _)| *id == current_agent)
        .map(|(_, d)| *d)
        .unwrap_or(current_agent);
    rows.push(setting_row_desc_hint(
        "Agent",
        "Which agent's launch environment you are editing.",
        segmented("launch-env-agent", agent_segs, theme, density, typography, cx),
        // The two rows below are scoped to this choice and say nothing about
        // it themselves, which is easy to lose once a profile is selected.
        hint_text(
            format!("The profiles and environment below belong to {agent_display}."),
            theme,
            typography,
        ),
        theme,
        typography,
    ));

    // Which profile of that agent. `default` is the plain entry every config
    // already has, so the list is never empty.
    let profile_names = modal.agent_launch.profile_names(current_agent);
    // `profile_names` always leads with `default`, so "only default" is the
    // empty state — the user has never created a profile for this agent.
    let has_named_profiles = profile_names.len() > 1;

    let mut list = div().flex().flex_col().w_full().gap(px(2.0));
    for name in &profile_names {
        list = list.child(profile_row(
            modal,
            current_agent,
            name,
            theme,
            density,
            typography,
            cx,
        ));
    }
    // The name field is revealed by an action, not permanently open: adding or
    // renaming a profile is rare next to picking one, and a resting text field
    // reads as something the card wants filled in.
    if let (true, Some(state)) =
        (modal.profile_name_mode.is_some(), modal.profile_name_input.as_ref())
    {
        list = list.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .gap(px(6.0))
                .pt(px(4.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(state).small().text_size(px(typography.t_body_sm))),
                )
                .child(value_chip(
                    SharedString::from("launch-env-profile-name-cancel"),
                    "Cancel",
                    theme,
                    density,
                    typography,
                    |this, _w, cx| this.cancel_profile_name(cx),
                    cx,
                )),
        );
    }

    // The last commit's answer, else what the field is asking for, else what
    // the current selection means — `default` in a picker reads as "nothing
    // chosen yet" unless something says otherwise, and with no named profiles
    // this line doubles as the empty state.
    let profile_hint = match (modal.notice_for(NoticeSlot::Profile), &modal.profile_name_mode) {
        (Some(n), _) => notice_text(n.ok, n.text.clone(), theme, typography),
        (None, Some(mode)) => hint_text(mode.prompt(), theme, typography),
        (None, None) => {
            let text: SharedString = match modal.env_profile.as_deref() {
                None if has_named_profiles => format!(
                    "Editing “{DEFAULT_PROFILE}” — the plain configuration this agent already \
                     launches with."
                )
                .into(),
                None => format!(
                    "Editing “{DEFAULT_PROFILE}” — the plain configuration this agent already \
                     launches with. Add a profile to launch it against a second endpoint or \
                     account."
                )
                .into(),
                Some(name) => format!("Editing “{name}” — it launches with its own environment.").into(),
            };
            hint_text(text, theme, typography)
        }
    };

    // `+` opens the name field; while one is open the same slot cancels it, so
    // the affordance that created the field is also the one that dismisses it.
    let add_action = match modal.profile_name_mode {
        Some(_) => icon_button(
            "launch-env-profile-add",
            "icons/x.svg",
            "Cancel",
            false,
            theme,
            density,
            |this, _w, cx| this.cancel_profile_name(cx),
            cx,
        ),
        None => icon_button(
            "launch-env-profile-add",
            "icons/plus.svg",
            "Add a profile",
            false,
            theme,
            density,
            |this, window, cx| this.begin_profile_name(ProfileNameMode::Add, window, cx),
            cx,
        ),
    };

    rows.push(setting_row_action_hint(
        "Profiles",
        "A second configuration of the same agent — an alternate endpoint, a proxy, or a \
         second account. Pick one to edit its environment below.",
        add_action,
        list,
        profile_hint,
        theme,
        typography,
    ));

    // Flags + model for the SELECTED profile. Until now these lived only in the
    // launch card above, which always writes the agent's default entry — so a
    // profile advertised three axes and could only be given one. They sit here,
    // beside the environment they share a selection with, rather than being
    // duplicated per profile row: one selection, one editor.
    let selected_launch = modal
        .agent_launch
        .for_agent_in(current_agent, modal.env_profile.as_deref());
    let sel_args = selected_launch.map(|l| l.args.as_str()).unwrap_or("");
    let sel_model = selected_launch.map(|l| l.model.as_str()).unwrap_or("");

    let mut launch_controls = div().flex().flex_row().items_center().gap(px(6.0));
    // Agents with no approval gate (`yolo_flag` = None) get no chip at all.
    if let Some(flag) = yolo_flag(current_agent) {
        launch_controls = launch_controls.child(toggle_chip(
            SharedString::from("launch-env-yolo"),
            "Skip perms",
            has_flag(sel_args, flag),
            theme,
            density,
            typography,
            move |this, _w, cx| {
                let e = this.selected_launch_mut();
                e.args = toggle_flag(&e.args, flag);
                this.persist_agent_launch(cx);
            },
            cx,
        ));
    }
    launch_controls = launch_controls.child(value_chip(
        SharedString::from("launch-env-model"),
        model_label(sel_model),
        theme,
        density,
        typography,
        move |this, _w, cx| {
            let presets = model_presets(this.env_agent);
            let e = this.selected_launch_mut();
            e.model = cycle_model(presets, &e.model);
            this.persist_agent_launch(cx);
        },
        cx,
    ));

    // With `default` selected these controls and the agent row above address
    // the SAME entry, which is invisible unless said: two controls, one value.
    let launch_hint: SharedString = match modal.env_profile.as_deref() {
        None => format!(
            "Writing to “{DEFAULT_PROFILE}” — the same entry the {agent_display} row above edits."
        )
        .into(),
        Some(name) => format!(
            "Writing to “{name}” only — the {agent_display} row above keeps showing the default."
        )
        .into(),
    };
    rows.push(setting_row_desc_hint(
        "Flags & model",
        "The extra CLI flags and default model this profile launches with.",
        launch_controls,
        hint_text(launch_hint, theme, typography),
        theme,
        typography,
    ));

    // The editor itself. Stacked rather than pinned right: it is the one
    // control in this card that wants the full width of the row.
    if let Some(state) = modal.env_input.as_ref() {
        // Masked by default. The widget's own `masked` mode is single-line
        // only, so hiding swaps the live field for a read-only stand-in rather
        // than trying to make a masked textarea editable — editing text you
        // cannot read is worse than a second click to reveal.
        let body: AnyElement = if modal.env_revealed {
            Input::new(state).text_size(px(typography.t_body_sm)).into_any_element()
        } else {
            let env = modal
                .agent_launch
                .for_agent_in(current_agent, modal.env_profile.as_deref())
                .map(|l| l.env.clone())
                .unwrap_or_default();
            let preview = masked_env_preview(&env);
            div()
                .w_full()
                .min_w_0()
                .px(px(9.0))
                .py(px(7.0))
                .rounded(px(density.r_xs))
                .border_1()
                .border_color(theme.border_inactive)
                .bg(theme.bg_panel_alt)
                .text_size(px(typography.t_body_sm))
                .text_color(if preview.is_empty() { theme.fg_subtle } else { theme.fg_base })
                .child(if preview.is_empty() {
                    SharedString::from("No variables set.")
                } else {
                    SharedString::from(preview)
                })
                .into_any_element()
        };

        let reveal = toggle_chip(
            SharedString::from("launch-env-reveal"),
            if modal.env_revealed { "Hide" } else { "Reveal" },
            modal.env_revealed,
            theme,
            density,
            typography,
            |this, _w, cx| this.toggle_env_reveal(cx),
            cx,
        );

        // The format rule lives under the field, where every reference
        // preferences pane puts it, so the description above can stay one
        // sentence about what the field is *for*. The plaintext warning is not
        // repeated here — it is the card's footer already, and the masking
        // above must not be read as contradicting it: one is about who can
        // read this screen, the other about what is on disk.
        let hint = match modal.notice_for(NoticeSlot::Environment) {
            Some(n) => notice_text(n.ok, n.text.clone(), theme, typography),
            None if modal.env_revealed => {
                hint_text("One KEY=value per line.", theme, typography)
            }
            None => hint_text(
                "Values are hidden on screen. Reveal to read or edit them — they are stored \
                 in plain text either way.",
                theme,
                typography,
            ),
        };
        rows.push(setting_row_action_hint(
            "Environment",
            "Applied on top of the inherited environment at launch, for both terminal and chat.",
            reveal,
            body,
            hint,
            theme,
            typography,
        ));
    }

    card_surface(theme, density, section_card(theme, density, rows))
}


/// One row of the profile list: the profile's name and its résumé on the left,
/// its actions pinned right, and an accent fill when it is the one being
/// edited. Clicking anywhere on the row selects it.
///
/// An action click also selects the row it acted on — the click bubbles from
/// the button to the row, and that is deliberate rather than tolerated: an
/// action names a profile in its confirmation, and the profile it names should
/// be the one the card is visibly pointing at.
///
/// When this row is the one armed for deletion, the résumé is replaced by the
/// consequence and the actions by a two-step confirm. Inline rather than a
/// dialog: the settings modal is already a modal, and a dialog over a dialog is
/// where this codebase's focus bugs live.
#[allow(clippy::too_many_arguments)]
fn profile_row(
    modal: &SettingsModal,
    adapter_id: &'static str,
    name: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let is_default = name == DEFAULT_PROFILE;
    let selected = match (modal.env_profile.as_deref(), is_default) {
        (None, true) => true,
        (Some(sel), false) => sel == name,
        _ => false,
    };
    let armed = modal.pending_profile_delete.as_deref() == Some(name);
    // The selection value this row stands for: `None` is the plain entry.
    let target = (!is_default).then(|| name.to_string());
    let owned = name.to_string();

    let mut name_line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(SharedString::from(owned.clone())),
        );
    if is_default {
        // Named in the row rather than only in the hint below, so the label
        // holds whether or not this row is the selected one.
        name_line = name_line.child(pill("Plain configuration", theme.fg_subtle, density, typography));
    }

    let subtitle: SharedString = if armed {
        format!(
            "Deleting “{owned}” removes its flags, model, and environment. Anything already \
             launched under it falls back to {DEFAULT_PROFILE}."
        )
        .into()
    } else {
        profile_summary(modal, adapter_id, target.as_deref())
    };
    let text = div().child(name_line).child(
        div()
            .text_size(px(typography.t_sub_label))
            .text_color(if armed { theme.status_error } else { theme.fg_subtle })
            .child(subtitle),
    );

    let mut controls = div().flex().flex_row().items_center().gap(px(6.0));
    if armed {
        let confirming = owned.clone();
        controls = controls
            .child(value_chip(
                SharedString::from(format!("launch-env-profile-delete-go-{owned}")),
                "Delete",
                theme,
                density,
                typography,
                move |this, window, cx| this.confirm_profile_delete(&confirming, window, cx),
                cx,
            ))
            .child(value_chip(
                SharedString::from(format!("launch-env-profile-delete-no-{owned}")),
                "Cancel",
                theme,
                density,
                typography,
                |this, _w, cx| this.cancel_profile_delete(cx),
                cx,
            ));
    } else {
        if !is_default {
            let renaming = owned.clone();
            controls = controls.child(icon_button(
                SharedString::from(format!("launch-env-profile-rename-{owned}")),
                "icons/pencil.svg",
                format!("Rename “{owned}”"),
                false,
                theme,
                density,
                move |this, window, cx| {
                    this.begin_profile_name(ProfileNameMode::Rename(renaming.clone()), window, cx);
                },
                cx,
            ));
        }
        // `default` is duplicable and only duplicable: copying it is the one
        // way to start a profile from the plain configuration without editing
        // the plain configuration itself.
        let copying = owned.clone();
        controls = controls.child(icon_button(
            SharedString::from(format!("launch-env-profile-duplicate-{owned}")),
            "icons/copy.svg",
            format!("Duplicate “{owned}”"),
            false,
            theme,
            density,
            move |this, window, cx| {
                this.begin_profile_name(ProfileNameMode::Duplicate(copying.clone()), window, cx);
            },
            cx,
        ));
        if !is_default {
            let deleting = owned.clone();
            controls = controls.child(icon_button(
                SharedString::from(format!("launch-env-profile-delete-{owned}")),
                "icons/trash.svg",
                format!("Delete “{owned}”"),
                true,
                theme,
                density,
                move |this, window, cx| this.arm_profile_delete(deleting.clone(), window, cx),
                cx,
            ));
        }
    }

    list_row(
        SharedString::from(format!("launch-env-profile-{name}")),
        selected,
        text,
        controls,
        theme,
        density,
    )
    .on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _ev, window, cx| {
            this.select_env_profile(target.clone(), window, cx);
        }),
    )
    .into_any_element()
}

/// A small rounded label pill in `color`, used to mark a row's kind.
fn pill(
    text: impl Into<SharedString>,
    color: Hsla,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_chip))
        .bg(Hsla { a: 0.16, ..color })
        .text_size(px(typography.t_sub_label))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// The full launch-defaults card: a "Default agent" row with one icon chip per
/// option, then a rich row per built-in agent (icon tile, name, current flags,
/// and live enabled / skip-permissions / model controls). Wrapped in a card so
/// the cluster reads as its own panel — the carded, icon-led look of a polished
/// preferences pane rather than a flat row list.
pub(super) fn render_launch_card(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::with_capacity(LAUNCH_AGENTS.len() + 2);
    rows.push(setting_row_desc(
        "Status hooks",
        "Show each Claude Code agent's prompt, live tool, and status on the rail and dashboard. On by default.",
        status_hooks_toggle(modal, theme, cx),
        theme,
        typography,
    ));
    rows.push(setting_row_desc(
        "Open new agents as chat",
        "Open a new Claude launch as a structured chat thread instead of a raw terminal. Other agents always open as terminals.",
        default_open_mode_toggle(modal, theme, cx),
        theme,
        typography,
    ));
    rows.push(setting_row_desc(
        "Auto-name chats",
        "Generate a short chat title from your first message via a quick haiku call. On by default; ACP agents use their own titles.",
        auto_title_toggle(modal, theme, cx),
        theme,
        typography,
    ));
    rows.push(setting_row_desc(
        "Default agent",
        "Surfaced first in the launcher.",
        default_agent_chips(modal, theme, density, typography, cx),
        theme,
        typography,
    ));
    for (adapter_id, display) in LAUNCH_AGENTS {
        rows.push(agent_row(
            modal, adapter_id, display, theme, density, typography, cx,
        ));
    }
    card_surface(theme, density, section_card(theme, density, rows))
}

/// The default-agent picker as a row of icon chips: "None" plus one chip per
/// built-in agent (glyph + name). The selected chip is accent-ringed.
fn default_agent_chips(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let current = modal.agent_launch.default_agent.clone();
    let mut row = div().flex().flex_row().items_center().gap(px(6.0));
    row = row.child(choice_chip(
        "launch-default-none",
        "None",
        None,
        current.is_empty(),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.agent_launch.default_agent = String::new();
            this.persist_agent_launch(cx);
        },
        cx,
    ));
    for (adapter_id, display) in LAUNCH_AGENTS {
        let selected = current == adapter_id;
        row = row.child(choice_chip(
            SharedString::from(format!("launch-default-{adapter_id}")),
            display,
            Some(adapter_icon_path(adapter_id)),
            selected,
            theme,
            density,
            typography,
            move |this, _w, cx| {
                this.agent_launch.default_agent = adapter_id.to_string();
                this.persist_agent_launch(cx);
            },
            cx,
        ));
    }
    row.into_any_element()
}

/// The status-hooks toggle: when on, Claude Code launches inject the
/// `--settings` hooks block so each agent reports its live tool to the
/// dashboard card. Global (not per-agent) — Claude-only for now.
fn status_hooks_toggle(
    modal: &SettingsModal,
    theme: Theme,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    toggle_switch(
        "launch-status-hooks",
        modal.agent_launch.status_hooks_enabled,
        theme,
        |this, _w, cx| {
            this.agent_launch.status_hooks_enabled = !this.agent_launch.status_hooks_enabled;
            this.persist_agent_launch(cx);
        },
        cx,
    )
}

/// The auto-title toggle: when on, a new Claude/Codex chat generates a short LLM
/// title from its first message. Global; ACP chats always use their native title.
fn auto_title_toggle(
    modal: &SettingsModal,
    theme: Theme,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    toggle_switch(
        "launch-auto-title",
        modal.agent_launch.auto_title_enabled,
        theme,
        |this, _w, cx| {
            this.agent_launch.auto_title_enabled = !this.agent_launch.auto_title_enabled;
            this.persist_agent_launch(cx);
        },
        cx,
    )
}

/// The default-open-mode toggle: when on, a new Claude launch opens as a
/// structured chat thread instead of a raw-PTY terminal. Global and Claude-only
/// (other adapters always open as terminals).
fn default_open_mode_toggle(
    modal: &SettingsModal,
    theme: Theme,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    toggle_switch(
        "launch-open-mode-chat",
        modal.agent_launch.default_open_mode == OpenMode::Chat,
        theme,
        |this, _w, cx| {
            this.agent_launch.default_open_mode = match this.agent_launch.default_open_mode {
                OpenMode::Chat => OpenMode::Terminal,
                OpenMode::Terminal => OpenMode::Chat,
            };
            this.persist_agent_launch(cx);
        },
        cx,
    )
}

/// One selectable icon chip used by the default-agent picker. Accent-ringed
/// (info-blue fill + border) when `selected`, muted otherwise.
#[allow(clippy::too_many_arguments)]
fn choice_chip(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    icon: Option<&'static str>,
    selected: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_click: impl Fn(&mut SettingsModal, &mut Window, &mut gpui::Context<SettingsModal>) + 'static,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let icon_color = if selected {
        theme.status_info
    } else {
        theme.fg_muted
    };
    div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h(px(density.h_overlay_item))
        .px(px(9.0))
        .rounded(px(density.r_chip))
        .border_1()
        .text_size(px(typography.t_body_sm))
        .cursor_pointer()
        .when(selected, |s| {
            s.bg(Hsla { a: 0.14, ..theme.status_info })
                .border_color(theme.status_info)
                .text_color(theme.fg_base)
        })
        .when(!selected, |s| {
            s.bg(theme.bg_panel_alt)
                .border_color(theme.border_inactive)
                .text_color(theme.fg_muted)
                .hover(|h| h.border_color(theme.border_active).text_color(theme.fg_base))
        })
        .when_some(icon, |c, path| {
            c.child(
                svg()
                    .path(path)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(icon_color),
            )
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, window, cx| on_click(this, window, cx)),
        )
        .child(label.into())
        .into_any_element()
}

/// A rich per-agent row: an icon tile + name (+ a "Default" badge when this is
/// the default agent) + the current flags/model summary on the left, and the
/// live skip-permissions / model / enabled controls pinned right. The identity
/// (tile + text) dims when the agent is disabled; the controls stay lit so the
/// row can be re-enabled and pre-configured.
#[allow(clippy::too_many_arguments)]
fn agent_row(
    modal: &SettingsModal,
    adapter_id: &'static str,
    display: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let launch = modal.agent_launch.for_agent(adapter_id);
    let disabled = launch.map(|l| l.disabled).unwrap_or(false);
    let args = launch.map(|l| l.args.as_str()).unwrap_or("");
    let model = launch.map(|l| l.model.as_str()).unwrap_or("");
    let is_default =
        !modal.agent_launch.default_agent.is_empty() && modal.agent_launch.default_agent == adapter_id;

    let tile = div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(30.0))
        .flex_none()
        .rounded(px(density.r_xs))
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive)
        .child(
            svg()
                .path(adapter_icon_path(adapter_id))
                .size(px(17.0))
                .text_color(theme.fg_base),
        );

    let mut name_line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_base)
                .child(display),
        );
    if is_default {
        name_line = name_line.child(default_badge(theme, density, typography));
    }

    let info = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .child(name_line)
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(agent_summary(modal, adapter_id)),
        );

    let identity = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .flex_1()
        .min_w(px(0.0))
        .when(disabled, |s| s.opacity(0.5))
        .child(tile)
        .child(info);

    // Agents with no approval gate (yolo_flag = None) get no chip at all.
    let yolo_chip = yolo_flag(adapter_id).map(|flag| {
        toggle_chip(
            SharedString::from(format!("launch-yolo-{adapter_id}")),
            "Skip perms",
            has_flag(args, flag),
            theme,
            density,
            typography,
            move |this, _w, cx| {
                let e = this.agent_launch.entry_mut(adapter_id);
                e.args = toggle_flag(&e.args, flag);
                this.persist_agent_launch(cx);
            },
            cx,
        )
    });
    let model_chip = value_chip(
        SharedString::from(format!("launch-model-{adapter_id}")),
        model_label(model),
        theme,
        density,
        typography,
        move |this, _w, cx| {
            let presets = model_presets(adapter_id);
            let e = this.agent_launch.entry_mut(adapter_id);
            e.model = cycle_model(presets, &e.model);
            this.persist_agent_launch(cx);
        },
        cx,
    );
    let enabled_toggle = toggle_switch(
        SharedString::from(format!("launch-enabled-{adapter_id}")),
        !disabled,
        theme,
        move |this, _w, cx| {
            let e = this.agent_launch.entry_mut(adapter_id);
            e.disabled = !e.disabled;
            this.persist_agent_launch(cx);
        },
        cx,
    );

    let controls = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .flex_none()
        .children(yolo_chip)
        .child(model_chip)
        .child(enabled_toggle);

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .w_full()
        .py(px(10.0))
        .child(identity)
        .child(controls)
        .into_any_element()
}

/// A small accent pill marking the configured default agent.
fn default_badge(theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    pill("Default", theme.status_info, density, typography)
}

/// Segmented picker: None + one segment per built-in agent.
fn default_agent_control(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let current = modal.agent_launch.default_agent.clone();
    let mut segs = vec![Segment::new("None", current.is_empty(), |this, _w, cx| {
        this.agent_launch.default_agent = String::new();
        this.persist_agent_launch(cx);
    })];
    for (adapter_id, display) in LAUNCH_AGENTS {
        let selected = current == adapter_id;
        segs.push(Segment::new(display, selected, move |this, _w, cx| {
            this.agent_launch.default_agent = adapter_id.to_string();
            this.persist_agent_launch(cx);
        }));
    }
    segmented("launch-default-agent", segs, theme, density, typography, cx)
}

/// The three live chips for one agent: enabled, skip-permissions, model.
fn agent_control(
    modal: &SettingsModal,
    adapter_id: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let launch = modal.agent_launch.for_agent(adapter_id);
    let disabled = launch.map(|l| l.disabled).unwrap_or(false);
    let args = launch.map(|l| l.args.as_str()).unwrap_or("");
    let model = launch.map(|l| l.model.as_str()).unwrap_or("");

    let enabled_chip = value_chip(
        SharedString::from(format!("launch-enabled-{adapter_id}")),
        if disabled { "Disabled" } else { "Enabled" },
        theme,
        density,
        typography,
        move |this, _w, cx| {
            let e = this.agent_launch.entry_mut(adapter_id);
            e.disabled = !e.disabled;
            this.persist_agent_launch(cx);
        },
        cx,
    );
    // Agents with no approval gate (yolo_flag = None) get no chip at all.
    let yolo_chip = yolo_flag(adapter_id).map(|flag| {
        value_chip(
            SharedString::from(format!("launch-yolo-{adapter_id}")),
            if has_flag(args, flag) {
                "Skip perms: On"
            } else {
                "Skip perms: Off"
            },
            theme,
            density,
            typography,
            move |this, _w, cx| {
                let e = this.agent_launch.entry_mut(adapter_id);
                e.args = toggle_flag(&e.args, flag);
                this.persist_agent_launch(cx);
            },
            cx,
        )
    });
    let model_chip = value_chip(
        SharedString::from(format!("launch-model-{adapter_id}")),
        model_label(model),
        theme,
        density,
        typography,
        move |this, _w, cx| {
            let presets = model_presets(adapter_id);
            let e = this.agent_launch.entry_mut(adapter_id);
            e.model = cycle_model(presets, &e.model);
            this.persist_agent_launch(cx);
        },
        cx,
    );

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(enabled_chip)
        .children(yolo_chip)
        .child(model_chip)
        .into_any_element()
}

/// One-line description of an agent's current launch config (also the search
/// haystack), e.g. "Flags: --dangerously-skip-permissions · model opus".
fn agent_summary(modal: &SettingsModal, adapter_id: &str) -> SharedString {
    let Some(launch) = modal.agent_launch.for_agent(adapter_id) else {
        return NO_OVERRIDES.into();
    };
    if launch.disabled {
        return "Hidden from the launcher.".into();
    }
    let mut parts = summary_parts(Some(launch));
    // This row describes the agent's DEFAULT entry, and once a profile can
    // diverge from it (flags and model, not just environment) the row would be
    // over-claiming without saying so. The count both admits the subtitle is
    // partial and makes profiles discoverable from the card users read first.
    // Omitted entirely at zero: most agents have none, and "0 profiles" is
    // noise on every one of them.
    //
    // `profile_names` always leads with `default`, which is this entry.
    match modal.agent_launch.profile_names(adapter_id).len().saturating_sub(1) {
        0 => {}
        1 => parts.push("1 profile".to_string()),
        n => parts.push(format!("{n} profiles")),
    }
    joined(parts)
}

/// What both résumés say when a configuration overrides nothing at all.
const NO_OVERRIDES: &str = "Launches with defaults.";

/// The one place a launch configuration is put into words.
///
/// Both surfaces that describe one — the agent rows in the launch card and the
/// profile rows in this card — read from here, so "flags … · model … · N
/// variables" means the same thing in both and cannot drift into two spellings
/// of the same sentence. Each caller appends the one term only it can know
/// (the agent row a profile count, a profile row how it diverges) rather than
/// growing its own copy of the shared three.
fn summary_parts(launch: Option<&PerAgentLaunch>) -> Vec<String> {
    let Some(l) = launch else {
        return Vec::new();
    };
    let mut parts: Vec<String> = Vec::new();
    if !l.args.trim().is_empty() {
        parts.push(format!("flags {}", l.args.trim()));
    }
    if !l.model.trim().is_empty() {
        parts.push(format!("model {}", l.model.trim()));
    }
    // Counted the way resolution counts them: a blank key and a reserved key
    // never reach a spawn, so neither may be advertised as a variable. The
    // count is a promise about what will be applied, not about how many lines
    // were typed.
    match l
        .env
        .keys()
        .filter(|k| !k.trim().is_empty() && !oximux_settings::is_reserved_env_key(k))
        .count()
    {
        0 => {}
        1 => parts.push("1 variable".to_string()),
        n => parts.push(format!("{n} variables")),
    }
    parts
}

/// Join a résumé's terms, or say that there are none.
fn joined(parts: Vec<String>) -> SharedString {
    if parts.is_empty() {
        NO_OVERRIDES.into()
    } else {
        SharedString::from(parts.join(" · "))
    }
}

/// One profile's résumé. `None` is the adapter's plain entry (`default`).
///
/// A named profile also says which axis it diverges on. Now that flags and
/// model are editable per profile, two rows can differ in a way neither
/// résumé's own terms make obvious — "model opus" above "model sonnet" is a
/// comparison the reader has to perform. Environment is excluded: varying it
/// is the whole reason a profile exists, so naming it as a divergence would
/// mark every profile.
fn profile_summary(modal: &SettingsModal, adapter_id: &str, profile: Option<&str>) -> SharedString {
    let launch = modal.agent_launch.for_agent_in(adapter_id, profile);
    let mut parts = summary_parts(launch);
    if profile.is_some()
        && let Some(term) = divergence(modal.agent_launch.for_agent(adapter_id), launch)
    {
        parts.push(term.to_string());
    }
    joined(parts)
}

/// How `profile` differs from the agent's `default` entry, on the two axes a
/// profile can now override besides environment. `None` when it matches.
fn divergence(default: Option<&PerAgentLaunch>, profile: Option<&PerAgentLaunch>) -> Option<&'static str> {
    let (Some(d), Some(p)) = (default, profile) else {
        return None;
    };
    match (d.args.trim() != p.args.trim(), d.model.trim() != p.model.trim()) {
        (true, true) => Some("own flags and model"),
        (true, false) => Some("own flags"),
        (false, true) => Some("own model"),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared formatter, exercised the way both callers use it.
    fn summarize(launch: Option<&PerAgentLaunch>) -> SharedString {
        joined(summary_parts(launch))
    }

    #[test]
    fn summarize_puts_a_configuration_into_words() {
        // Nothing set reads as the same sentence for an agent row and a
        // profile row, because it is literally the same sentence.
        assert_eq!(summarize(None), NO_OVERRIDES);
        assert_eq!(summarize(Some(&PerAgentLaunch::default())), NO_OVERRIDES);

        let mut l = PerAgentLaunch { args: " --verbose ".into(), model: " opus ".into(), ..Default::default() };
        assert_eq!(summarize(Some(&l)), "flags --verbose · model opus");

        l.env.insert("A".into(), "1".into());
        assert_eq!(summarize(Some(&l)), "flags --verbose · model opus · 1 variable");
        l.env.insert("B".into(), "2".into());
        assert_eq!(summarize(Some(&l)), "flags --verbose · model opus · 2 variables");
        // Counted the way resolution counts: neither a blank key nor a
        // reserved one reaches a spawn, so neither is advertised as a variable.
        l.env.insert("   ".into(), "orphan".into());
        l.env.insert("PATH".into(), "/nowhere".into());
        assert_eq!(summarize(Some(&l)), "flags --verbose · model opus · 2 variables");

        // Env alone is enough to have something to say.
        let env_only = PerAgentLaunch {
            env: [("K".to_string(), "v".to_string())].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(summarize(Some(&env_only)), "1 variable");
    }

    #[test]
    fn a_divergent_profile_says_which_axis_it_diverges_on() {
        let default = PerAgentLaunch {
            args: "--verbose".into(),
            model: "opus".into(),
            ..Default::default()
        };
        // Identical on both axes: nothing to say. Environment is deliberately
        // not compared — varying it is why a profile exists.
        let mut same = default.clone();
        same.env.insert("K".into(), "v".into());
        assert_eq!(divergence(Some(&default), Some(&same)), None);

        let flags = PerAgentLaunch { args: "--quiet".into(), ..default.clone() };
        assert_eq!(divergence(Some(&default), Some(&flags)), Some("own flags"));

        let model = PerAgentLaunch { model: "haiku".into(), ..default.clone() };
        assert_eq!(divergence(Some(&default), Some(&model)), Some("own model"));

        let both = PerAgentLaunch { args: String::new(), model: String::new(), ..default.clone() };
        assert_eq!(divergence(Some(&default), Some(&both)), Some("own flags and model"));

        // Whitespace is not a divergence: the same value typed with a trailing
        // space would otherwise mark a profile as differing forever.
        let padded = PerAgentLaunch { args: "  --verbose  ".into(), ..default.clone() };
        assert_eq!(divergence(Some(&default), Some(&padded)), None);

        // An agent with no default entry at all has nothing to diverge from.
        assert_eq!(divergence(None, Some(&flags)), None);
    }

    #[test]
    fn a_name_field_opens_with_what_it_is_asking_for() {
        assert_eq!(ProfileNameMode::Add.seed(), "");
        // A rename starts from the current name because a rename is usually an
        // edit of it; a duplicate offers a name that already validates, so
        // Enter alone is a complete answer.
        assert_eq!(ProfileNameMode::Rename("proxy".into()).seed(), "proxy");
        assert_eq!(ProfileNameMode::Duplicate("proxy".into()).seed(), "proxy-copy");

        // Each mode asks a different question, and each names the profile it
        // is about.
        let prompts = [
            ProfileNameMode::Add.prompt().to_string(),
            ProfileNameMode::Rename("proxy".into()).prompt().to_string(),
            ProfileNameMode::Duplicate("proxy".into()).prompt().to_string(),
        ];
        assert!(prompts[1].contains("proxy") && prompts[2].contains("proxy"));
        let mut sorted = prompts.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "the three prompts must be distinguishable");
    }

    #[test]
    fn yolo_flag_is_per_agent() {
        assert_eq!(yolo_flag("claude-code"), Some("--dangerously-skip-permissions"));
        assert_eq!(
            yolo_flag("codex"),
            Some("--dangerously-bypass-approvals-and-sandbox")
        );
        assert_eq!(yolo_flag("pi"), None, "pi has no approval gate, so no chip");
        // F8 locks: the wildcard arms fall back to Claude's flag and Claude's
        // model list — either leaking to omp would hand it a flag it rejects
        // at parse, or a model id its resolver fuzzy-matches into the wrong
        // provider.
        assert_eq!(yolo_flag("omp"), Some("--approval-mode yolo"));
        assert_eq!(model_presets("omp"), &[""], "omp's catalog is live; only Default is safe");
    }

    #[test]
    fn omps_two_token_yolo_flag_round_trips_through_the_chip() {
        // `--approval-mode yolo` is two tokens; after a toggle it lives in the
        // free-text args as two tokens, and the chip must still read it as ON
        // and toggle it back OFF cleanly.
        let flag = yolo_flag("omp").unwrap();
        let on = toggle_flag("", flag);
        assert_eq!(on, "--approval-mode yolo");
        assert!(has_flag(&on, flag), "the chip must read its own writes");
        let off = toggle_flag(&on, flag);
        assert_eq!(off, "");
        // Other user tokens survive both directions.
        let mixed = toggle_flag("--no-color", flag);
        assert!(has_flag(&mixed, flag));
        assert_eq!(toggle_flag(&mixed, flag), "--no-color");
    }

    #[test]
    fn toggling_yolo_on_replaces_a_conflicting_approval_mode() {
        // A user's hand-edited TOML can already carry a different mode. The
        // chip reads OFF then (correct — it is not yolo), but toggling ON must
        // REPLACE that pair, not append a second `--approval-mode` — an argv
        // that says two things is wrong even though omp resolves it last-wins.
        let flag = yolo_flag("omp").unwrap();
        let on = toggle_flag("--approval-mode always-ask", flag);
        assert_eq!(on, "--approval-mode yolo", "the conflicting pair must be replaced");
        // Unrelated tokens around the conflict survive.
        let on = toggle_flag("--no-color --approval-mode write -v", flag);
        assert_eq!(on, "--no-color -v --approval-mode yolo");
        // And OFF still restores a clean string.
        assert_eq!(toggle_flag(&on, flag), "--no-color -v");
    }

    #[test]
    fn toggle_flag_adds_then_removes() {
        let f = "--dangerously-skip-permissions";
        let on = toggle_flag("", f);
        assert_eq!(on, f);
        let off = toggle_flag(&on, f);
        assert_eq!(off, "");
    }

    #[test]
    fn toggle_flag_preserves_other_args() {
        let f = "--yes-always";
        // Adding keeps the existing flag; removing leaves only it.
        let on = toggle_flag("--model sonnet", f);
        assert!(has_flag(&on, f));
        assert!(has_flag(&on, "--model"));
        let off = toggle_flag(&on, f);
        assert_eq!(off, "--model sonnet");
    }

    #[test]
    fn cycle_model_walks_presets_and_wraps() {
        let p = model_presets("claude-code"); // ["", opus, sonnet, haiku]
        assert_eq!(cycle_model(p, ""), "opus");
        assert_eq!(cycle_model(p, "haiku"), ""); // wraps to Default
    }

    #[test]
    fn cycle_model_anchors_custom_value() {
        let p = model_presets("codex");
        // A hand-set model not in presets is anchored, so cycling moves on
        // and is reachable again by wrapping.
        assert_eq!(cycle_model(p, "my-model"), "");
    }

    #[test]
    fn model_label_maps_empty_to_default() {
        assert_eq!(model_label(""), "Model: Default");
        assert_eq!(model_label("opus"), "Model: opus");
    }

    #[test]
    fn env_lines_round_trip_and_stay_sorted() {
        let raw = "ZED_LAST=z\nANTHROPIC_BASE_URL=https://proxy.internal/v1\n";
        let env = parse_env_lines(raw);
        // Re-rendered in key order, not entry order — a reopen must not shuffle
        // the user's file.
        assert_eq!(
            format_env_lines(&env),
            "ANTHROPIC_BASE_URL=https://proxy.internal/v1\nZED_LAST=z"
        );
        assert_eq!(parse_env_lines(&format_env_lines(&env)), env);
    }

    #[test]
    fn env_lines_split_on_the_first_equals_only() {
        // A base URL with a query string, or a padded token, must survive whole.
        let env = parse_env_lines("URL=https://h/v1?a=1&b=2\n  TOKEN = sk-abc=def==  ");
        assert_eq!(env.get("URL").map(String::as_str), Some("https://h/v1?a=1&b=2"));
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("sk-abc=def=="));
    }

    #[test]
    fn env_lines_drop_blanks_comments_and_half_typed_rows() {
        // A line still being typed must not become a variable named after a
        // fragment, and an annotated file must survive a round-trip of editing.
        let env = parse_env_lines(
            "\n# a comment\n   \nJUST_A_KEY\n=orphan\n   =orphan2\nGOOD=1\n",
        );
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("GOOD").map(String::as_str), Some("1"));
    }

    #[test]
    fn every_unusable_line_is_reported_instead_of_dropped() {
        // The same input as `env_lines_drop_blanks_comments_and_half_typed_rows`,
        // read through the diagnostic half: the map is identical, and each
        // dropped line now has a message naming which line it was.
        let (env, rejects) =
            parse_env_draft("\n# a comment\n   \nJUST_A_KEY\n=orphan\n   =orphan2\nGOOD=1\n");
        assert_eq!(env.len(), 1);
        assert_eq!(
            rejects,
            vec![
                EnvReject::NoAssignment { line: 4 },
                EnvReject::BlankKey { line: 5 },
                EnvReject::BlankKey { line: 6 },
            ],
            "blank lines and comments are annotation, not failed input",
        );
        // Every message names its line, because the draft is free text and
        // "one of your lines is wrong" is not actionable.
        for r in &rejects {
            assert!(r.message().contains("Line "), "{}", r.message());
        }
    }

    #[test]
    fn a_reserved_key_is_reported_but_kept_in_the_draft() {
        let (env, rejects) = parse_env_draft("PATH=/nowhere\nANTHROPIC_BASE_URL=https://p/v1");
        assert_eq!(
            rejects,
            vec![EnvReject::Reserved { line: 1, key: "PATH".to_string() }],
        );
        // Kept, so reopening the pane does not silently delete the line the
        // user typed. Resolution is what refuses to apply it.
        assert_eq!(env.len(), 2);
        assert!(oximux_settings::is_reserved_env_key("PATH"));
    }

    #[test]
    fn one_message_covers_however_many_lines_are_wrong() {
        assert_eq!(reject_message(&[]), None);
        let one = reject_message(&[EnvReject::NoAssignment { line: 2 }]).expect("a message");
        assert!(one.contains("Line 2") && !one.contains("more"));
        let many = reject_message(&[
            EnvReject::NoAssignment { line: 2 },
            EnvReject::BlankKey { line: 5 },
            EnvReject::Reserved { line: 7, key: "HOME".into() },
        ])
        .expect("a message");
        assert!(many.contains("Line 2") && many.contains("(+2 more)"), "{many}");
    }

    #[test]
    fn the_mask_shows_the_keys_and_never_the_value_or_its_length() {
        let env: BTreeMap<String, String> = [
            ("SHORT".to_string(), "a".to_string()),
            ("LONG".to_string(), "a".repeat(200)),
            ("UNSET".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        let preview = masked_env_preview(&env);
        // Fixed width: a mask that tracked length would leak how long a token
        // is, which is itself a hint.
        assert_eq!(preview, "LONG=••••••••\nSHORT=••••••••\nUNSET=");
        assert!(!preview.contains('a'), "no fragment of any value survives");
        assert!(masked_env_preview(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn no_placeholder_demonstrates_a_key_the_field_would_refuse() {
        // The worked examples teach the format. One of them naming a reserved
        // key would teach a line that is reported the moment it is committed.
        for agent in ["claude-code", "codex", "pi", "omp", "something-new"] {
            for line in env_placeholder(agent).lines() {
                let Some((key, _)) = line.split_once('=') else { continue };
                assert!(
                    !oximux_settings::is_reserved_env_key(key),
                    "{agent}'s placeholder demonstrates the reserved key {key}",
                );
            }
        }
    }

    #[test]
    fn env_lines_keep_an_empty_value() {
        // `KEY=` is a deliberate act: it sets the variable to empty, which is
        // how a user unsets an inherited one for the child.
        let env = parse_env_lines("KEY=");
        assert_eq!(env.get("KEY").map(String::as_str), Some(""));
        assert_eq!(format_env_lines(&env), "KEY=");
    }

    #[test]
    fn empty_env_formats_to_empty_text() {
        assert_eq!(format_env_lines(&BTreeMap::new()), "");
        assert!(parse_env_lines("").is_empty());
    }

    /// The three rejections must be DISTINCT messages, not one generic refusal:
    /// each was previously a silent early return (or, for a duplicate, a
    /// re-selection that read as a dead button), and the whole point of the
    /// phase is that the user learns which one happened.
    #[test]
    fn a_profile_name_is_rejected_with_the_reason_it_failed() {
        let existing = vec![DEFAULT_PROFILE.to_string(), "proxy".to_string()];

        let blank = validate_profile_name("   ", &existing).unwrap_err();
        assert!(blank.contains("Type a name"), "blank said: {blank}");

        let reserved = validate_profile_name(" default ", &existing).unwrap_err();
        assert!(
            reserved.contains("plain configuration"),
            "`default` said: {reserved}"
        );

        let dupe = validate_profile_name("proxy", &existing).unwrap_err();
        assert!(dupe.contains("already has"), "duplicate said: {dupe}");

        // All three differ, so the message identifies the fault.
        assert_ne!(blank, reserved);
        assert_ne!(reserved, dupe);
        assert_ne!(blank, dupe);
    }

    #[test]
    fn a_valid_profile_name_is_trimmed_and_accepted() {
        let existing = vec![DEFAULT_PROFILE.to_string()];
        assert_eq!(validate_profile_name("  staging  ", &existing), Ok("staging".into()));
        // Case is NOT folded: `for_agent_in` matches exactly, so "Proxy" and
        // "proxy" really are two reachable profiles and refusing one would be
        // stricter than resolution.
        let existing = vec![DEFAULT_PROFILE.to_string(), "proxy".to_string()];
        assert!(validate_profile_name("Proxy", &existing).is_ok());
    }

    /// The placeholder teaches the field's shape, so it must be multi-line and
    /// must not demonstrate another vendor's variables in this agent's editor.
    #[test]
    fn the_env_placeholder_is_a_worked_example_per_agent() {
        for id in LAUNCH_AGENTS.map(|(id, _)| id) {
            let p = env_placeholder(id);
            assert!(
                p.lines().count() >= 3,
                "{id}'s placeholder teaches only {} line(s)",
                p.lines().count()
            );
            // Every non-comment line has to parse, or the example contradicts
            // the format rule printed directly beneath it.
            assert_eq!(
                parse_env_lines(p).len(),
                p.lines().filter(|l| !l.trim().starts_with('#')).count(),
                "{id}'s placeholder shows a line the field would drop"
            );
        }
        assert!(
            !env_placeholder("codex").contains("ANTHROPIC"),
            "Codex's editor must not teach Anthropic variables"
        );
        assert!(env_placeholder("claude-code").contains("ANTHROPIC_BASE_URL"));
    }
}
