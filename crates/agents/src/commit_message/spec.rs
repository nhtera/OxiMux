//! Per-CLI declarative spec for non-interactive commit-message generation.
//!
//! Each spec describes one agent CLI: the binary name, how to deliver
//! the prompt (stdin or argv), the argv builder fn, and the static
//! model list. Round 1 ships Claude, Codex, and a generic `Custom`
//! template path for everything else.
//!
//! Deliberately separate from the interactive PTY agent adapter
//! surface ([`crate::cli::CliAgentAdapter`]) — non-interactive
//! one-shot generation has a different shape (no PTY, no streaming
//! UI, stdin prompt, single stdout slurp) and mixing the two would
//! confuse both code paths.
//!
//! Extending the catalog is mechanical: add a `pub const` spec, add the
//! variant to [`AgentId`], add it to [`get_spec`].

use serde::{Deserialize, Serialize};

/// Top-level discriminator for which agent to spawn (or use a custom
/// user template). `Custom` lives outside the spec table because its
/// binary + argv come from the user's settings template, not a static
/// spec entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentId {
    Claude,
    Codex,
    Custom,
}

impl AgentId {
    /// String form used in settings files. Matches the serde
    /// `rename_all = "lowercase"` representation.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentId::Claude => "claude",
            AgentId::Codex => "codex",
            AgentId::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Some(AgentId::Claude),
            "codex" => Some(AgentId::Codex),
            "custom" => Some(AgentId::Custom),
            _ => None,
        }
    }
}

/// Where the prompt is delivered: piped via stdin, or appended to argv.
/// Stdin is the default for diff-bearing prompts because argv has a
/// length limit on every POSIX shell and on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    Stdin,
    Argv,
}

/// Reasoning effort level for thinking-enabled models. Subset shared
/// across Claude / Codex / OpenAI variants — extend the per-agent
/// model entry to advertise which levels apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingLevel {
    pub id: &'static str,
    pub label: &'static str,
}

/// One model the agent supports. `thinking_levels` is empty when the
/// model has no effort selector (UI hides the dropdown in that case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub thinking_levels: &'static [ThinkingLevel],
    /// Default level to use when none is specified. `""` when the
    /// model has no thinking-levels list.
    pub default_thinking: &'static str,
}

/// Static spec for one agent CLI. `build_args` is a fn pointer so the
/// per-agent argv assembly stays inline + branchless.
#[derive(Debug)]
pub struct AgentSpec {
    pub id: AgentId,
    pub label: &'static str,
    pub binary: &'static str,
    pub prompt_delivery: PromptDelivery,
    /// Produce the argv vector for `cmd spawn`. `prompt_argv` is the
    /// prompt text when `prompt_delivery == Argv`, empty string
    /// otherwise.
    pub build_args:
        fn(model: &str, thinking_level: Option<&str>, prompt_argv: &str) -> Vec<String>,
    pub models: &'static [Model],
    pub default_model: &'static str,
}

const NO_THINKING: &[ThinkingLevel] = &[];

const CLAUDE_THINKING: &[ThinkingLevel] = &[
    ThinkingLevel { id: "low", label: "Low" },
    ThinkingLevel { id: "medium", label: "Medium" },
    ThinkingLevel { id: "high", label: "High" },
    ThinkingLevel { id: "xhigh", label: "Extra High" },
    ThinkingLevel { id: "max", label: "Max" },
];

const OPENAI_THINKING: &[ThinkingLevel] = &[
    ThinkingLevel { id: "low", label: "Low" },
    ThinkingLevel { id: "medium", label: "Medium" },
    ThinkingLevel { id: "high", label: "High" },
    ThinkingLevel { id: "xhigh", label: "Extra High" },
];

const CLAUDE_MODELS: &[Model] = &[
    Model {
        id: "haiku",
        label: "Haiku",
        thinking_levels: NO_THINKING,
        default_thinking: "",
    },
    Model {
        id: "sonnet",
        label: "Sonnet",
        thinking_levels: CLAUDE_THINKING,
        default_thinking: "low",
    },
    Model {
        id: "opus",
        label: "Opus",
        thinking_levels: CLAUDE_THINKING,
        default_thinking: "low",
    },
];

const CODEX_MODELS: &[Model] = &[
    Model {
        id: "gpt-5.5",
        label: "GPT-5.5",
        thinking_levels: OPENAI_THINKING,
        default_thinking: "low",
    },
    Model {
        id: "gpt-5.4",
        label: "GPT-5.4",
        thinking_levels: OPENAI_THINKING,
        default_thinking: "low",
    },
    Model {
        id: "gpt-5.4-mini",
        label: "GPT-5.4 Mini",
        thinking_levels: OPENAI_THINKING,
        default_thinking: "low",
    },
];

/// `claude -p --output-format text --model {model} --permission-mode plan [--effort {level}]`
///
/// Stdin delivery — `claude -p` reads the prompt from stdin when no
/// positional prompt is given. The `--permission-mode plan` flag
/// ensures Claude treats this as a read-only generation, not an agent
/// session that could mutate files.
pub const CLAUDE_SPEC: AgentSpec = AgentSpec {
    id: AgentId::Claude,
    label: "Claude",
    binary: "claude",
    prompt_delivery: PromptDelivery::Stdin,
    build_args: build_claude_args,
    models: CLAUDE_MODELS,
    default_model: "sonnet",
};

fn build_claude_args(model: &str, thinking: Option<&str>, _prompt: &str) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "--output-format".into(),
        "text".into(),
        "--model".into(),
        model.into(),
        "--permission-mode".into(),
        "plan".into(),
    ];
    if let Some(level) = thinking {
        args.push("--effort".into());
        args.push(level.into());
    }
    args
}

/// `codex exec --ephemeral --skip-git-repo-check -s read-only --model {model} [-c model_reasoning_effort={level}]`
///
/// Stdin delivery — `codex exec` reads stdin when no prompt arg is
/// supplied. The `--ephemeral` + `-s read-only` combo enforces the
/// "text generation only, no persisted session, no workspace writes"
/// invariant — commit-message generation should never mutate the
/// repo or leak into a long-lived agent session.
/// `--skip-git-repo-check` lets it run from a worktree without
/// complaining about uncommitted state.
pub const CODEX_SPEC: AgentSpec = AgentSpec {
    id: AgentId::Codex,
    label: "Codex",
    binary: "codex",
    prompt_delivery: PromptDelivery::Stdin,
    build_args: build_codex_args,
    models: CODEX_MODELS,
    default_model: "gpt-5.5",
};

fn build_codex_args(model: &str, thinking: Option<&str>, _prompt: &str) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "--ephemeral".into(),
        "--skip-git-repo-check".into(),
        "-s".into(),
        "read-only".into(),
        "--model".into(),
        model.into(),
    ];
    if let Some(level) = thinking {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort={level}"));
    }
    args
}

/// Look up the static spec for a built-in agent. Returns `None` for
/// `AgentId::Custom` — custom commands go through
/// [`super::plan::plan_custom`] instead, which tokenizes the user's
/// template directly.
pub fn get_spec(agent_id: AgentId) -> Option<&'static AgentSpec> {
    match agent_id {
        AgentId::Claude => Some(&CLAUDE_SPEC),
        AgentId::Codex => Some(&CODEX_SPEC),
        AgentId::Custom => None,
    }
}

/// All built-in specs, for settings UI dropdowns.
pub const BUILTIN_SPECS: &[&AgentSpec] = &[&CLAUDE_SPEC, &CODEX_SPEC];

/// Look up a specific model on a spec. Returns `None` when the model
/// id isn't known to the agent.
pub fn get_model(spec: &AgentSpec, model_id: &str) -> Option<&'static Model> {
    spec.models.iter().find(|m| m.id == model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_round_trips_through_string_form() {
        for id in [AgentId::Claude, AgentId::Codex, AgentId::Custom] {
            assert_eq!(AgentId::parse(id.as_str()), Some(id));
        }
    }

    #[test]
    fn agent_id_parse_is_case_insensitive() {
        assert_eq!(AgentId::parse("CLAUDE"), Some(AgentId::Claude));
        assert_eq!(AgentId::parse("Codex"), Some(AgentId::Codex));
    }

    #[test]
    fn agent_id_parse_rejects_unknown() {
        assert_eq!(AgentId::parse("opencode"), None);
        assert_eq!(AgentId::parse(""), None);
    }

    #[test]
    fn get_spec_returns_none_for_custom() {
        assert!(get_spec(AgentId::Custom).is_none());
    }

    #[test]
    fn claude_spec_builds_expected_argv() {
        let args = (CLAUDE_SPEC.build_args)("sonnet", Some("medium"), "");
        assert!(args.iter().any(|a| a == "-p"));
        assert!(args.iter().any(|a| a == "--model"));
        let model_idx = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[model_idx + 1], "sonnet");
        assert!(args.iter().any(|a| a == "--effort"));
        let effort_idx = args.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(args[effort_idx + 1], "medium");
        assert!(args.iter().any(|a| a == "--permission-mode"));
    }

    #[test]
    fn claude_spec_omits_effort_when_no_thinking() {
        let args = (CLAUDE_SPEC.build_args)("haiku", None, "");
        assert!(!args.iter().any(|a| a == "--effort"));
    }

    #[test]
    fn codex_spec_builds_expected_argv() {
        let args = (CODEX_SPEC.build_args)("gpt-5.5", Some("high"), "");
        assert_eq!(args[0], "exec");
        assert!(args.iter().any(|a| a == "--ephemeral"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(args.iter().any(|a| a == "-s"));
        let s_idx = args.iter().position(|a| a == "-s").unwrap();
        assert_eq!(args[s_idx + 1], "read-only");
        assert!(args.iter().any(|a| a == "model_reasoning_effort=high"));
    }

    #[test]
    fn codex_spec_omits_reasoning_when_no_thinking() {
        let args = (CODEX_SPEC.build_args)("gpt-5.5", None, "");
        assert!(!args.iter().any(|a| a.starts_with("model_reasoning_effort")));
    }

    #[test]
    fn get_model_returns_known_models() {
        assert!(get_model(&CLAUDE_SPEC, "sonnet").is_some());
        assert!(get_model(&CODEX_SPEC, "gpt-5.5").is_some());
        assert!(get_model(&CLAUDE_SPEC, "unknown").is_none());
    }

    #[test]
    fn default_models_exist_in_their_spec_model_list() {
        for spec in BUILTIN_SPECS {
            assert!(
                get_model(spec, spec.default_model).is_some(),
                "default_model {} missing from spec {}",
                spec.default_model,
                spec.label,
            );
        }
    }
}
