//! Agent-launch section of the Agents pane — edits the `AgentLaunchSettings`
//! working copy (default agent, per-agent enabled / skip-permissions / model).
//! Applies immediately: mutate the copy, write `agent_launch.toml`, watcher
//! reloads + swaps the global. These are the defaults the one-click launcher
//! applies when the user picks an agent.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Density, Theme, Typography, split_args};

use super::SettingsModal;
use super::controls::value_chip;
use super::layout::{SettingEntry, entry};
use super::segmented::{Segment, segmented};

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
    let mut out = Vec::with_capacity(LAUNCH_AGENTS.len() + 1);
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
