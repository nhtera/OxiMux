//! Agents / AI pane — edits the `CommitMessageAiSettings` working copy
//! (commit-message generation mode + agent + model). Applies immediately:
//! mutate the copy, write `commit_message_ai.toml`, watcher re-applies.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{CommitMessageAiMode, Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry};
use super::segmented::{Segment, segmented};
use crate::notifier::{AgentNotifySettings, keys};

/// Notification toggle rows: label, description, settings key, and a selector
/// resolving the matching atomic in [`AgentNotifySettings`]. Driven as data so
/// the six rows stay in sync with the struct without six near-identical blocks.
type NotifySelect = fn(&AgentNotifySettings) -> &AtomicBool;
const NOTIFY_ROWS: [(&str, &str, &str, NotifySelect); 6] = [
    (
        "Approval needed",
        "Notify when an agent pauses for a dangerous-action approval.",
        keys::NEEDS_APPROVAL,
        |s| &s.needs_approval,
    ),
    (
        "Waiting for input",
        "Notify when an agent pauses waiting for a reply.",
        keys::WAITING_INPUT,
        |s| &s.waiting_input,
    ),
    (
        "Agent finished",
        "Notify when an agent completes successfully.",
        keys::DONE,
        |s| &s.done,
    ),
    (
        "Agent failed",
        "Notify when an agent exits with an error.",
        keys::FAILED,
        |s| &s.failed,
    ),
    (
        "Play sound",
        "Play a system sound with each notification.",
        keys::SOUND,
        |s| &s.sound,
    ),
    (
        "Only when unfocused",
        "Suppress notifications while the OxiMux window is focused.",
        keys::ONLY_WHEN_UNFOCUSED,
        |s| &s.only_when_unfocused,
    ),
];

/// Agent CLIs the segmented picker exposes.
const AGENTS: [&str; 3] = ["claude", "codex", "custom"];

/// Model presets the model chip cycles through. The current value is kept
/// as a stable anchor (prepended when not already a preset) so cycling
/// never silently drops a hand-set model.
const MODEL_PRESETS: [&str; 5] = ["sonnet", "opus", "haiku", "gpt-5.5-codex", "gpt-5.5"];

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(entries_card(
            theme,
            density,
            typography,
            entries(modal, theme, density, typography, cx),
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(hint(modal.ai.mode)),
        )
        .into_any_element()
}

/// The Agents/AI pane's settings as reusable entries. Agent + Model rows only
/// appear in Agent mode (they don't apply otherwise).
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let ai = &modal.ai;
    let is_agent = ai.mode == CommitMessageAiMode::Agent;

    let mode = segmented(
        "ai-mode",
        [
            CommitMessageAiMode::Off,
            CommitMessageAiMode::Heuristic,
            CommitMessageAiMode::Agent,
        ]
        .into_iter()
        .map(|m| {
            Segment::new(mode_label(m), ai.mode == m, move |this, _w, cx| {
                this.ai.mode = m;
                this.persist_ai(cx);
            })
        })
        .collect(),
        theme,
        density,
        typography,
        cx,
    );

    let agent_id = segmented(
        "ai-agent",
        AGENTS
            .into_iter()
            .map(|a| {
                Segment::new(a, ai.agent.agent_id == a, move |this, _w, cx| {
                    this.ai.agent.agent_id = a.to_string();
                    this.persist_ai(cx);
                })
            })
            .collect(),
        theme,
        density,
        typography,
        cx,
    );

    let model = value_chip(
        "ai-model",
        ai.agent.model.clone(),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.ai.agent.model = cycle_model(&this.ai.agent.model);
            this.persist_ai(cx);
        },
        cx,
    );

    let mut entries = vec![entry(
        "Commit-message AI",
        "How commit messages are generated from the staged diff.",
        mode,
    )];

    if is_agent {
        entries.push(entry(
            "Agent",
            "Which agent CLI runs for agent-mode generation.",
            agent_id,
        ));
        entries.push(entry("Model", "Model name passed to the agent CLI.", model));
    }

    // Desktop-notification prefs apply in every mode, so they're always shown.
    for (idx, (label, description, key, select)) in NOTIFY_ROWS.iter().enumerate() {
        entries.push(entry(
            *label,
            *description,
            notify_toggle(idx, key, *select, modal, theme, cx),
        ));
    }

    entries
}

/// One notification-pref toggle. Reads the live atomic for its current value;
/// clicking flips the atomic (effective immediately) and persists the new
/// value so it survives a restart.
fn notify_toggle(
    idx: usize,
    key: &'static str,
    select: NotifySelect,
    modal: &SettingsModal,
    theme: Theme,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let current = select(&modal.notify).load(Ordering::Relaxed);
    toggle_switch(
        ("notify-toggle", idx),
        current,
        theme,
        move |this, _w, cx| {
            let flag = select(&this.notify);
            let next = !flag.load(Ordering::Relaxed);
            flag.store(next, Ordering::Relaxed);
            this.persist_notify(key, next, cx);
        },
        cx,
    )
}

fn hint(mode: CommitMessageAiMode) -> &'static str {
    match mode {
        CommitMessageAiMode::Off => "Sparkles button hidden. Choose Heuristic or Agent to enable.",
        CommitMessageAiMode::Heuristic => "Offline local heuristic — no agent CLI required.",
        CommitMessageAiMode::Agent => {
            "Runs the configured agent CLI. Edit commit_message_ai.toml for custom commands."
        }
    }
}

fn mode_label(mode: CommitMessageAiMode) -> &'static str {
    match mode {
        CommitMessageAiMode::Off => "Off",
        CommitMessageAiMode::Heuristic => "Heuristic",
        CommitMessageAiMode::Agent => "Agent",
    }
}

/// Cycle to the next model preset, anchoring on the current value so a
/// hand-set custom model is preserved (reachable by wrapping).
fn cycle_model(current: &str) -> String {
    let mut list: Vec<&str> = Vec::with_capacity(MODEL_PRESETS.len() + 1);
    if !MODEL_PRESETS.contains(&current) {
        list.push(current);
    }
    list.extend_from_slice(&MODEL_PRESETS);
    let idx = list.iter().position(|m| *m == current).unwrap_or(0);
    list[(idx + 1) % list.len()].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_model_advances_through_presets() {
        assert_eq!(cycle_model("sonnet"), "opus");
        assert_eq!(cycle_model("gpt-5.5"), "sonnet"); // wraps
    }

    #[test]
    fn cycle_model_preserves_custom_anchor() {
        // A non-preset value is anchored, so the first cycle moves into
        // the presets and wrapping returns to it.
        assert_eq!(cycle_model("my-local-model"), "sonnet");
        assert_eq!(cycle_model("gpt-5.5"), "sonnet");
    }

    #[test]
    fn mode_labels_cover_all_modes() {
        assert_eq!(mode_label(CommitMessageAiMode::Off), "Off");
        assert_eq!(mode_label(CommitMessageAiMode::Heuristic), "Heuristic");
        assert_eq!(mode_label(CommitMessageAiMode::Agent), "Agent");
    }
}
