//! Pi adapter (`pi` CLI).
//!
//! Spawns the `pi` interactive TUI in a PTY. This is the *terminal* face of pi;
//! its structured-chat face is `pi --mode rpc`, which lives in
//! `thread/pi/` and never goes through this adapter.
//!
//! Registering pi here is what makes it **reachable**: the tab `+` menu and the
//! New Agent draft's roster are both built from the registry, and
//! `AgentLaunchSettings::chat_capable`/`transport_for` — which already knew
//! `"pi"` — are only ever consulted for ids those lists produce. A chat backend
//! that no list mentions cannot be opened, however complete it is.
//!
//! ## Why no `models()`
//!
//! Pi's model list is per-user (it depends on which providers they have auth
//! for), so no static list is correct. Worse, a static list would have to name
//! bare ids, and a bare id is not a model reference in pi — it is a fuzzy search
//! *pattern* matched across every provider it knows, so `--model gpt-5.5` can
//! silently load a different provider's build of that name. The chat backend
//! reads the real catalog from `get_available_models` and offers
//! provider-qualified wires; the terminal launch simply doesn't offer a model
//! row rather than offering a hazardous one.
//!
//! ## Why empty `status_patterns()`
//!
//! Same standing rule as `CodexAdapter`: pi's TUI prompt bytes
//! (with ANSI SGR wrapping) have not been captured, and this project does not
//! write regex against an imagined haystack. The `StatusMachine` fallback
//! supplies Running / Idle / exit transitions.

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use crate::cli::adapter::EMPTY_PATTERNS;
use crate::cli::detect::which_on_path;
use crate::cli::{CliAgentAdapter, CommandSpec, StatusPattern};
use crate::runtime::AgentSessionConfig;

/// Bare-name binary, resolved via PATH.
const PI_BIN: &str = "pi";

/// Stateless adapter — one instance lives in the runtime registry and is reused
/// across all Pi terminal sessions.
#[derive(Debug, Clone, Copy, Default)]
pub struct PiAdapter;

#[async_trait]
impl CliAgentAdapter for PiAdapter {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn name(&self) -> &'static str {
        "Pi"
    }

    async fn detect(&self) -> Result<bool> {
        Ok(which_on_path(PI_BIN).await)
    }

    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec> {
        // Argv order (`pi [options] [@files...] [messages...]`):
        //   --session <id>   resume, when one was requested
        //   --model <m>      only when the caller supplied one
        //   <extra args>     user-configured launch flags
        //   <prompt>         positional, trailing
        let mut args: Vec<String> = Vec::new();

        // Resume takes pi's session **id**, never a file path: pi resolves a
        // path-shaped reference with no existence check and starts a brand-new
        // empty session at it, reporting nothing. An id that doesn't resolve
        // exits 1 with a message the user can act on. `SessionResumption`
        // carries ids, so passing it straight through keeps the loud branch.
        if let Some(id) = cfg.resumption.source_id() {
            args.push("--session".to_string());
            args.push(id.to_string());
        }

        if let Some(model) = cfg.model.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push("--model".to_string());
            args.push(model.to_string());
        }

        args.extend(cfg.extra_args.iter().cloned());

        if let Some(prompt) = cfg.prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.to_string());
        }

        Ok(CommandSpec {
            program: PathBuf::from(PI_BIN),
            args,
            env: Vec::new(),
            stdin_seed: None,
        })
    }

    fn status_patterns(&self) -> &[StatusPattern] {
        EMPTY_PATTERNS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{AgentAdapter, SessionResumption};

    fn cfg() -> AgentSessionConfig {
        AgentSessionConfig {
            adapter: AgentAdapter::Pi,
            worktree_path: PathBuf::from("/tmp"),
            prompt: None,
            model: None,
            effort: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            custom_command: None,
            resumption: SessionResumption::None,
        }
    }

    #[test]
    fn bare_launch_passes_no_flags() {
        let spec = PiAdapter.build_command(&cfg()).expect("build");
        assert_eq!(spec.program, PathBuf::from("pi"));
        assert!(spec.args.is_empty(), "got {:?}", spec.args);
    }

    #[test]
    fn model_and_prompt_ride_in_pis_own_argv_order() {
        let c = AgentSessionConfig {
            model: Some("openai-codex/gpt-5.5".into()),
            prompt: Some("summarize this repo".into()),
            ..cfg()
        };
        let spec = PiAdapter.build_command(&c).expect("build");
        assert_eq!(
            spec.args,
            vec!["--model", "openai-codex/gpt-5.5", "summarize this repo"],
            "the prompt is positional and trails every flag"
        );
    }

    #[test]
    fn resume_passes_the_session_id_not_a_path() {
        // A path-shaped reference makes pi mint a new empty session in silence;
        // only an id fails loudly when it doesn't resolve.
        let c = AgentSessionConfig {
            resumption: SessionResumption::Resume {
                id: "019f650c-a70a-77c4-8fa4-f81e6e6ad1f3".into(),
            },
            ..cfg()
        };
        let spec = PiAdapter.build_command(&c).expect("build");
        assert_eq!(spec.args, vec!["--session", "019f650c-a70a-77c4-8fa4-f81e6e6ad1f3"]);
    }

    #[test]
    fn no_static_model_list_is_offered() {
        // A bare id is a fuzzy search pattern in pi, not a reference — a static
        // list would name bare ids and could silently load another provider's
        // model of the same name. The live catalog is the only safe source.
        assert!(PiAdapter.models().is_empty());
        assert!(PiAdapter.efforts().is_empty());
    }

    #[test]
    fn extra_args_precede_the_positional_prompt() {
        let c = AgentSessionConfig {
            extra_args: vec!["--tools".into(), "read,ls".into()],
            prompt: Some("hi".into()),
            ..cfg()
        };
        let spec = PiAdapter.build_command(&c).expect("build");
        assert_eq!(spec.args, vec!["--tools", "read,ls", "hi"]);
    }
}
