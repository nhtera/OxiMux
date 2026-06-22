//! Claude Code adapter.
//!
//! Spawns the `claude` CLI in a PTY for interactive use. Status detection
//! uses a starter regex set focused on the two prompts that map cleanly to
//! `NeedsApproval` — the workspace-trust dialog at first launch and the
//! tool-permission prompt during a session. The generic between-turns
//! `WaitingForInput` prompt is intentionally NOT pattern-matched yet; the
//! ANSI-padded TUI prompt is brittle to lock in pre-dogfood (see step 14).
//!
//! All other transitions fall through to `StatusMachine` defaults: any
//! output → `Running`, idle decay → `Idle`, exit code → `Done`/`Failed`.

use anyhow::Result;
use async_trait::async_trait;
use oximux_core::AgentStatus;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::cli::detect::which_on_path;
use crate::cli::{CliAgentAdapter, CommandSpec, StatusPattern};
use crate::runtime::AgentSessionConfig;

/// The binary name resolved on PATH. Centralized so the future "user has
/// `claude` aliased or installed under a non-standard name" case becomes a
/// settings override in step 8 without grepping the adapter body.
const CLAUDE_BIN: &str = "claude";

/// Stateless adapter — one instance lives in the runtime registry and is
/// reused across all Claude Code sessions. Per-session config (model,
/// effort, prompt, worktree) flows through `AgentSessionConfig`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCodeAdapter;

/// Lazily-initialized `&'static [StatusPattern]`. Built once on first
/// `status_patterns()` call so the regexes compile exactly once for the
/// life of the process.
fn patterns() -> &'static [StatusPattern] {
    static PATTERNS: OnceLock<Vec<StatusPattern>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            // Order is significant: first match wins. Workspace-trust check
            // appears once at first launch and is more specific; the generic
            // "Do you want to proceed/continue" covers ongoing tool prompts.
            //
            // NOTE: no leading `\b`. The claude TUI color-wraps prompts with
            // ANSI SGR sequences ending in a letter (`m`, `K`, `J`), all of
            // which are word chars under `regex::bytes`. A leading `\b` would
            // silently miss every colored prompt — exactly the "tests-pass /
            // runtime-broken" failure mode this project guards against.
            // Trailing `\b` is sufficient to block accidental concatenations
            // like "folderless" / "proceedwith".
            //
            // L1 (review 260520-1530): tool-approval can match agent
            // conversational output (e.g. "Do you want to proceed with X" in
            // a summary). Acceptable for v1: StatusMachine clears the ring on
            // blocking entry, so the next non-prompt chunk unsticks the
            // badge; manual badge-click `force()` covers residual FPs.
            // Calibrate further during dogfood week 18 (phase-03 step 14).
            vec![
                StatusPattern::new(
                    r"(?i)Do you trust the files in this folder\b",
                    AgentStatus::NeedsApproval("workspace trust".into()),
                )
                .expect("workspace-trust regex must compile"),
                StatusPattern::new(
                    r"(?i)Do you want to (?:proceed|continue)\b",
                    AgentStatus::NeedsApproval("tool approval".into()),
                )
                .expect("tool-approval regex must compile"),
            ]
        })
        .as_slice()
}

#[async_trait]
impl CliAgentAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    async fn detect(&self) -> Result<bool> {
        Ok(which_on_path(CLAUDE_BIN).await)
    }

    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec> {
        // Interactive PTY launch — claude defaults to interactive when stdin
        // is a TTY; no `-p/--print` here. Order of optional flags:
        //   --model <m>
        //   --effort <e>
        //   <prompt>             (positional, trailing)
        // TODO(v1.1): pass `--add-dir <parent_repo>` when the worktree is
        // nested under a project root the user wants editable. v1 worktrees
        // are self-contained per ADR-002.
        let mut args: Vec<String> = Vec::new();

        // Resume / fork flags lead so the session selection is unambiguous.
        // `--resume <id>` continues the prior conversation; `--fork-session`
        // branches a copy that leaves the original intact. The CLI rejects
        // `--session-id` alongside `--resume` UNLESS `--fork-session` is also
        // given, so the fresh id is fork-only — a plain resume reuses the
        // original session id and must not pass `--session-id`.
        if let Some(id) = cfg.resumption.source_id() {
            args.push("--resume".to_string());
            args.push(id.to_string());
            if cfg.resumption.is_fork() {
                args.push("--fork-session".to_string());
                args.push("--session-id".to_string());
                args.push(uuid::Uuid::new_v4().to_string());
            }
        }

        if let Some(model) = cfg.model.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push("--model".to_string());
            args.push(model.to_string());
        }

        if let Some(effort) = cfg.effort.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push("--effort".to_string());
            args.push(effort.to_string());
        }

        // User-configured launch flags (e.g. a skip-permissions default)
        // go after model/effort and before the positional prompt.
        args.extend(cfg.extra_args.iter().cloned());

        if let Some(prompt) = cfg.prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.to_string());
        }

        Ok(CommandSpec {
            program: PathBuf::from(CLAUDE_BIN),
            args,
            env: Vec::new(),
            stdin_seed: None,
        })
    }

    fn status_patterns(&self) -> &[StatusPattern] {
        patterns()
    }

    fn models(&self) -> &'static [&'static str] {
        // Documented `--model` aliases the CLI resolves to the latest of
        // each tier. Aliases (not pinned IDs) so they keep tracking the
        // newest model without a code change.
        &["opus", "sonnet", "haiku"]
    }

    fn efforts(&self) -> &'static [&'static str] {
        &["high", "medium", "low"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::AgentAdapter;

    fn cfg() -> AgentSessionConfig {
        AgentSessionConfig {
            adapter: AgentAdapter::ClaudeCode,
            worktree_path: PathBuf::from("/tmp"),
            prompt: None,
            model: None,
            effort: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            custom_command: None,
            resumption: oximux_core::SessionResumption::None,
        }
    }

    #[tokio::test]
    async fn detect_returns_a_bool_without_panicking() {
        // Cannot assert truthy: dev box has `claude` on PATH but CI may not.
        // Contract is "Ok(_) + never panics".
        let _ = ClaudeCodeAdapter.detect().await.expect("detect ok");
    }

    #[test]
    fn id_and_name_are_stable() {
        let a = ClaudeCodeAdapter;
        assert_eq!(a.id(), "claude-code");
        assert_eq!(a.name(), "Claude Code");
    }

    #[test]
    fn build_command_minimal() {
        let spec = ClaudeCodeAdapter.build_command(&cfg()).unwrap();
        assert_eq!(spec.program, PathBuf::from("claude"));
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
        c.model = Some("claude-opus-4-7".into());
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.args,
            vec!["--model".to_string(), "claude-opus-4-7".into()]
        );
    }

    #[test]
    fn build_command_includes_effort_when_set() {
        let mut c = cfg();
        c.effort = Some("high".into());
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert_eq!(spec.args, vec!["--effort".to_string(), "high".into()]);
    }

    #[test]
    fn build_command_includes_prompt_as_positional() {
        let mut c = cfg();
        c.prompt = Some("hello world".into());
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert_eq!(spec.args, vec!["hello world".to_string()]);
    }

    #[test]
    fn build_command_includes_resume_flag() {
        let mut c = cfg();
        c.resumption = oximux_core::SessionResumption::Resume { id: "abc".into() };
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        // Plain resume = `--resume <id>` ONLY: no --fork-session, and crucially
        // no --session-id (the CLI rejects it without --fork-session).
        assert_eq!(&spec.args[0..2], &["--resume".to_string(), "abc".into()]);
        assert!(!spec.args.iter().any(|a| a == "--fork-session"));
        assert!(
            !spec.args.iter().any(|a| a == "--session-id"),
            "plain resume must not pass --session-id"
        );
    }

    #[test]
    fn build_command_includes_fork_session_flag() {
        let mut c = cfg();
        c.resumption = oximux_core::SessionResumption::Fork { id: "x".into() };
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        // Fork = resume + --fork-session, before the fresh --session-id.
        assert_eq!(&spec.args[0..3], &["--resume".to_string(), "x".into(), "--fork-session".into()]);
        assert_eq!(spec.args.get(3).map(String::as_str), Some("--session-id"));
        assert!(spec.args.get(4).is_some_and(|v| !v.is_empty()), "uuid follows --session-id");
    }

    #[test]
    fn build_command_full_house() {
        let mut c = cfg();
        c.model = Some("claude-opus-4-7".into());
        c.effort = Some("high".into());
        c.prompt = Some("refactor this".into());
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.args,
            vec![
                "--model".to_string(),
                "claude-opus-4-7".into(),
                "--effort".into(),
                "high".into(),
                "refactor this".into(),
            ],
            "expected model, effort, then prompt as trailing positional"
        );
    }

    #[test]
    fn build_command_appends_extra_args_before_prompt() {
        // Launch-config flags land after model/effort and before the
        // positional prompt, so claude parses them as flags not as the
        // prompt text.
        let mut c = cfg();
        c.model = Some("opus".into());
        c.extra_args = vec!["--dangerously-skip-permissions".into()];
        c.prompt = Some("do it".into());
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert_eq!(
            spec.args,
            vec![
                "--model".to_string(),
                "opus".into(),
                "--dangerously-skip-permissions".into(),
                "do it".into(),
            ]
        );
    }

    #[test]
    fn build_command_extra_args_only() {
        let mut c = cfg();
        c.extra_args = vec!["--foo".into(), "bar".into()];
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert_eq!(spec.args, vec!["--foo".to_string(), "bar".into()]);
    }

    #[test]
    fn build_command_skips_blank_fields() {
        // Empty/whitespace strings on optional fields should not emit flags.
        let mut c = cfg();
        c.model = Some(String::new());
        c.effort = Some("   ".into());
        c.prompt = Some(" ".into());
        let spec = ClaudeCodeAdapter.build_command(&c).unwrap();
        assert!(
            spec.args.is_empty(),
            "blank fields should not produce args, got {:?}",
            spec.args
        );
    }

    #[test]
    fn status_patterns_compile_and_are_nonempty() {
        let pats = ClaudeCodeAdapter.status_patterns();
        assert_eq!(pats.len(), 2, "expected 2 starter patterns");
        // Order contract: workspace-trust first, tool-approval second.
        match &pats[0].transition {
            AgentStatus::NeedsApproval(reason) => assert_eq!(reason, "workspace trust"),
            other => panic!("pattern[0] not NeedsApproval(workspace trust): {other:?}"),
        }
        match &pats[1].transition {
            AgentStatus::NeedsApproval(reason) => assert_eq!(reason, "tool approval"),
            other => panic!("pattern[1] not NeedsApproval(tool approval): {other:?}"),
        }
    }

    #[test]
    fn status_patterns_match_known_approval_prompts() {
        let pats = ClaudeCodeAdapter.status_patterns();
        // Sample text the real CLI emits (case-insensitive guard).
        let trust = b"  Do you trust the files in this folder?";
        let proceed = b"Do you want to proceed?\n  1. Yes\n  2. No";
        let cont = b"Do you want to continue with this edit?";
        assert!(
            pats[0].regex.is_match(trust),
            "workspace-trust did not match"
        );
        assert!(
            pats[1].regex.is_match(proceed),
            "tool-approval (proceed) did not match"
        );
        assert!(
            pats[1].regex.is_match(cont),
            "tool-approval (continue) did not match"
        );
    }

    #[test]
    fn status_patterns_match_ansi_wrapped_prompts() {
        // M1 (review 260520-1530): regression test for the leading-`\b`
        // trap. The live TUI color-wraps prompts with SGR sequences ending
        // in `m` (a word char); a leading word boundary would silently
        // miss every colored prompt.
        let pats = ClaudeCodeAdapter.status_patterns();
        let trust_ansi = b"\x1b[1;33mDo you trust the files in this folder?\x1b[0m";
        let proceed_ansi = b"\x1b[1;33mDo you want to proceed?\x1b[0m";
        let cont_ansi = b"\x1b[33;1mDo you want to continue?\x1b[0m";
        assert!(
            pats[0].regex.is_match(trust_ansi),
            "workspace-trust missed ANSI-wrapped"
        );
        assert!(
            pats[1].regex.is_match(proceed_ansi),
            "tool-approval (proceed) missed ANSI-wrapped"
        );
        assert!(
            pats[1].regex.is_match(cont_ansi),
            "tool-approval (continue) missed ANSI-wrapped"
        );
    }

    #[test]
    fn status_patterns_do_not_match_unrelated_output() {
        // Guardrail against over-eager regex pinning the badge to NeedsApproval
        // during normal turn output.
        let pats = ClaudeCodeAdapter.status_patterns();
        let chatter = b"Edited 3 files, ran tests. Next step: write the docstring.";
        assert!(!pats[0].regex.is_match(chatter));
        assert!(!pats[1].regex.is_match(chatter));
    }
}
