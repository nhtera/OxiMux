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

use oximux_settings::{DEFAULT_PROFILE, Density, OpenMode, Theme, Typography, split_args};

use super::SettingsModal;
use super::controls::{toggle_chip, toggle_switch, value_chip};
use super::layout::{
    SettingEntry, card_surface, entry, hint_text, notice_text, section_card, setting_row_desc,
    setting_row_desc_hint, setting_row_hint,
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
/// Parse the environment editor's text into the map [`PerAgentLaunch::env`]
/// holds. One `KEY=value` per line, `.env`-shaped because that is the form a
/// proxy or alternate-endpoint configuration is already pasted from.
///
/// - The FIRST `=` splits; a value may contain further `=` (a URL query, a
///   base64 token) without quoting.
/// - Blank lines and `#` comments are skipped, so a user can annotate.
/// - A line with no `=`, or an empty key, is dropped rather than guessed at —
///   a half-typed line must not become a variable named after a fragment.
/// - Key and value are both trimmed. A trailing space in a key is a *different*
///   variable than the one the user meant, which is the failure this prevents.
///
/// [`PerAgentLaunch::env`]: oximux_settings::PerAgentLaunch::env
pub(super) fn parse_env_lines(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        out.insert(k.to_string(), v.trim().to_string());
    }
    out
}

/// Render an env map back into editor text: one `KEY=value` per line, in the
/// map's key order (it is a `BTreeMap`, so sorted and stable across reopens).
pub(super) fn format_env_lines(env: &BTreeMap<String, String>) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n")
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
    rows.push(setting_row_desc(
        "Agent",
        "Which agent's launch environment you are editing.",
        segmented("launch-env-agent", agent_segs, theme, density, typography, cx),
        theme,
        typography,
    ));

    // Which profile of that agent. `default` is the plain entry every config
    // already has, so this reads "default" even before a profile is created.
    let current_profile = modal.env_profile.clone();
    let profile_names = modal.agent_launch.profile_names(current_agent);
    // `profile_names` always leads with `default`, so "only default" is the
    // empty state — the user has never created a profile for this agent.
    let has_named_profiles = profile_names.len() > 1;
    let profile_segs: Vec<Segment> = profile_names
        .clone()
        .into_iter()
        .map(|name| {
            let selected = match (&current_profile, name.as_str()) {
                (None, DEFAULT_PROFILE) => true,
                (Some(sel), n) => sel == n,
                _ => false,
            };
            let target = (name != DEFAULT_PROFILE).then(|| name.clone());
            Segment::new(name, selected, move |this, window, cx| {
                this.select_env_profile(target.clone(), window, cx);
            })
        })
        .collect();
    // What the current selection means, stated rather than left to be inferred
    // — `default` in a picker reads as "nothing chosen yet" unless something
    // says otherwise. When the agent has no named profiles this line doubles as
    // the empty state, naming what a profile would be *for*.
    let profile_hint: SharedString = match current_profile.as_deref() {
        None if has_named_profiles => {
            format!("“{DEFAULT_PROFILE}” is the plain configuration this agent already launches with.")
                .into()
        }
        None => format!(
            "“{DEFAULT_PROFILE}” is the plain configuration this agent already launches with. \
             No profiles yet — add one to launch this agent against a second endpoint or account."
        )
        .into(),
        Some(name) => {
            format!("“{name}” launches this agent with its own environment.").into()
        }
    };
    rows.push(setting_row_desc_hint(
        "Profile",
        "A second configuration of the same agent — an alternate endpoint, a proxy, or a \
         second account.",
        segmented("launch-env-profile", profile_segs, theme, density, typography, cx),
        hint_text(profile_hint, theme, typography),
        theme,
        typography,
    ));

    // Add / remove. The name field commits on Enter (see the modal's
    // `_new_profile_sub`); Remove is disabled on `default`, which is the
    // adapter's plain entry rather than a profile.
    if let Some(state) = modal.new_profile_input.as_ref() {
        let mut controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .w(px(180.0))
                    .child(Input::new(state).small().text_size(px(typography.t_body_sm))),
            );
        if current_profile.is_some() {
            controls = controls.child(value_chip(
                SharedString::from("launch-env-profile-remove"),
                "Remove",
                theme,
                density,
                typography,
                |this, window, cx| this.remove_env_profile(window, cx),
                cx,
            ));
        }
        // The create attempt's answer, or the format rule while there is none.
        let hint = match modal.notice_for(NoticeSlot::Profile) {
            Some(n) => notice_text(n.ok, n.text.clone(), theme, typography),
            None => hint_text("Type a name and press Enter.", theme, typography),
        };
        rows.push(setting_row_desc_hint(
            "Add a profile",
            "A new profile starts from this agent's current flags, model, and environment.",
            controls,
            hint,
            theme,
            typography,
        ));
    }

    // The editor itself. Stacked rather than pinned right: it is the one
    // control in this card that wants the full width of the row.
    if let Some(state) = modal.env_input.as_ref() {
        // The format rule lives under the field, where every reference
        // preferences pane puts it, so the description above can stay one
        // sentence about what the field is *for*. The plaintext warning is not
        // repeated here — it is the card's footer already.
        let hint = match modal.notice_for(NoticeSlot::Environment) {
            Some(n) => notice_text(n.ok, n.text.clone(), theme, typography),
            None => hint_text("One KEY=value per line.", theme, typography),
        };
        rows.push(setting_row_hint(
            "Environment",
            "Applied on top of the inherited environment at launch, for both terminal and chat.",
            Input::new(state).text_size(px(typography.t_body_sm)),
            hint,
            theme,
            typography,
        ));
    }

    card_surface(theme, density, section_card(theme, density, rows))
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
    div()
        .flex()
        .items_center()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_chip))
        .bg(Hsla { a: 0.16, ..theme.status_info })
        .text_size(px(typography.t_sub_label))
        .text_color(theme.status_info)
        .child("Default")
        .into_any_element()
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
        return "Launches with defaults.".into();
    };
    if launch.disabled {
        return "Hidden from the launcher.".into();
    }
    let mut parts: Vec<String> = Vec::new();
    if !launch.args.trim().is_empty() {
        parts.push(format!("flags {}", launch.args.trim()));
    }
    if !launch.model.trim().is_empty() {
        parts.push(format!("model {}", launch.model.trim()));
    }
    if parts.is_empty() {
        "Launches with defaults.".into()
    } else {
        SharedString::from(parts.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
