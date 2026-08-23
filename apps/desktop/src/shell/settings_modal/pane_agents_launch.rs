//! Agent-launch section of the Agents pane — edits the `AgentLaunchSettings`
//! working copy (default agent, per-agent enabled / skip-permissions / model).
//! Applies immediately: mutate the copy, write `agent_launch.toml`, watcher
//! reloads + swaps the global. These are the defaults the one-click launcher
//! applies when the user picks an agent.

use gpui::{
    AnyElement, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px, svg,
};
use oximux_settings::{Density, OpenMode, Theme, Typography, split_args};

use super::SettingsModal;
use super::controls::{toggle_chip, toggle_switch, value_chip};
use super::layout::{SettingEntry, card_surface, entry, section_card, setting_row_desc};
use super::segmented::{Segment, segmented};
use crate::shell::agent_presentation::adapter_icon_path;

/// Built-in agents exposed in the launch settings, in picker order. The
/// custom adapter is excluded — its command is fully user-supplied, so a
/// "default flags" override has no meaning.
pub(super) const LAUNCH_AGENTS: [(&str, &str); 3] = [
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("aider", "Aider"),
];

/// The skip-permissions ("YOLO") flag for an agent — toggled in and out of
/// that agent's free-text args by the skip-perms chip. Each CLI spells it
/// differently; an unknown id falls back to the most common form.
fn yolo_flag(adapter_id: &str) -> &'static str {
    match adapter_id {
        "codex" => "--dangerously-bypass-approvals-and-sandbox",
        "aider" => "--yes-always",
        // claude-code and any future addition default to claude's spelling.
        _ => "--dangerously-skip-permissions",
    }
}

/// Model presets the per-agent model chip cycles through. The leading empty
/// string is the "Default" option (no `--model` flag). Mirrors the slugs the
/// adapters accept.
fn model_presets(adapter_id: &str) -> &'static [&'static str] {
    match adapter_id {
        "codex" => &["", "gpt-5-codex", "o3"],
        "aider" => &["", "sonnet", "opus", "gpt-5.5"],
        _ => &["", "opus", "sonnet", "haiku"],
    }
}

/// Whether `args` already contains `flag` as a standalone token.
fn has_flag(args: &str, flag: &str) -> bool {
    split_args(args).iter().any(|t| t == flag)
}

/// Add or remove `flag` from a free-text args string, preserving any other
/// tokens the user configured. Re-joined with single spaces.
fn toggle_flag(args: &str, flag: &str) -> String {
    let mut toks = split_args(args);
    let had = toks.iter().any(|t| t == flag);
    toks.retain(|t| t != flag);
    if !had {
        toks.push(flag.to_string());
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
    let yolo_on = has_flag(args, yolo_flag(adapter_id));
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

    let yolo_chip = toggle_chip(
        SharedString::from(format!("launch-yolo-{adapter_id}")),
        "Skip perms",
        yolo_on,
        theme,
        density,
        typography,
        move |this, _w, cx| {
            let flag = yolo_flag(adapter_id);
            let e = this.agent_launch.entry_mut(adapter_id);
            e.args = toggle_flag(&e.args, flag);
            this.persist_agent_launch(cx);
        },
        cx,
    );
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
        .child(yolo_chip)
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
    let yolo_on = has_flag(args, yolo_flag(adapter_id));

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
    let yolo_chip = value_chip(
        SharedString::from(format!("launch-yolo-{adapter_id}")),
        if yolo_on {
            "Skip perms: On"
        } else {
            "Skip perms: Off"
        },
        theme,
        density,
        typography,
        move |this, _w, cx| {
            let flag = yolo_flag(adapter_id);
            let e = this.agent_launch.entry_mut(adapter_id);
            e.args = toggle_flag(&e.args, flag);
            this.persist_agent_launch(cx);
        },
        cx,
    );
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
        .child(yolo_chip)
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
        assert_eq!(yolo_flag("claude-code"), "--dangerously-skip-permissions");
        assert_eq!(yolo_flag("codex"), "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(yolo_flag("aider"), "--yes-always");
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
}
