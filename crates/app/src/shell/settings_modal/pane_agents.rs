//! Agents / AI pane — edits the `CommitMessageAiSettings` working copy
//! (commit-message generation mode + agent + model). Applies immediately:
//! mutate the copy, write `commit_message_ai.toml`, watcher re-applies.
//! Desktop-notification prefs live in the Notifications pane.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{CommitMessageAiMode, Density, Theme, Typography};

use super::SettingsModal;
use super::controls::value_chip;
use super::layout::{SettingEntry, card_surface, entries_card, entry, section_title};
use super::segmented::{Segment, segmented};

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
    let ai_card = card_surface(
        theme,
        density,
        entries_card(
            theme,
            density,
            typography,
            ai_entries(modal, theme, density, typography, cx),
        ),
    );
    let launch_card =
        super::pane_agents_launch::render_launch_card(modal, theme, density, typography, cx);

    // Each section = a labelled title + its card + a muted footnote, grouped
    // tightly (8px) and separated from the next section by a wider gap.
    let ai_section = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(section_title(
            "Commit messages",
            "How commit messages are generated from the staged diff.",
            theme,
            typography,
        ))
        .child(ai_card)
        .child(footnote(hint(modal.ai.mode), theme, typography));

    let launch_section = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(section_title(
            "Agent launch",
            "Defaults the one-click launcher applies when you pick an agent.",
            theme,
            typography,
        ))
        .child(launch_card)
        .child(footnote(
            "One-click launch applies these. Hand-edit agent_launch.toml for arbitrary flags.",
            theme,
            typography,
        ));

    div()
        .flex()
        .flex_col()
        .gap(px(20.0))
        .child(ai_section)
        .child(launch_section)
        .into_any_element()
}

/// A muted explanatory line beneath a card.
fn footnote(
    text: impl Into<gpui::SharedString>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    div()
        .px(px(2.0))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(text.into())
        .into_any_element()
}

/// All Agents-pane entries (commit-message AI + launch defaults), unioned so
/// global search covers both sections.
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut all = ai_entries(modal, theme, density, typography, cx);
    all.extend(super::pane_agents_launch::entries(
        modal, theme, density, typography, cx,
    ));
    all
}

/// The commit-message AI rows. Agent + Model rows only appear in Agent mode
/// (they don't apply otherwise).
fn ai_entries(
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

    entries
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
