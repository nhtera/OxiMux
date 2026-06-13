//! Custom-command adapter.
//!
//! The escape hatch for any CLI agent that doesn't ship a first-class
//! adapter. The user supplies `program + args` in the launch dialog; the
//! runtime spawns it in a PTY just like the bundled adapters. Status
//! detection falls through to the `StatusMachine` defaults: any output =>
//! `Running`, idle decay => `Idle`, exit-code => `Done`/`Failed`. Without
//! a known prompt regex the dialog can't distinguish `WaitingForInput`
//! from `Running`; the user clicks the badge to override when needed.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::path::PathBuf;

use crate::cli::adapter::EMPTY_PATTERNS;
use crate::cli::{CliAgentAdapter, CommandSpec, StatusPattern};
use crate::runtime::AgentSessionConfig;

/// Stateless adapter — one instance lives in the runtime registry and is
/// reused across all custom-command sessions. The user's `program + args`
/// travel on `AgentSessionConfig::custom_command`, not on the adapter
/// itself, so adapter instances stay shareable.
#[derive(Debug, Clone, Copy, Default)]
pub struct CustomCommandAdapter;

#[async_trait]
impl CliAgentAdapter for CustomCommandAdapter {
    fn id(&self) -> &'static str {
        "custom"
    }

    fn name(&self) -> &'static str {
        "Custom command"
    }

    /// Always available — the user's chosen binary is checked when the
    /// runtime spawns it (a missing program surfaces as a `Failed` status,
    /// not a launch-dialog filter). Hiding "Custom" would defeat its
    /// purpose as the universal escape hatch.
    async fn detect(&self) -> Result<bool> {
        Ok(true)
    }

    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec> {
        let (program, args) = cfg
            .custom_command
            .as_ref()
            .ok_or_else(|| anyhow!("Custom adapter requires AgentSessionConfig::custom_command"))?;
        let program = program.trim();
        if program.is_empty() {
            return Err(anyhow!(
                "Custom adapter program is empty; pick a binary or shell command"
            ));
        }
        Ok(CommandSpec {
            program: PathBuf::from(program),
            args: args.clone(),
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
    use oximux_core::AgentAdapter;
    use std::path::PathBuf;

    fn cfg_with(custom: Option<(String, Vec<String>)>) -> AgentSessionConfig {
        AgentSessionConfig {
            adapter: AgentAdapter::Custom,
            worktree_path: PathBuf::from("/tmp"),
            prompt: None,
            model: None,
            effort: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            custom_command: custom,
        }
    }

    #[tokio::test]
    async fn detect_is_always_true() {
        assert!(CustomCommandAdapter.detect().await.unwrap());
    }

    #[test]
    fn id_and_name_are_stable() {
        let a = CustomCommandAdapter;
        assert_eq!(a.id(), "custom");
        assert_eq!(a.name(), "Custom command");
    }

    #[test]
    fn status_patterns_are_empty() {
        assert!(CustomCommandAdapter.status_patterns().is_empty());
    }

    #[test]
    fn build_command_happy_path() {
        let cfg = cfg_with(Some(("echo".into(), vec!["hello".into()])));
        let spec = CustomCommandAdapter.build_command(&cfg).unwrap();
        assert_eq!(spec.program, PathBuf::from("echo"));
        assert_eq!(spec.args, vec!["hello".to_string()]);
        assert!(spec.env.is_empty());
        assert!(spec.stdin_seed.is_none());
    }

    #[test]
    fn build_command_no_args_is_ok() {
        let cfg = cfg_with(Some(("/bin/sh".into(), Vec::new())));
        let spec = CustomCommandAdapter.build_command(&cfg).unwrap();
        assert_eq!(spec.program, PathBuf::from("/bin/sh"));
        assert!(spec.args.is_empty());
    }

    #[test]
    fn build_command_missing_config_errors() {
        let cfg = cfg_with(None);
        let err = CustomCommandAdapter.build_command(&cfg).unwrap_err();
        assert!(err.to_string().contains("custom_command"));
    }

    #[test]
    fn build_command_empty_program_errors() {
        let cfg = cfg_with(Some(("   ".into(), Vec::new())));
        let err = CustomCommandAdapter.build_command(&cfg).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("empty"));
    }
}
