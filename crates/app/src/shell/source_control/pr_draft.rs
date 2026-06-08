//! Agent-backed PR title/body drafting.
//!
//! The Create-PR dialog's "Draft from commits" button asks the host to fill the
//! title + body. When the user has the commit-message AI set to Agent mode, we
//! reuse that same generator over the branch-range diff (`<base>..HEAD`) instead
//! of the staged diff: the agent returns a subject + body that map directly onto
//! a PR title + body. Off / Heuristic users — and any failure (no base, empty
//! range, agent error, no tokio runtime) — fall back to the deterministic
//! commit-subject draft in [`oximux_git::pr_context::draft_from_commits`].
//!
//! Only the *generation source* differs from the deterministic path; the dialog
//! handshake (`set_generating` → `apply_generated`) is unchanged.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use oximux_agents::commit_message::{self, AgentConfig, Mode, StagedContext, split_message};

/// Generate a PR `(title, body)` from the branch-range diff via the configured
/// agent. Returns `None` on any failure so the caller can fall back to the
/// deterministic commit-subject draft. `cancel` is shared with the host so a
/// dismissed dialog aborts the in-flight agent CLI mid-run.
pub(in crate::shell::source_control) async fn generate_pr_draft(
    config: AgentConfig,
    workdir: PathBuf,
    cancel: Arc<AtomicBool>,
) -> Option<(String, String)> {
    let range = oximux_git::pr_context::fetch_range_context(&workdir).await?;
    let context = StagedContext {
        branch: range.branch,
        summary: range.summary,
        patch: range.patch,
        workdir,
    };
    // Agent mode ignores the file-list argument (it reasons over `context`); the
    // returned payload is a formatted `subject\n\nbody`, which split_message
    // re-separates into title + body.
    let message = commit_message::generate(&Mode::Agent(config), &[], context, cancel)
        .await
        .ok()?;
    let split = split_message(&message);
    Some((split.subject, split.body))
}
