//! Codex adapter (`codex` CLI from OpenAI).
//!
//! Spawns the `codex` interactive TUI in a PTY. Optional `-m <model>` from
//! `AgentSessionConfig::model`; prompt as trailing positional. No
//! `--effort` (no CLI analog — adapter ignores per the `runtime.rs`
//! `AgentSessionConfig::effort` contract). No `--ask-for-approval` and no
//! `--sandbox` overrides: the user's `~/.codex/config.toml` owns approval
//! cadence and sandbox policy, matching the "no fabricated env vars / no
//! paternalistic defaults" rule from the v0.9 retro.
//!
//! ## Why empty `status_patterns()`
//!
//! Codex TUI approval prompts vary by `--ask-for-approval` × `--sandbox`
//! combinations, and the real byte sequences (with ANSI SGR wrapping)
//! have not been captured. The Phase 3 step-5 journal lesson is
//! unambiguous: writing regex against an imagined haystack is exactly
//! the "tests pass / runtime broken" failure mode this project guards
//! against (see `docs/journals/260520-1545-phase-03-step-05-claude-code-adapter.md`).
//! Pattern calibration is deferred to dogfood week 18 (step 14) when
//! real TUI bytes get captured into fixtures.
//!
//! Until then the `StatusMachine` fallback supplies Running / Idle /
//! Done / Failed transitions; `NeedsApproval` relies on the manual
//! badge-click `force()` override (which the user does anyway when
//! regex misclassifies).

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use crate::cli::adapter::EMPTY_PATTERNS;
use crate::cli::detect::which_on_path;
use crate::cli::{CliAgentAdapter, CommandSpec, StatusPattern};
use crate::runtime::AgentSessionConfig;

/// Bare-name binary, resolved via PATH. Centralized so step 8 (registry)
/// can later surface a settings override without grepping the body.
const CODEX_BIN: &str = "codex";

/// Stateless adapter — one instance lives in the runtime registry and is
/// reused across all Codex sessions. Per-session config (model, prompt,
/// worktree) flows through `AgentSessionConfig`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

#[async_trait]
impl CliAgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn name(&self) -> &'static str {
        "Codex"
    }

    async fn detect(&self) -> Result<bool> {
        Ok(which_on_path(CODEX_BIN).await)
    }

    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec> {
        // Interactive PTY launch — running `codex` with no subcommand
        // drops into the TUI; the non-interactive form is `codex exec`
        // (out of scope here).
        //
        // Argv order:
        //   -m <model>           (short flag is the documented form)
        //   <prompt>             (positional, trailing)
        //
        // cfg.effort is intentionally ignored — codex CLI has no
        // reasoning-effort analog. The "adapter ignores when CLI has no
        // analog" contract is locked by the build_command_ignores_effort
        // test below.
        let mut args: Vec<String> = Vec::new();

        if let Some(model) = cfg.model.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push("-m".to_string());
            args.push(model.to_string());
        }

        if let Some(prompt) = cfg.prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.to_string());
        }

        Ok(CommandSpec {
            program: PathBuf::from(CODEX_BIN),
            args,
            env: Vec::new(),
            stdin_seed: None,
        })
    }

    fn status_patterns(&self) -> &[StatusPattern] {
        EMPTY_PATTERNS
    }

    fn models(&self) -> &'static [&'static str] {
        // `-m <model>` slugs. No effort row — the codex CLI has no
        // reasoning-effort analog (see `build_command` note).
        &["gpt-5-codex", "o3"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::AgentAdapter;

    fn cfg() -> AgentSessionConfig {
        AgentSessionConfig {
            adapter: AgentAdapter::Codex,
            worktree_path: PathBuf::from("/tmp"),
            prompt: None,
            model: None,
            effort: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            custom_command: None,
        }
    }

    #[tokio::test]
    async fn detect_returns_a_bool_without_panicking() {
        // Cannot assert truthy: dev box has codex installed, CI may not.
        // Contract is "Ok(_) + never panics".
        let _ = CodexAdapter.detect().await.expect("detect ok");
    }

    #[test]
    fn id_and_name_are_stable() {
        let a = CodexAdapter;
        assert_eq!(a.id(), "codex");
        assert_eq!(a.name(), "Codex");
    }

    #[test]
    fn build_command_minimal() {
        let spec = CodexAdapter.build_command(&cfg()).unwrap();
        assert_eq!(spec.program, PathBuf::from("codex"));
        assert!(
            spec.args.is_empty(),
            "expected no args, got {:?}",
            spec.args
        );
        assert!(spec.env.is_empty());
        assert!(spec.stdin_seed.is_none());
    }

    #[test]
    fn build_command_includes_model_when_set() {
        let mut c = cfg();
        c.model = Some("o3".into());
        let spec = CodexAdapter.build_command(&c).unwrap();
        assert_eq!(spec.args, vec!["-m".to_string(), "o3".into()]);
    }

    #[test]
    fn build_command_ignores_effort() {
        // Codex CLI has no --effort analog. Locks the adapter contract
        // that ignored fields produce no args (silently dropping
        // `cfg.effort = Some("high")` here is intentional).
        let mut c = cfg();
        c.effort = Some("high".into());
        let spec = CodexAdapter.build_command(&c).unwrap();
        assert!(
            spec.args.is_empty(),
            "effort should not produce args, got {:?}",
            spec.args
        );
    }

    #[test]
    fn build_command_includes_prompt_as_positional() {
        let mut c = cfg();
        c.prompt = Some("refactor this module".into());
        let spec = CodexAdapter.build_command(&c).unwrap();
        assert_eq!(spec.args, vec!["refactor this module".to_string()]);
    }

    #[test]
    fn build_command_full_house() {
        let mut c = cfg();
        c.model = Some("o3".into());
        c.effort = Some("high".into()); // ignored
        c.prompt = Some("ship the bug fix".into());
        let spec = CodexAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.args,
            vec!["-m".to_string(), "o3".into(), "ship the bug fix".into(),],
            "expected model then prompt; effort silently dropped"
        );
    }

    #[test]
    fn build_command_skips_blank_fields() {
        let mut c = cfg();
        c.model = Some(String::new());
        c.prompt = Some("   ".into());
        let spec = CodexAdapter.build_command(&c).unwrap();
        assert!(
            spec.args.is_empty(),
            "blank fields should not produce args, got {:?}",
            spec.args
        );
    }

    #[test]
    fn status_patterns_are_empty() {
        // Locks the "patterns deferred to step 14 dogfood capture"
        // decision documented at the top of this file. Flipping this
        // to non-empty requires capturing real Codex TUI bytes into a
        // fixture first — see the journal entry for the trap that
        // taught the project to require that.
        assert!(CodexAdapter.status_patterns().is_empty());
    }
}
