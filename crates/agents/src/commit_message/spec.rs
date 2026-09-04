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

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::thread::claude_catalog::shared_claude_catalog;

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
///
/// `id`/`label` are `Cow` so the static seed lists stay `const` while a
/// Claude row taken from the installed CLI's catalog (see [`models_for`])
/// can carry its own strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: Cow<'static, str>,
    pub label: Cow<'static, str>,
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
        id: Cow::Borrowed("haiku"),
        label: Cow::Borrowed("Haiku"),
        thinking_levels: NO_THINKING,
        default_thinking: "",
    },
    Model {
        id: Cow::Borrowed("sonnet"),
        label: Cow::Borrowed("Sonnet"),
        thinking_levels: CLAUDE_THINKING,
        default_thinking: "low",
    },
    Model {
        id: Cow::Borrowed("opus"),
        label: Cow::Borrowed("Opus"),
        thinking_levels: CLAUDE_THINKING,
        default_thinking: "low",
    },
];

const CODEX_MODELS: &[Model] = &[
    Model {
        id: Cow::Borrowed("gpt-5.5"),
        label: Cow::Borrowed("GPT-5.5"),
        thinking_levels: OPENAI_THINKING,
        default_thinking: "low",
    },
    Model {
        id: Cow::Borrowed("gpt-5.4"),
        label: Cow::Borrowed("GPT-5.4"),
        thinking_levels: OPENAI_THINKING,
        default_thinking: "low",
    },
    Model {
        id: Cow::Borrowed("gpt-5.4-mini"),
        label: Cow::Borrowed("GPT-5.4 Mini"),
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

/// The models `spec` accepts right now.
///
/// Claude's follow the installed CLI once its catalog has been probed (the
/// chat's model picker publishes it): every wire the CLI's own `/model` picker
/// sends is valid for `claude -p` too, so the commit-message settings can
/// offer Fable, or whatever the next release adds, without a code change. The
/// catalog rows come first; the static seed's aliases that the catalog does not
/// list (`opus`, where the CLI now says `opus[1m]`) stay behind them, because
/// they remain valid `--model` values and a settings file that names one must
/// keep generating the moment a chat probe lands. Effort levels are the static
/// Claude set for any row the CLI says takes effort and none for one that does
/// not (Haiku); the CLI itself is the final judge of a level. Until a probe
/// lands — and for every other agent — the static seed list is the answer.
pub fn models_for(spec: &AgentSpec) -> Vec<Model> {
    let mut models = Vec::new();
    if spec.id == AgentId::Claude
        && let Some(catalog) = shared_claude_catalog()
    {
        models.extend(catalog.models.iter().map(|m| {
            let thinks = !m.effort_levels.is_empty();
            Model {
                id: Cow::Owned(m.wire.clone()),
                label: Cow::Owned(m.label.clone()),
                thinking_levels: if thinks { CLAUDE_THINKING } else { NO_THINKING },
                default_thinking: if thinks { "low" } else { "" },
            }
        }));
    }
    let seed: Vec<Model> =
        spec.models.iter().filter(|m| !models.iter().any(|c| c.id == m.id)).cloned().collect();
    models.extend(seed);
    models
}

/// Look up a specific model on a spec. Returns `None` when the model
/// id isn't known to the agent (see [`models_for`] for what "known" means).
pub fn get_model(spec: &AgentSpec, model_id: &str) -> Option<Model> {
    models_for(spec).into_iter().find(|m| m.id == model_id)
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
        // `get_model` reads the process-wide Claude catalog slot.
        let _guard = crate::thread::claude_catalog::slot_test_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::thread::claude_catalog::clear_claude_catalog_for_test();
        assert!(get_model(&CLAUDE_SPEC, "sonnet").is_some());
        assert!(get_model(&CODEX_SPEC, "gpt-5.5").is_some());
        assert!(get_model(&CLAUDE_SPEC, "unknown").is_none());
    }

    /// With the CLI's catalog published, Claude's list is the catalog: a wire
    /// the static seed never knew (Fable) validates, a row the CLI says takes
    /// no effort carries none, and Codex is untouched.
    #[test]
    fn claude_models_follow_the_published_catalog() {
        use crate::thread::claude_catalog::{
            clear_claude_catalog_for_test, parse_list_models, publish_claude_catalog,
            slot_test_lock, FIXTURE_2_1_260,
        };
        let _guard = slot_test_lock().lock().unwrap_or_else(|p| p.into_inner());
        clear_claude_catalog_for_test();
        assert!(get_model(&CLAUDE_SPEC, "claude-fable-5-1[1m]").is_none(), "static seed has no Fable");
        assert_eq!(models_for(&CLAUDE_SPEC).len(), 3);

        publish_claude_catalog(parse_list_models(FIXTURE_2_1_260));
        let fable = get_model(&CLAUDE_SPEC, "claude-fable-5-1[1m]").expect("catalog row");
        assert_eq!(fable.label, "Fable");
        assert_eq!(fable.thinking_levels, CLAUDE_THINKING);
        let haiku = get_model(&CLAUDE_SPEC, "haiku").expect("catalog row");
        assert!(haiku.thinking_levels.is_empty(), "the catalog row wins over the seed's");
        // Four catalog rows plus the seed's `opus`, which the catalog spells
        // `opus[1m]` but which a settings file may still name.
        let all = models_for(&CLAUDE_SPEC);
        let ids: Vec<&str> = all.iter().map(|m| m.id.as_ref()).collect();
        assert_eq!(ids, ["opus[1m]", "claude-fable-5-1[1m]", "sonnet", "haiku", "opus"]);
        assert!(get_model(&CLAUDE_SPEC, "opus").is_some(), "a stored alias keeps generating");
        assert_eq!(models_for(&CODEX_SPEC), CODEX_MODELS.to_vec(), "codex keeps its seed");
        clear_claude_catalog_for_test();
    }

    #[test]
    fn default_models_exist_in_their_spec_model_list() {
        let _guard = crate::thread::claude_catalog::slot_test_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::thread::claude_catalog::clear_claude_catalog_for_test();
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
