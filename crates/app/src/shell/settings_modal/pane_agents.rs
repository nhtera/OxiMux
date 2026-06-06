//! Agents / AI pane — edits the `CommitMessageAiSettings` working copy
//! (commit-message generation mode + agent + model). Applies immediately:
//! mutate the copy, write `commit_message_ai.toml`, watcher re-applies.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{CommitMessageAiMode, Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{setting_row, value_chip};

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
    let ai = &modal.ai;
    let is_agent = ai.mode == CommitMessageAiMode::Agent;

    let mode = value_chip(
        "ai-mode",
        mode_label(ai.mode),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.ai.mode = next_mode(this.ai.mode);
            this.persist_ai(cx);
        },
        cx,
    );

    let agent_id = value_chip(
        "ai-agent",
        ai.agent.agent_id.clone(),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.ai.agent.agent_id = next_agent(&this.ai.agent.agent_id).to_string();
            this.persist_ai(cx);
        },
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

    let mut col = div()
        .flex()
        .flex_col()
        .child(setting_row("Commit-message AI", mode, theme, typography));

    if is_agent {
        col = col
            .child(setting_row("Agent", agent_id, theme, typography))
            .child(setting_row("Model", model, theme, typography));
    }

    col.child(
        div()
            .pt(px(12.0))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .child(hint(modal.ai.mode)),
    )
    .into_any_element()
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

fn next_mode(mode: CommitMessageAiMode) -> CommitMessageAiMode {
    match mode {
        CommitMessageAiMode::Off => CommitMessageAiMode::Heuristic,
        CommitMessageAiMode::Heuristic => CommitMessageAiMode::Agent,
        CommitMessageAiMode::Agent => CommitMessageAiMode::Off,
    }
}

fn next_agent(current: &str) -> &'static str {
    match current {
        "claude" => "codex",
        "codex" => "custom",
        _ => "claude",
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
    fn mode_cycles_off_heuristic_agent() {
        assert_eq!(next_mode(CommitMessageAiMode::Off), CommitMessageAiMode::Heuristic);
        assert_eq!(
            next_mode(CommitMessageAiMode::Heuristic),
            CommitMessageAiMode::Agent
        );
        assert_eq!(next_mode(CommitMessageAiMode::Agent), CommitMessageAiMode::Off);
    }
}
