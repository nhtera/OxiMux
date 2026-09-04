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

/// Model presets the model chip cycles through when no Claude catalog has
/// been probed yet. The current value is kept as a stable anchor (prepended
/// when not already a preset) so cycling never silently drops a hand-set
/// model.
const MODEL_PRESETS: [&str; 5] = ["sonnet", "opus", "haiku", "gpt-5.5-codex", "gpt-5.5"];

/// The presets that are not Claude's: what stays when the installed CLI's own
/// Claude list replaces the three static aliases.
const NON_CLAUDE_PRESETS: [&str; 2] = ["gpt-5.5-codex", "gpt-5.5"];

/// The model chip's presets right now: the installed CLI's Claude wires (the
/// same rows the chat picker shows, all valid for `claude -p`) followed by the
/// Codex presets, or the static list until a probe has landed.
fn model_presets() -> Vec<String> {
    presets_from(oximux_agents::thread::shared_claude_catalog().as_deref())
}

/// [`model_presets`] over an explicit catalog, so the derivation is testable
/// without touching the process-wide slot.
fn presets_from(catalog: Option<&oximux_agents::thread::ClaudeCatalog>) -> Vec<String> {
    match catalog {
        Some(catalog) => catalog
            .models
            .iter()
            .map(|m| m.wire.clone())
            .chain(NON_CLAUDE_PRESETS.iter().map(|p| (*p).to_string()))
            .collect(),
        None => MODEL_PRESETS.iter().map(|p| (*p).to_string()).collect(),
    }
}

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
    let env_card = super::pane_agents_launch::render_env_card(modal, theme, density, typography, cx);

    // Each section = a labelled title + its card + a muted footnote, grouped
    // tightly (8px) and separated from the next section by a wider gap.
    // Every column here claims the pane's full width. A `flex_col` that sets
    // none is sized by its content, and the launch-environment card's rows have
    // no intrinsic width left once their descriptions are allowed to wrap — the
    // card collapsed to nothing and took its picker and editor with it.
    let ai_section = div()
        .flex()
        .flex_col()
        .w_full()
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
        .w_full()
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

    let env_section = div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(8.0))
        .child(section_title(
            "Environment & profiles",
            "Point an agent at an alternate endpoint, a proxy, or a second account \
             without patching source.",
            theme,
            typography,
        ))
        .child(env_card)
        .child(footnote(
            "Values are stored in plain text in agent_launch.toml. Not a credential vault.",
            theme,
            typography,
        ));

    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(20.0))
        .child(ai_section)
        .child(launch_section)
        .child(env_section)
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
            this.ai.agent.model = cycle_model(&this.ai.agent.model, &model_presets());
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
fn cycle_model(current: &str, presets: &[String]) -> String {
    let mut list: Vec<&str> = Vec::with_capacity(presets.len() + 1);
    if !presets.iter().any(|p| p == current) {
        list.push(current);
    }
    list.extend(presets.iter().map(String::as_str));
    let idx = list.iter().position(|m| *m == current).unwrap_or(0);
    list[(idx + 1) % list.len()].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_presets() -> Vec<String> {
        MODEL_PRESETS.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn cycle_model_advances_through_presets() {
        let presets = static_presets();
        assert_eq!(cycle_model("sonnet", &presets), "opus");
        assert_eq!(cycle_model("gpt-5.5", &presets), "sonnet"); // wraps
    }

    #[test]
    fn cycle_model_preserves_custom_anchor() {
        // A non-preset value is anchored, so the first cycle moves into
        // the presets and wrapping returns to it.
        let presets = static_presets();
        assert_eq!(cycle_model("my-local-model", &presets), "sonnet");
        assert_eq!(cycle_model("gpt-5.5", &presets), "sonnet");
    }

    /// The chip offers the CLI's own Claude rows once probed — Fable among
    /// them — and keeps the Codex presets after them.
    #[test]
    fn presets_follow_the_claude_catalog() {
        use oximux_agents::thread::parse_list_models;
        let fixture = include_str!(
            "../../../../../crates/agents/src/thread/testdata/claude_list_models_2_1_260.jsonl"
        );
        let presets = presets_from(Some(&parse_list_models(fixture)));
        assert!(presets.iter().any(|p| p == "claude-fable-5-1[1m]"), "{presets:?}");
        assert_eq!(&presets[presets.len() - 2..], &["gpt-5.5-codex", "gpt-5.5"]);
        assert!(!presets.iter().any(|p| p == "opus"), "static alias replaced by the CLI's wire");
        assert_eq!(presets_from(None), static_presets());
    }

    #[test]
    fn mode_labels_cover_all_modes() {
        assert_eq!(mode_label(CommitMessageAiMode::Off), "Off");
        assert_eq!(mode_label(CommitMessageAiMode::Heuristic), "Heuristic");
        assert_eq!(mode_label(CommitMessageAiMode::Agent), "Agent");
    }
}
