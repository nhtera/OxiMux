//! Pure transformation from "user request + prompt text" into a
//! spawn-ready `(binary, argv, stdin_payload, label)` plan.
//!
//! Keeping this in a pure module (no I/O, no async) lets the spawner
//! stay thin and the test suite cover every branch deterministically.
//! Validation errors are typed via [`PlanError`] so the caller can
//! render specific messages without parsing string output.
//!
//! Two paths:
//!
//! 1. **Built-in agent** ([`AgentId::Claude`], [`AgentId::Codex`]) —
//!    consult [`super::spec::get_spec`], validate model + thinking
//!    level against the spec, build argv via the spec's
//!    `build_args` fn pointer.
//!
//! 2. **Custom command** ([`AgentId::Custom`]) — tokenize the user's
//!    template ([`tokenize_custom_command`]) with POSIX-style quote
//!    handling, substitute `{prompt}` placeholder when present
//!    (argv delivery), or pipe via stdin when absent.
//!
//! Errors are typed [`PlanError`] so the caller can render specific
//! messages ("Model X not available for Y", "Thinking level Z not
//! valid for model A", "Custom command empty", etc).

use thiserror::Error;

use super::spec::{AgentId, AgentSpec, PromptDelivery, get_model, get_spec};

/// Resolved plan ready to feed [`super::spawn::run_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessagePlan {
    pub binary: String,
    pub args: Vec<String>,
    /// `Some(payload)` when the prompt should be piped via stdin;
    /// `None` when it's already embedded in `args` (argv delivery).
    pub stdin_payload: Option<String>,
    /// Human-readable label used in error prefixes
    /// ("Claude failed: ..."). Built-in agents use their spec label;
    /// custom commands use the binary name as the label.
    pub label: String,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("agent `{0}` does not support AI commit messages")]
    UnknownAgent(String),
    #[error("model `{model}` is not available for {agent}")]
    UnknownModel { model: String, agent: String },
    #[error("thinking level `{level}` is not valid for model `{model}`")]
    UnknownThinkingLevel { level: String, model: String },
    #[error("model `{model}` does not support a thinking effort level")]
    ThinkingNotSupported { model: String },
    #[error("custom command is empty. Add one in commit_message_ai.toml")]
    CustomCommandEmpty,
    #[error("custom command template is invalid: {0}")]
    CustomCommandInvalid(String),
    #[error("agent command override is invalid: {0}")]
    OverrideInvalid(String),
}

/// Build a plan for one of the built-in agents. `custom_command` and
/// `prompt` are needed only for `AgentId::Custom`; for built-in
/// agents `custom_command` is ignored.
///
/// `agent_command_override` lets the user wrap the agent CLI in a
/// prefix (e.g. `tsx /path/to/dev-claude` for local dev builds) — it
/// replaces just the binary half of the plan; the rest of the argv
/// comes from the spec.
pub fn plan(
    agent_id: AgentId,
    model: &str,
    thinking_level: Option<&str>,
    custom_command: Option<&str>,
    agent_command_override: Option<&str>,
    prompt: &str,
) -> Result<CommitMessagePlan, PlanError> {
    if agent_id == AgentId::Custom {
        let template = custom_command
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(PlanError::CustomCommandEmpty)?;
        return plan_custom(template, prompt);
    }

    let agent_label = agent_id.as_str().to_string();
    let spec = get_spec(agent_id).ok_or(PlanError::UnknownAgent(agent_label.clone()))?;
    plan_builtin(spec, model, thinking_level, agent_command_override, prompt)
}

fn plan_builtin(
    spec: &'static AgentSpec,
    model: &str,
    thinking_level: Option<&str>,
    agent_command_override: Option<&str>,
    prompt: &str,
) -> Result<CommitMessagePlan, PlanError> {
    let model_entry = get_model(spec, model).ok_or_else(|| PlanError::UnknownModel {
        model: model.to_string(),
        agent: spec.label.to_string(),
    })?;
    if let Some(level) = thinking_level {
        if model_entry.thinking_levels.is_empty() {
            return Err(PlanError::ThinkingNotSupported {
                model: model_entry.label.to_string(),
            });
        }
        if !model_entry.thinking_levels.iter().any(|l| l.id == level) {
            return Err(PlanError::UnknownThinkingLevel {
                level: level.to_string(),
                model: model_entry.label.to_string(),
            });
        }
    }

    let prompt_argv = if spec.prompt_delivery == PromptDelivery::Argv {
        prompt.to_string()
    } else {
        String::new()
    };
    let mut args = (spec.build_args)(model, thinking_level, &prompt_argv);
    if spec.prompt_delivery == PromptDelivery::Argv && !args.iter().any(|a| a == prompt) {
        // build_args for argv-mode agents already includes the prompt;
        // fall through for any spec that elects to omit it (defensive).
        args.push(prompt.to_string());
    }

    let stdin_payload = if spec.prompt_delivery == PromptDelivery::Stdin {
        Some(prompt.to_string())
    } else {
        None
    };

    let (binary, prefix_args) = resolve_binary(spec.binary, agent_command_override)?;
    let mut final_args = prefix_args;
    final_args.extend(args);

    Ok(CommitMessagePlan {
        binary,
        args: final_args,
        stdin_payload,
        label: spec.label.to_string(),
    })
}

fn resolve_binary(
    default_binary: &str,
    override_template: Option<&str>,
) -> Result<(String, Vec<String>), PlanError> {
    let Some(template) = override_template.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((default_binary.to_string(), Vec::new()));
    };
    let tokens = tokenize_custom_command(template)
        .map_err(|e| PlanError::OverrideInvalid(e.to_string()))?;
    let mut iter = tokens.into_iter();
    let binary = iter
        .next()
        .ok_or_else(|| PlanError::OverrideInvalid("must start with a binary name".to_string()))?;
    Ok((binary, iter.collect()))
}

fn plan_custom(template: &str, prompt: &str) -> Result<CommitMessagePlan, PlanError> {
    let tokens = tokenize_custom_command(template)
        .map_err(|e| PlanError::CustomCommandInvalid(e.to_string()))?;
    if tokens.is_empty() {
        return Err(PlanError::CustomCommandEmpty);
    }

    let uses_placeholder = tokens.iter().any(|t| t.contains(CUSTOM_PROMPT_PLACEHOLDER));
    if uses_placeholder {
        let substituted: Vec<String> = tokens
            .iter()
            .map(|t| t.replace(CUSTOM_PROMPT_PLACEHOLDER, prompt))
            .collect();
        let mut iter = substituted.into_iter();
        let binary = iter.next().expect("non-empty asserted above");
        return Ok(CommitMessagePlan {
            label: extract_label(&binary),
            binary,
            args: iter.collect(),
            stdin_payload: None,
        });
    }

    let mut iter = tokens.into_iter();
    let binary = iter.next().expect("non-empty asserted above");
    let args: Vec<String> = iter.collect();
    Ok(CommitMessagePlan {
        label: extract_label(&binary),
        binary,
        args,
        stdin_payload: Some(prompt.to_string()),
    })
}

fn extract_label(binary: &str) -> String {
    // Use just the basename so a fully-qualified path like
    // `/usr/local/bin/ollama` renders as `ollama` in error prefixes.
    std::path::Path::new(binary)
        .file_name()
        .and_then(|f| f.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| binary.to_string())
}

pub const CUSTOM_PROMPT_PLACEHOLDER: &str = "{prompt}";

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TokenizeError {
    #[error("unclosed quote in command template")]
    UnclosedQuote,
}

/// POSIX-shell-style tokenizer for *grouping only* — single and
/// double quotes, backslash escapes inside double quotes.
/// **Does NOT** expand `$VAR`, backticks, globs, `~`, or command
/// substitution. The user's intent is "spawn this exact CLI";
/// adding shell semantics would create cross-platform surprises
/// (especially on Windows) and a security surface we don't need.
pub fn tokenize_custom_command(template: &str) -> Result<Vec<String>, TokenizeError> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            if ch == '\\' && q == '"' && i + 1 < chars.len() {
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == q {
                quote = None;
                in_token = true;
                i += 1;
                continue;
            }
            current.push(ch);
            i += 1;
            continue;
        }

        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            in_token = true;
            i += 1;
            continue;
        }

        if ch == '\\' && i + 1 < chars.len() {
            current.push(chars[i + 1]);
            in_token = true;
            i += 2;
            continue;
        }

        if ch.is_whitespace() {
            if in_token {
                tokens.push(std::mem::take(&mut current));
                in_token = false;
            }
            i += 1;
            continue;
        }

        current.push(ch);
        in_token = true;
        i += 1;
    }

    if quote.is_some() {
        return Err(TokenizeError::UnclosedQuote);
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_built_in_claude_includes_stdin_payload() {
        let plan = plan(AgentId::Claude, "sonnet", None, None, None, "PROMPT").unwrap();
        assert_eq!(plan.binary, "claude");
        assert_eq!(plan.stdin_payload.as_deref(), Some("PROMPT"));
        assert_eq!(plan.label, "Claude");
        assert!(plan.args.iter().any(|a| a == "-p"));
        assert!(plan.args.iter().any(|a| a == "sonnet"));
    }

    #[test]
    fn plan_built_in_codex_includes_stdin_payload() {
        let plan = plan(AgentId::Codex, "gpt-5.5", Some("high"), None, None, "P").unwrap();
        assert_eq!(plan.binary, "codex");
        assert_eq!(plan.stdin_payload.as_deref(), Some("P"));
        assert!(plan.args.iter().any(|a| a == "exec"));
        assert!(plan.args.iter().any(|a| a == "model_reasoning_effort=high"));
    }

    #[test]
    fn plan_rejects_unknown_model() {
        let err = plan(AgentId::Claude, "ghostmodel", None, None, None, "p").unwrap_err();
        match err {
            PlanError::UnknownModel { model, .. } => assert_eq!(model, "ghostmodel"),
            other => panic!("expected UnknownModel, got {other:?}"),
        }
    }

    #[test]
    fn plan_rejects_thinking_on_non_thinking_model() {
        let err = plan(AgentId::Claude, "haiku", Some("low"), None, None, "p").unwrap_err();
        assert!(matches!(err, PlanError::ThinkingNotSupported { .. }));
    }

    #[test]
    fn plan_rejects_unknown_thinking_level() {
        let err = plan(AgentId::Claude, "sonnet", Some("ultra"), None, None, "p").unwrap_err();
        match err {
            PlanError::UnknownThinkingLevel { level, .. } => assert_eq!(level, "ultra"),
            other => panic!("expected UnknownThinkingLevel, got {other:?}"),
        }
    }

    #[test]
    fn plan_custom_with_placeholder_uses_argv_delivery() {
        let plan = plan(
            AgentId::Custom,
            "",
            None,
            Some("ollama run llama3 {prompt}"),
            None,
            "my prompt",
        )
        .unwrap();
        assert_eq!(plan.binary, "ollama");
        assert_eq!(plan.args, vec!["run", "llama3", "my prompt"]);
        assert!(plan.stdin_payload.is_none());
        assert_eq!(plan.label, "ollama");
    }

    #[test]
    fn plan_custom_without_placeholder_uses_stdin_delivery() {
        let plan = plan(
            AgentId::Custom,
            "",
            None,
            Some("ollama run llama3"),
            None,
            "my prompt",
        )
        .unwrap();
        assert_eq!(plan.binary, "ollama");
        assert_eq!(plan.args, vec!["run", "llama3"]);
        assert_eq!(plan.stdin_payload.as_deref(), Some("my prompt"));
    }

    #[test]
    fn plan_custom_empty_command_errors() {
        let err = plan(AgentId::Custom, "", None, Some("   "), None, "p").unwrap_err();
        assert_eq!(err, PlanError::CustomCommandEmpty);
    }

    #[test]
    fn plan_custom_invalid_quote_errors() {
        let err = plan(AgentId::Custom, "", None, Some("ollama \"unclosed"), None, "p").unwrap_err();
        assert!(matches!(err, PlanError::CustomCommandInvalid(_)));
    }

    #[test]
    fn plan_custom_label_basename_only() {
        let plan = plan(
            AgentId::Custom,
            "",
            None,
            Some("/usr/local/bin/ollama run llama"),
            None,
            "p",
        )
        .unwrap();
        assert_eq!(plan.label, "ollama");
    }

    #[test]
    fn plan_built_in_with_agent_command_override() {
        let plan = plan(
            AgentId::Claude,
            "sonnet",
            None,
            None,
            Some("tsx /path/to/dev-claude"),
            "p",
        )
        .unwrap();
        assert_eq!(plan.binary, "tsx");
        // The override prefix becomes the first arg, then the spec's
        // own argv.
        assert_eq!(plan.args[0], "/path/to/dev-claude");
        assert!(plan.args.iter().any(|a| a == "-p"));
    }

    #[test]
    fn tokenize_handles_double_quoted_strings() {
        let tokens = tokenize_custom_command(r#"a "b c" d"#).unwrap();
        assert_eq!(tokens, vec!["a", "b c", "d"]);
    }

    #[test]
    fn tokenize_handles_single_quoted_strings() {
        let tokens = tokenize_custom_command("a 'b c' d").unwrap();
        assert_eq!(tokens, vec!["a", "b c", "d"]);
    }

    #[test]
    fn tokenize_handles_backslash_in_double_quotes() {
        let tokens = tokenize_custom_command(r#"a "b\"c""#).unwrap();
        assert_eq!(tokens, vec!["a", "b\"c"]);
    }

    #[test]
    fn tokenize_concatenates_adjacent_quoted_segments() {
        let tokens = tokenize_custom_command(r#"a"b"c"#).unwrap();
        assert_eq!(tokens, vec!["abc"]);
    }

    #[test]
    fn tokenize_rejects_unclosed_quote() {
        let err = tokenize_custom_command(r#"a "b"#).unwrap_err();
        assert_eq!(err, TokenizeError::UnclosedQuote);
    }

    #[test]
    fn tokenize_empty_template_returns_empty_vec() {
        assert!(tokenize_custom_command("").unwrap().is_empty());
        assert!(tokenize_custom_command("   ").unwrap().is_empty());
    }

    #[test]
    fn tokenize_does_not_expand_dollar_or_backtick() {
        // Variables and command substitution must stay literal — we
        // intentionally don't reimplement the shell.
        let tokens = tokenize_custom_command("echo $HOME `pwd`").unwrap();
        assert_eq!(tokens, vec!["echo", "$HOME", "`pwd`"]);
    }
}
