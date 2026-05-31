//! AI commit-message generation surface.
//!
//! Three modes dispatched from a single entry point:
//!
//! - [`Mode::Off`] — feature disabled; sparkles button hides.
//! - [`Mode::Heuristic`] — pure deterministic generator over the
//!   already-parsed [`FileStatus`] records; no network, no spawn.
//!   Backed by [`crate::commit_message_heuristic::generate_heuristic`].
//! - [`Mode::Agent`] — spawn a configured CLI binary (Claude, Codex,
//!   or a user-supplied custom command), pipe a prompt built from
//!   `git diff --cached`, parse the response. See [`spawn`].
//!
//! The dispatcher takes a single [`StagedContext`] for the agent path
//! and a [`&[FileStatus]`] for the heuristic path. The caller (the
//! commit-area in the app crate) fetches both up front and hands them
//! in — the dispatcher doesn't touch git itself.

pub mod plan;
pub mod prompt;
pub mod spawn;
pub mod spec;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use oximux_core::FileStatus;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use plan::{CommitMessagePlan, PlanError, plan as plan_commit_message};
pub use prompt::{
    SplitMessage, StagedContext, build_prompt, clean_message, format_message, split_message,
};
pub use spawn::{GENERATION_TIMEOUT, GenerationError, MAX_OUTPUT_BYTES, run_plan};
pub use spec::{
    AgentId, AgentSpec, BUILTIN_SPECS, CLAUDE_SPEC, CODEX_SPEC, Model, PromptDelivery,
    ThinkingLevel, get_model, get_spec,
};

use crate::commit_message_heuristic;

/// Top-level mode selected via the user's settings file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum Mode {
    /// Feature disabled — sparkles button hidden.
    Off,
    /// Pure deterministic heuristic. Default for fresh installs.
    #[default]
    Heuristic,
    /// Spawn a CLI agent. Field config picks which.
    Agent(AgentConfig),
}

/// Configuration carried by [`Mode::Agent`]. Loaded from
/// `commit_message_ai.toml`. Custom-command lives in its own
/// optional field so claude/codex configs don't need to clear it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub agent_id: AgentId,
    pub model: String,
    /// Optional thinking-effort level. `None` when the model has no
    /// effort selector or the user picked the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// Optional user-supplied prompt suffix appended verbatim to the
    /// prompt (gitmoji, internal copy rules, ...). Truncated to
    /// [`prompt::CUSTOM_PROMPT_BYTE_BUDGET`] before sending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_prompt: Option<String>,
    /// Required when `agent_id == Custom`; ignored otherwise. The
    /// user's shell-like command template; `{prompt}` substitution
    /// picks argv delivery, absence picks stdin delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_command: Option<String>,
    /// Optional override for the agent binary itself (e.g.
    /// `tsx /path/to/dev-claude`). Ignored when `agent_id == Custom`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_command_override: Option<String>,
}

/// Errors surfaced to the UI's status row. Wraps the lower-level
/// plan + spawn errors so the caller doesn't have to match on three
/// enums.
#[derive(Debug, Clone, Error)]
pub enum GenerateError {
    #[error("AI commit generation is disabled in commit_message_ai.toml")]
    Disabled,
    #[error("nothing staged to generate from")]
    NothingStaged,
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Run(#[from] GenerationError),
}

/// Generate a commit message according to `mode`. Returns the
/// finished textarea payload (subject + blank line + body) ready to
/// hand to `InputState::set_value`.
///
/// `staged_files` is consulted only by [`Mode::Heuristic`].
/// `context` is consulted only by [`Mode::Agent`] — the caller may
/// pass an empty placeholder when mode is heuristic, but typically
/// fetches it once and passes through unconditionally.
/// `cancel` is observed by the agent spawner; the heuristic path
/// ignores it (synchronous, microseconds).
pub async fn generate(
    mode: &Mode,
    staged_files: &[FileStatus],
    context: StagedContext,
    cancel: Arc<AtomicBool>,
) -> Result<String, GenerateError> {
    match mode {
        Mode::Off => Err(GenerateError::Disabled),
        Mode::Heuristic => {
            if staged_files.is_empty() {
                return Err(GenerateError::NothingStaged);
            }
            Ok(commit_message_heuristic::generate_heuristic(staged_files))
        }
        Mode::Agent(config) => generate_agent(config, context, cancel).await,
    }
}

async fn generate_agent(
    config: &AgentConfig,
    context: StagedContext,
    cancel: Arc<AtomicBool>,
) -> Result<String, GenerateError> {
    if context.summary.trim().is_empty() {
        return Err(GenerateError::NothingStaged);
    }
    let prompt_text = build_prompt(&context, config.custom_prompt.as_deref().unwrap_or(""));
    let plan = plan_commit_message(
        config.agent_id,
        &config.model,
        config.thinking_level.as_deref(),
        config.custom_command.as_deref(),
        config.agent_command_override.as_deref(),
        &prompt_text,
    )?;
    let workdir: &Path = context.workdir.as_path();
    let message = run_plan(plan, workdir, cancel).await?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};

    fn ctx() -> StagedContext {
        StagedContext {
            branch: Some("main".into()),
            summary: "M\ta.txt".into(),
            patch: "diff body".into(),
            workdir: std::env::temp_dir(),
        }
    }

    fn fs(path: &str) -> FileStatus {
        FileStatus {
            path: PathBuf::from(path),
            index: IndexStatus::Modified,
            worktree: WorktreeStatus::Unmodified,
            rename: None,
            line_counts: None,
            conflict_kind: None,
        }
    }

    #[tokio::test]
    async fn generate_off_returns_disabled_error() {
        let result =
            generate(&Mode::Off, &[fs("a.rs")], ctx(), Arc::new(AtomicBool::new(false))).await;
        assert!(matches!(result, Err(GenerateError::Disabled)));
    }

    #[tokio::test]
    async fn generate_heuristic_returns_message_for_staged_files() {
        let result = generate(
            &Mode::Heuristic,
            &[fs("crates/app/src/a.rs"), fs("crates/app/src/b.rs")],
            ctx(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("heuristic should succeed");
        assert!(result.contains("chore") || result.contains("feat") || result.contains("update"));
    }

    #[tokio::test]
    async fn generate_heuristic_with_no_staged_returns_nothing_staged() {
        let result =
            generate(&Mode::Heuristic, &[], ctx(), Arc::new(AtomicBool::new(false))).await;
        assert!(matches!(result, Err(GenerateError::NothingStaged)));
    }

    #[tokio::test]
    async fn generate_agent_with_empty_context_summary_returns_nothing_staged() {
        let mut empty = ctx();
        empty.summary = "".into();
        let config = AgentConfig {
            agent_id: AgentId::Custom,
            model: String::new(),
            thinking_level: None,
            custom_prompt: None,
            custom_command: Some("/bin/echo".into()),
            agent_command_override: None,
        };
        let result = generate(
            &Mode::Agent(config),
            &[],
            empty,
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert!(matches!(result, Err(GenerateError::NothingStaged)));
    }

    #[test]
    fn mode_default_is_heuristic() {
        assert_eq!(Mode::default(), Mode::Heuristic);
    }

    #[test]
    fn mode_round_trips_through_toml() {
        let config = AgentConfig {
            agent_id: AgentId::Claude,
            model: "sonnet".into(),
            thinking_level: Some("medium".into()),
            custom_prompt: Some("Use gitmoji".into()),
            custom_command: None,
            agent_command_override: None,
        };
        let mode = Mode::Agent(config);
        let toml = toml::to_string(&mode).expect("serialize");
        let parsed: Mode = toml::from_str(&toml).expect("parse");
        assert_eq!(mode, parsed);
    }
}
