//! omp adapter (`omp` CLI, a Pi fork).
//!
//! Spawns the `omp` interactive TUI in a PTY. This is the *terminal* face of
//! omp; its structured-chat face is `omp --mode rpc-ui`, which lives in
//! `thread/omp/` and never goes through this adapter.
//!
//! Registering omp here is what makes it **reachable** (the Pi phase-8
//! lesson): the tab `+` menu and the New Agent roster are built from the
//! registry, and `chat_capable`/`transport_for` are only consulted for ids
//! those lists produce.
//!
//! ## Why no `models()` / `efforts()`
//!
//! Same reason as Pi's adapter, same resolver lineage: omp's catalog is
//! per-user (which providers hold credentials), and a bare model id is a
//! fuzzy search PATTERN across every provider omp knows, so a static list of
//! bare ids could silently load a different provider's build. The chat
//! backend reads the live catalog and offers provider-qualified wires; the
//! terminal launch offers no model row rather than a hazardous one.
//!
//! ## Why empty `status_patterns()`
//!
//! omp's TUI prompt bytes have not been captured, and this project does not
//! write regex against an imagined haystack. The `StatusMachine` fallback
//! supplies Running / Idle / exit transitions, and the phase-2 status
//! extension supplies the rich readings.

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use crate::cli::adapter::EMPTY_PATTERNS;
use crate::cli::detect::which_on_path;
use crate::cli::{CliAgentAdapter, CommandSpec, StatusPattern};
use crate::runtime::AgentSessionConfig;

/// Bare-name binary, resolved via PATH.
const OMP_BIN: &str = "omp";

/// Stateless adapter — one instance lives in the runtime registry and is
/// reused across all omp terminal sessions.
#[derive(Debug, Clone, Copy, Default)]
pub struct OmpAdapter;

#[async_trait]
impl CliAgentAdapter for OmpAdapter {
    fn id(&self) -> &'static str {
        "omp"
    }

    fn name(&self) -> &'static str {
        "omp"
    }

    async fn detect(&self) -> Result<bool> {
        Ok(which_on_path(OMP_BIN).await)
    }

    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec> {
        // Argv order (`omp [options] [@files...] [messages...]`):
        //   --resume <id>    resume, when one was requested
        //   --model <m>      only when the caller supplied one
        //   <extra args>     user-configured launch flags
        //   <prompt>         positional, trailing
        let mut args: Vec<String> = Vec::new();

        // Resume takes omp's session **id** (`--resume`; `--session` is its
        // alias). The interactive TUI resume is allowed to carry whatever id
        // the picker recorded — the picker's own seam
        // (`import_resume_command`) already refused anything that is not a
        // full canonical UUID, and an id omp cannot resolve exits 1 with a
        // message the user can read in the terminal.
        if let Some(id) = cfg.resumption.source_id() {
            args.push("--resume".to_string());
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
            program: PathBuf::from(OMP_BIN),
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
            adapter: AgentAdapter::Omp,
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
        let spec = OmpAdapter.build_command(&cfg()).expect("build");
        assert_eq!(spec.program, PathBuf::from("omp"));
        assert!(spec.args.is_empty(), "got {:?}", spec.args);
    }

    #[test]
    fn resume_uses_the_canonical_resume_flag_with_the_id() {
        let c = AgentSessionConfig {
            resumption: SessionResumption::Resume {
                id: "01a037fe-2a2b-76e1-8d1f-db954755a79c".into(),
            },
            ..cfg()
        };
        let spec = OmpAdapter.build_command(&c).expect("build");
        assert_eq!(
            spec.args,
            vec!["--resume", "01a037fe-2a2b-76e1-8d1f-db954755a79c"],
            "id, never a file path — and `--resume`, the canonical spelling"
        );
    }

    #[test]
    fn model_extra_args_and_prompt_keep_omps_argv_order() {
        let c = AgentSessionConfig {
            model: Some("openai-codex/gpt-5.6-sol".into()),
            extra_args: vec!["--no-color".into()],
            prompt: Some("summarize this repo".into()),
            ..cfg()
        };
        let spec = OmpAdapter.build_command(&c).expect("build");
        assert_eq!(
            spec.args,
            vec!["--model", "openai-codex/gpt-5.6-sol", "--no-color", "summarize this repo"]
        );
    }
}
