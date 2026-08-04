//! The headless [`ScheduleFirer`]: a scheduled fire spawns straight into the
//! registry through the same [`HeadlessLauncher`] the CreateSession RPC uses,
//! then sends the schedule's prompt as the session's first message. No tab,
//! no keep-awake — a server stays awake on its own.

use std::sync::Arc;

use oximux_agents::schedule::{FireOutcome, Schedule, ScheduleFirer, ScheduleTarget};
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{LaunchError, SessionLauncher};

use super::launcher::HeadlessLauncher;

pub struct ServeFirer {
    launcher: Arc<HeadlessLauncher>,
    registry: Arc<SessionRegistry>,
}

impl ServeFirer {
    pub fn new(launcher: Arc<HeadlessLauncher>, registry: Arc<SessionRegistry>) -> Self {
        Self { launcher, registry }
    }
}

#[async_trait::async_trait]
impl ScheduleFirer for ServeFirer {
    async fn fire(&self, schedule: &Schedule, target: &ScheduleTarget) -> FireOutcome {
        match target {
            ScheduleTarget::NewSession => {}
            ScheduleTarget::ExistingSession(_) => {
                return FireOutcome::Failed {
                    session_id: None,
                    detail: "firing into an existing session is not supported yet".into(),
                };
            }
        }
        let session_id =
            match self.launcher.create(&schedule.cwd, schedule.agent_id.as_deref()).await {
                Ok(session_id) => session_id,
                // Draining: the host is shutting down. The occurrence stays due
                // for whichever host boots next.
                Err(LaunchError::Unavailable) => return FireOutcome::NotNow,
                Err(err) => {
                    return FireOutcome::Failed {
                        session_id: None,
                        detail: launch_error_detail(&err).to_string(),
                    };
                }
            };
        // The launcher registered the session and started its pump; the prompt
        // goes in like any remote prompt would. A session that opened but
        // cannot take the message still names itself in the run row, so
        // `schedule logs` points at something inspectable.
        let Some(handle) = self.registry.get(&session_id) else {
            return FireOutcome::Failed {
                session_id: Some(session_id),
                detail: "the session opened but exited before the prompt could be sent".into(),
            };
        };
        match handle.send_prompt(&schedule.prompt, &[]) {
            Ok(()) => FireOutcome::Completed { session_id: Some(session_id) },
            Err(err) => {
                tracing::warn!(%err, session_id, "scheduled prompt could not be delivered");
                FireOutcome::Failed {
                    session_id: Some(session_id),
                    detail: "the session opened but the prompt could not be delivered".into(),
                }
            }
        }
    }
}

/// A launch failure as run history should read it — the serve launcher's own
/// vocabulary (no default-agent fallback here: an unnamed agent is Claude, a
/// named-but-unknown one is refused as `Failed`).
fn launch_error_detail(err: &LaunchError) -> &'static str {
    match err {
        LaunchError::BadWorkingDirectory => "that working directory is not usable",
        LaunchError::Failed => "the agent could not be started (unknown agent id, or its CLI is missing)",
        // Handled as NotNow above; total match.
        LaunchError::Unavailable => "the host could not start a session right now",
    }
}
