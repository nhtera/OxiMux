//! AI commit-message generation lifecycle for the sparkles button.
//!
//! Owns the `AiState` enum + the `CommitArea` methods that drive
//! it (`set_staged_snapshot`, `is_generating_ai`,
//! `start_ai_generation`, `cancel_ai_generation`). Split out of
//! `commit_area.rs` so the main module stays under the workspace
//! file-size cap; the methods are still on `CommitArea` via a
//! re-opened `impl` block — call sites are unchanged.
//!
//! Dispatches through [`oximux_agents::commit_message::generate`]
//! which routes to one of three backends based on the user's
//! [`oximux_settings::CommitMessageAiSettings`]:
//!
//! - **Off**: sparkles button hides; this method is unreachable.
//! - **Heuristic**: pure local generator over the staged file list.
//!   Synchronous, microseconds, no network.
//! - **Agent**: spawn the configured CLI (claude / codex / custom),
//!   pipe the staged-diff prompt over stdin, parse the response.
//!   Async, seconds to tens of seconds, hits the configured agent.
//!
//! For Agent mode we fetch the staged-diff context lazily, only
//! when actually about to spawn the CLI — the heuristic path
//! doesn't need it.
//!
//! Cooperative cancellation: the spawned task holds an
//! `Arc<AtomicBool>` shared with the `AiState::Generating` variant.
//! Three sites set the flag:
//!
//! 1. The Stop button on the overlay → `cancel_ai_generation`.
//! 2. The InputEvent::Change observer in `CommitArea::new` →
//!    `area.ai_state.signal_cancel()` (user typed during gen).
//! 3. Entity drop — replacing `AiState` with `Idle` drops the
//!    held `Task<()>`, which gpui cancels.
//!
//! In agent mode the spawned CLI inherits cancellation via
//! `tokio::process::Command::kill_on_drop(true)` — dropping the
//! `Child` (which happens when the wait future is dropped on
//! cancel) sends `SIGKILL` to the CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{Context, Task, Window};
use oximux_agents::commit_message::{
    self, AgentConfig, AgentId, GenerateError, Mode, StagedContext,
};
use oximux_core::FileStatus;
use oximux_settings::{CommitMessageAiMode, CommitMessageAiSettings};

use crate::shell::source_control::commit_area::{CommitArea, CommitStatus};

/// AI generation lifecycle. `Idle` is the default; `Generating`
/// holds the cancel flag shared with the spawned task plus the
/// task handle (dropping the handle cancels gpui's await, but the
/// cancel flag is what the task body actually polls before
/// applying its result).
pub(in crate::shell::source_control) enum AiState {
    Idle,
    Generating {
        cancel: Arc<AtomicBool>,
        /// Held for the duration of the generation so dropping the
        /// `AiState` (e.g. on entity drop or explicit reset to
        /// `Idle`) cancels the task. The task itself ALSO checks
        /// the cancel flag before mutating; this field is the
        /// gpui-side cancel handle.
        _task: Task<()>,
    },
}

impl AiState {
    pub(in crate::shell::source_control) fn is_generating(&self) -> bool {
        matches!(self, AiState::Generating { .. })
    }

    /// Set the cancel flag on the in-flight generation (if any) so
    /// the task drops its result instead of inserting it.
    /// Idempotent: calling on `Idle` is a no-op.
    pub(in crate::shell::source_control) fn signal_cancel(&self) {
        if let AiState::Generating { cancel, .. } = self {
            cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// Spinner paint delay before computing the heuristic. Tuned so a
/// generation that completes in microseconds still shows the
/// overlay for at least one frame — without this the user sees a
/// glitch flash instead of a deliberate-looking AI surface.
const SPINNER_PAINT_DELAY: Duration = Duration::from_millis(80);

impl CommitArea {
    /// Replace the cached staged-file snapshot. Called by the
    /// panel's state observer once per poll tick with the
    /// staged-filtered file list. Equality-guarded so identical
    /// snapshots don't fire a spurious `cx.notify` (sparkles
    /// button only depends on non-empty / empty, so non-meaningful
    /// list reorderings cost nothing to ignore).
    pub fn set_staged_snapshot(
        &mut self,
        snapshot: Vec<FileStatus>,
        cx: &mut Context<Self>,
    ) {
        if self.staged_snapshot == snapshot {
            return;
        }
        self.staged_snapshot = snapshot;
        cx.notify();
    }

    /// True when the heuristic generator is currently running.
    pub fn is_generating_ai(&self) -> bool {
        self.ai_state.is_generating()
    }

    /// Cancel any in-flight AI generation. Idempotent; safe to
    /// call while `Idle`. Used by the Stop button on the overlay;
    /// the user-typing cancel path goes through the existing
    /// InputEvent::Change observer in `CommitArea::new`.
    pub fn cancel_ai_generation(&mut self, cx: &mut Context<Self>) {
        if !self.ai_state.is_generating() {
            return;
        }
        self.ai_state.signal_cancel();
        // Drop the task immediately so the overlay disappears
        // synchronously rather than waiting for the task to
        // observe the cancel flag and self-clear.
        self.ai_state = AiState::Idle;
        cx.notify();
    }

    /// True when the user's settings file has AI generation
    /// disabled (`mode = "off"`). The sparkles button uses this to
    /// hide itself entirely — disabled-with-tooltip would imply
    /// "you could click this" when actually nothing's wired.
    pub fn ai_disabled_by_settings(&self, cx: &gpui::App) -> bool {
        cx.try_global::<CommitMessageAiSettings>()
            .is_some_and(|s| matches!(s.mode, CommitMessageAiMode::Off))
    }

    /// Begin AI generation. No-op when already generating, when
    /// no staged files exist, or when a commit is in flight. The
    /// spawned task waits one spinner-paint frame, then dispatches
    /// to the user's configured mode (heuristic local-pure or
    /// agent CLI spawn), then checks the cancel flag before
    /// inserting into the textarea via `set_value` (`set_value`
    /// suppresses the Change observer, so the programmatic insert
    /// never trips our own cancel path).
    ///
    /// Failures (binary missing, agent timed out, parse error, …)
    /// surface in the status row via `CommitStatus::Failed` so
    /// the user sees what went wrong without poking at logs.
    pub fn start_ai_generation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ai_state.is_generating() {
            return;
        }
        if self.staged_snapshot.is_empty() {
            return;
        }
        if self.in_flight.load(Ordering::Relaxed) {
            return;
        }
        let mode = match mode_from_settings(cx) {
            ResolvedMode::Ok(Mode::Off) => return,
            ResolvedMode::Ok(m) => m,
            ResolvedMode::BadConfig(msg) => {
                // Surface in status row immediately — don't spawn
                // anything. User typed mode=agent but the config
                // is malformed; silent fallback would mask it.
                self.status = CommitStatus::Failed("generate".to_string(), msg);
                cx.notify();
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = cancel.clone();
        let staged = self.staged_snapshot.clone();
        let workdir = self.repo.workdir().to_path_buf();
        // `spawn_in(window, …)` so the resulting task's
        // `update_in` call has live `&mut Window` — needed because
        // `InputState::set_value` requires it.
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SPINNER_PAINT_DELAY).await;
            if cancel_for_task.load(Ordering::SeqCst) {
                let _ = this.update(cx, |area, cx| {
                    area.ai_state = AiState::Idle;
                    cx.notify();
                });
                return;
            }

            // Run the actual generation via the tokio runtime — the
            // heuristic path completes synchronously inside this
            // future; the agent path spawns a child process that
            // needs a real tokio reactor for stdin/stdout I/O. The
            // oneshot hand-off mirrors the pattern used by
            // `open_all_conflicts` in the panel so the same code
            // works in both production (tokio runtime entered) and
            // headless test contexts (silently no-ops).
            let result = run_generation(mode, staged, workdir, cancel_for_task.clone()).await;

            let _ = this.update_in(cx, |area, window, cx| {
                if cancel_for_task.load(Ordering::SeqCst) {
                    area.ai_state = AiState::Idle;
                    cx.notify();
                    return;
                }
                match result {
                    Ok(message) => {
                        area.message_state
                            .update(cx, |s, cx| s.set_value(message.clone(), window, cx));
                        // Mirror to disk explicitly — set_value
                        // suppresses the Change observer that
                        // normally schedules the debounced save.
                        area.schedule_draft_save(message, cx);
                    }
                    Err(err) => {
                        // Surface in the status row so the user
                        // sees the failure without digging into
                        // logs. "Generate failed: <detail>" matches
                        // the commit-failure status convention.
                        area.status = CommitStatus::Failed(
                            "generate".to_string(),
                            err.to_string(),
                        );
                    }
                }
                area.ai_state = AiState::Idle;
                cx.notify();
            });
        });
        self.ai_state = AiState::Generating {
            cancel,
            _task: task,
        };
        cx.notify();
    }
}

/// Outcome of resolving the user's settings to a runtime mode. The
/// `BadConfig` variant carries a user-facing message so an invalid
/// `agent_id` (typo, unknown CLI) lands in the status row instead of
/// silently downgrading the user's choice.
enum ResolvedMode {
    /// Ready-to-dispatch mode.
    Ok(Mode),
    /// User has `mode = "agent"` but the config is malformed.
    /// `start_ai_generation` surfaces the message via the status row
    /// rather than spawning anything.
    BadConfig(String),
}

/// Resolve the configured AI agent when (and only when) the user picked Agent
/// mode and the config is valid. `None` for Off / Heuristic / malformed config
/// — callers (e.g. the PR-draft path) fall back to a deterministic generator
/// rather than spawning a CLI. Shares the single settings→mode resolution in
/// [`mode_from_settings`] so the commit-message and PR-draft paths agree.
pub(in crate::shell::source_control) fn agent_config_from_settings(
    cx: &gpui::App,
) -> Option<AgentConfig> {
    match mode_from_settings(cx) {
        ResolvedMode::Ok(Mode::Agent(config)) => Some(config),
        _ => None,
    }
}

/// Read the user's `commit_message_ai.toml` settings (or the
/// in-memory default if the global isn't installed — defensive for
/// the test harness) and convert to the agents-crate [`Mode`].
///
/// Returns [`ResolvedMode::BadConfig`] when `mode = "agent"` but the
/// `agent_id` doesn't parse — silent downgrade to a different mode
/// would make the user think their CLI ran when it didn't. The
/// caller surfaces the BadConfig message in the status row.
fn mode_from_settings(cx: &gpui::App) -> ResolvedMode {
    let settings = cx
        .try_global::<CommitMessageAiSettings>()
        .cloned()
        .unwrap_or_default();
    match settings.mode {
        CommitMessageAiMode::Off => ResolvedMode::Ok(Mode::Off),
        CommitMessageAiMode::Heuristic => ResolvedMode::Ok(Mode::Heuristic),
        CommitMessageAiMode::Agent => {
            let Some(agent_id) = AgentId::parse(&settings.agent.agent_id) else {
                tracing::warn!(
                    agent_id = %settings.agent.agent_id,
                    "unknown agent_id in commit_message_ai.toml"
                );
                return ResolvedMode::BadConfig(format!(
                    "Unknown agent_id `{}` in commit_message_ai.toml",
                    settings.agent.agent_id
                ));
            };
            let to_option = |s: String| if s.is_empty() { None } else { Some(s) };
            ResolvedMode::Ok(Mode::Agent(AgentConfig {
                agent_id,
                model: settings.agent.model,
                thinking_level: to_option(settings.agent.thinking_level),
                custom_prompt: to_option(settings.agent.custom_prompt),
                custom_command: to_option(settings.agent.custom_command),
                agent_command_override: to_option(settings.agent.agent_command_override),
            }))
        }
    }
}

/// Run generation on the tokio runtime, returning the message text
/// (or a [`GenerateError`] suitable for the status row). For
/// agent mode this fetches the staged-diff context lazily (the
/// heuristic path doesn't need it). Uses a oneshot hand-off so it
/// works from both gpui's foreground executor and bare tokio test
/// contexts.
async fn run_generation(
    mode: Mode,
    staged: Vec<FileStatus>,
    workdir: PathBuf,
    cancel: Arc<AtomicBool>,
) -> Result<String, GenerateError> {
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            // No tokio runtime entered — happens in some headless
            // test contexts. The heuristic generator is pure
            // synchronous code, so call it directly here rather
            // than via the async dispatcher (which would need an
            // executor to drive its trivial async state machine).
            // Agent mode genuinely can't run without tokio.
            return match &mode {
                Mode::Heuristic => {
                    if staged.is_empty() {
                        Err(GenerateError::NothingStaged)
                    } else {
                        Ok(oximux_agents::commit_message_heuristic::generate_heuristic(
                            &staged,
                        ))
                    }
                }
                Mode::Off => Err(GenerateError::Disabled),
                Mode::Agent(_) => Err(GenerateError::Run(
                    oximux_agents::commit_message::GenerationError::SpawnFailed {
                        label: "agent".to_string(),
                        message: "no tokio runtime available".to_string(),
                    },
                )),
            };
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle.spawn(async move {
        let context = match &mode {
            Mode::Agent(_) => match oximux_git::staged_context::fetch(&workdir).await {
                Ok(Some(c)) => StagedContext {
                    branch: c.branch,
                    summary: c.summary,
                    patch: c.patch,
                    workdir: workdir.clone(),
                },
                Ok(None) => {
                    let _ = tx.send(Err(GenerateError::NothingStaged));
                    return;
                }
                Err(err) => {
                    let _ = tx.send(Err(GenerateError::Run(
                        oximux_agents::commit_message::GenerationError::SpawnFailed {
                            label: "agent".to_string(),
                            message: format!("staged diff fetch failed: {err}"),
                        },
                    )));
                    return;
                }
            },
            // Heuristic + Off don't consult the context; pass a
            // placeholder so the dispatcher's signature is honoured
            // without an extra shellout.
            _ => placeholder_context(&workdir),
        };
        let result = commit_message::generate(&mode, &staged, context, cancel).await;
        let _ = tx.send(result);
    });
    match rx.await {
        Ok(r) => r,
        Err(_) => Err(GenerateError::Run(
            oximux_agents::commit_message::GenerationError::SpawnFailed {
                label: "agent".to_string(),
                message: "generation task dropped without producing a result".to_string(),
            },
        )),
    }
}

fn placeholder_context(workdir: &std::path::Path) -> StagedContext {
    StagedContext {
        branch: None,
        summary: String::new(),
        patch: String::new(),
        workdir: workdir.to_path_buf(),
    }
}
