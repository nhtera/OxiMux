//! Aider adapter (`aider` OSS pair-programming CLI).
//!
//! Spawns `aider` interactively in a PTY. Optional `--model <m>` from
//! `AgentSessionConfig::model` (long flag — Aider's documented form).
//!
//! ## Prompt plumbing
//!
//! Aider has no positional-prompt argument: interactive sessions read
//! prompts from the REPL after launch, and `--message <msg>` is a
//! one-shot mode that exits after the reply (incompatible with the
//! OxiMux interactive-PTY model). `cfg.prompt` therefore travels on
//! `CommandSpec::stdin_seed` — the runtime writes it into the PTY
//! once the child is ready, and the agent receives it as the first
//! REPL input. The trait docstring (`adapter.rs::CommandSpec::stdin_seed`)
//! reserved this path for exactly this case; Aider is the first real
//! consumer.
//!
//! `MAX_SAFE_STDIN_SEED = 4096` (in `runtime_impl.rs`) keeps oversize
//! prompts from blocking the spawn_blocking thread on a full PTY
//! buffer; the runtime logs a warn above that threshold.
//!
//! ## Why empty `status_patterns()`
//!
//! Same reasoning as `CodexAdapter`: Aider TUI prompt bytes (with ANSI
//! SGR wrapping) have not been captured. The step-5 journal lesson
//! (`docs/journals/260520-1545-phase-03-step-05-claude-code-adapter.md`)
//! locked the project principle: ship status patterns only when the
//! real byte sequences are in hand. Pattern calibration is deferred
//! to dogfood week 18 (step 14). `StatusMachine` fallback supplies
//! Running / Idle / exit transitions; `NeedsApproval` falls back to
//! the manual badge-click `force()` override.
//!
//! ## Why no `--yes` / `--auto-commits` overrides
//!
//! User's `~/.aider.conf.yml` owns those — the "no paternalistic
//! defaults" rule from the v0.9 retro.

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use crate::cli::adapter::EMPTY_PATTERNS;
use crate::cli::detect::which_on_path;
use crate::cli::{CliAgentAdapter, CommandSpec, StatusPattern};
use crate::runtime::AgentSessionConfig;

/// Bare-name binary, resolved via PATH. Centralized so step 8 (registry)
/// can surface a settings override without grepping the body.
const AIDER_BIN: &str = "aider";

/// Stateless adapter — one instance lives in the runtime registry and is
/// reused across all Aider sessions. Per-session config (model, prompt,
/// worktree) flows through `AgentSessionConfig`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AiderAdapter;

#[async_trait]
impl CliAgentAdapter for AiderAdapter {
    fn id(&self) -> &'static str {
        "aider"
    }

    fn name(&self) -> &'static str {
        "Aider"
    }

    async fn detect(&self) -> Result<bool> {
        Ok(which_on_path(AIDER_BIN).await)
    }

    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec> {
        // Argv:
        //   --model <m>           (long flag is Aider's documented form)
        //
        // No positional file args (user `/add`s inside the REPL or
        // selects via the step-10 launch dialog). `cfg.effort` is
        // ignored — no CLI analog.
        let mut args: Vec<String> = Vec::new();

        if let Some(model) = cfg.model.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push("--model".to_string());
            args.push(model.to_string());
        }

        // User-configured launch flags (e.g. a yes-always default) go
        // after the model flag. Aider takes the prompt via stdin, so
        // there is no positional to precede.
        args.extend(cfg.extra_args.iter().cloned());

        // Prompt → stdin_seed (terminated with `\n` so the REPL treats
        // it as a submitted line). Trimming only the empty/whitespace
        // case keeps multi-line prompts intact for the runtime to write.
        let stdin_seed = cfg
            .prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                let mut bytes = s.as_bytes().to_vec();
                bytes.push(b'\n');
                bytes
            });

        Ok(CommandSpec {
            program: PathBuf::from(AIDER_BIN),
            args,
            env: Vec::new(),
            stdin_seed,
        })
    }

    fn status_patterns(&self) -> &[StatusPattern] {
        EMPTY_PATTERNS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::AgentAdapter;

    fn cfg() -> AgentSessionConfig {
        AgentSessionConfig {
            adapter: AgentAdapter::Aider,
            worktree_path: PathBuf::from("/tmp"),
            prompt: None,
            model: None,
            effort: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            custom_command: None,
        }
    }

    #[tokio::test]
    async fn detect_returns_a_bool_without_panicking() {
        // Aider may not be installed; contract is "Ok(_) + never panics".
        let _ = AiderAdapter.detect().await.expect("detect ok");
    }

    #[test]
    fn id_and_name_are_stable() {
        let a = AiderAdapter;
        assert_eq!(a.id(), "aider");
        assert_eq!(a.name(), "Aider");
    }

    #[test]
    fn build_command_minimal() {
        let spec = AiderAdapter.build_command(&cfg()).unwrap();
        assert_eq!(spec.program, PathBuf::from("aider"));
        assert!(
            spec.args.is_empty(),
            "expected no args, got {:?}",
            spec.args
        );
        assert!(spec.env.is_empty());
        assert!(
            spec.stdin_seed.is_none(),
            "expected no stdin_seed, got {:?}",
            spec.stdin_seed
        );
    }

    #[test]
    fn build_command_includes_model_when_set() {
        let mut c = cfg();
        c.model = Some("claude-3-5-sonnet".into());
        let spec = AiderAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.args,
            vec!["--model".to_string(), "claude-3-5-sonnet".into()]
        );
    }

    #[test]
    fn build_command_ignores_effort() {
        // Aider CLI has no --effort analog. Locks the adapter contract:
        // ignored fields produce no args, no stdin_seed bytes.
        let mut c = cfg();
        c.effort = Some("high".into());
        let spec = AiderAdapter.build_command(&c).unwrap();
        assert!(spec.args.is_empty(), "got {:?}", spec.args);
        assert!(spec.stdin_seed.is_none());
    }

    #[test]
    fn build_command_prompt_becomes_stdin_seed() {
        let mut c = cfg();
        c.prompt = Some("hello".into());
        let spec = AiderAdapter.build_command(&c).unwrap();
        assert!(
            spec.args.is_empty(),
            "prompt should not produce argv, got {:?}",
            spec.args
        );
        assert_eq!(
            spec.stdin_seed.as_deref(),
            Some(b"hello\n".as_ref()),
            "prompt should arrive on stdin_seed terminated with newline"
        );
    }

    #[test]
    fn build_command_full_house() {
        let mut c = cfg();
        c.model = Some("claude-3-5-sonnet".into());
        c.effort = Some("high".into()); // ignored
        c.prompt = Some("ship the fix".into());
        let spec = AiderAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.args,
            vec!["--model".to_string(), "claude-3-5-sonnet".into()],
            "argv carries model only; prompt rides stdin_seed; effort dropped"
        );
        assert_eq!(spec.stdin_seed.as_deref(), Some(b"ship the fix\n".as_ref()));
    }

    #[test]
    fn build_command_skips_blank_fields() {
        // Empty + whitespace strings on all optional fields should
        // produce neither argv nor stdin_seed.
        let mut c = cfg();
        c.model = Some(String::new());
        c.prompt = Some("   ".into());
        let spec = AiderAdapter.build_command(&c).unwrap();
        assert!(spec.args.is_empty(), "got {:?}", spec.args);
        assert!(
            spec.stdin_seed.is_none(),
            "blank prompt must not seed PTY, got {:?}",
            spec.stdin_seed
        );
    }

    #[test]
    fn build_command_preserves_multiline_prompt() {
        // stdin_seed bytes flow verbatim into Aider's readline layer.
        // Each embedded `\n` submits that line as a discrete REPL
        // prompt; the trailing `\n` appended by build_command submits
        // the final line. Multi-line prompts therefore arrive as N
        // sequential submissions — correct Aider REPL behaviour. We do
        // NOT collapse or escape newlines because doing so would break
        // multi-step sequences a caller may have deliberately built
        // (e.g. "switch to /architect mode\nplan the refactor\n").
        let mut c = cfg();
        c.prompt = Some("first line\nsecond line".into());
        let spec = AiderAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.stdin_seed.as_deref(),
            Some(b"first line\nsecond line\n".as_ref())
        );
    }

    #[test]
    fn status_patterns_are_empty() {
        // Locks the "patterns deferred to step 14 dogfood capture"
        // decision. Flipping this to non-empty requires capturing real
        // Aider TUI bytes into a fixture first — see the step-5 journal
        // for the trap that taught the project to require that.
        assert!(AiderAdapter.status_patterns().is_empty());
    }
}
