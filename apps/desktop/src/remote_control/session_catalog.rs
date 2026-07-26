//! The desktop's answer to "what sessions exist, and can you open one?".
//!
//! The registry only holds sessions whose chat views have been built, and the
//! desktop builds a project's views the first time that project is shown. So a
//! remote client saw only the sessions belonging to projects the desktop happened
//! to have visited this run — after a restart, one project's worth, or none. From
//! the phone that reads as the sessions having been deleted.
//!
//! This closes it from the other end. [`DesktopSessionCatalog::dormant`] reads
//! the persisted layout so the list is complete without building anything, and
//! `open` builds the owning project's panes on demand, which constructs its chat
//! views and registers them. The user's own view does not change: building a
//! project's panes and *activating* that project are deliberately separate (see
//! `WorkspaceRoot::build_project_panes_if_absent`).
//!
//! Not solved by building every project at startup: that spawns an agent process
//! per session per launch, nearly all of which are never opened.

use std::path::PathBuf;
use std::sync::Arc;

use futures::channel::oneshot;
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::catalog::{DormantSession, OpenGate, SessionCatalog};

use crate::session_restore::persisted_terminals::PersistedTabKind;

/// One request to build a session's project, and where to send the outcome.
pub struct OpenRequest {
    pub session_id: String,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Reads persisted layout to enumerate sessions, and asks the UI to build one.
pub struct DesktopSessionCatalog {
    /// Live sessions, so a dormant row is never emitted for one already running.
    registry: Arc<SessionRegistry>,
    /// Where the persisted layout is read from, and the window whose layout it is.
    settings: Arc<oximux_storage::SettingsRepo>,
    projects: Arc<oximux_storage::ProjectRepo>,
    window_id: String,
    /// Hands `open` requests to the UI thread, which owns pane construction.
    tx: tokio::sync::mpsc::Sender<OpenRequest>,
    /// Collapses concurrent opens of one session into a single build.
    gate: OpenGate,
}

impl DesktopSessionCatalog {
    pub fn new(
        registry: Arc<SessionRegistry>,
        settings: Arc<oximux_storage::SettingsRepo>,
        projects: Arc<oximux_storage::ProjectRepo>,
        window_id: String,
        tx: tokio::sync::mpsc::Sender<OpenRequest>,
    ) -> Self {
        Self { registry, settings, projects, window_id, tx, gate: OpenGate::new() }
    }

    /// Every persisted chat session, paired with the project that owns it.
    ///
    /// A chat with no agent session id has never completed a turn: there is
    /// nothing to resume and no stable id to name it by, so it is not offered.
    fn persisted(&self) -> Vec<(String, DormantSession)> {
        let Ok(projects) = self.projects.list_ordered(usize::from(u8::MAX)) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for project in projects {
            let snapshot = match crate::project_panes_factory::load_persisted_tabs(
                &self.settings,
                &project.id,
                &self.window_id,
            ) {
                crate::project_panes_factory::LoadedTabs::Snapshot(s) => s,
                _ => continue,
            };
            // `tabs` is the flat concatenation of every group's tabs, so this one
            // pass covers both the legacy single-group and multi-group layouts.
            for tab in &snapshot.tabs {
                let PersistedTabKind::AgentChat { cwd, model, session_id, .. } = &tab.kind else {
                    continue;
                };
                let Some(session_id) = session_id.clone().filter(|id| !id.is_empty()) else {
                    continue;
                };
                found.push((
                    project.id.clone(),
                    DormantSession {
                        session_id,
                        title: Some(tab.label.clone()).filter(|l| !l.is_empty()),
                        model: model.clone(),
                        cwd: Some(PathBuf::from(cwd)),
                    },
                ));
            }
        }
        found
    }
}

#[async_trait::async_trait]
impl SessionCatalog for DesktopSessionCatalog {
    fn dormant(&self) -> Vec<DormantSession> {
        self.persisted()
            .into_iter()
            .map(|(_, session)| session)
            // The registry owns anything it holds — a dormant row for a live
            // session would show it twice with contradictory status.
            .filter(|session| self.registry.get(&session.session_id).is_none())
            .collect()
    }

    async fn open(&self, session_id: &str) -> Result<(), String> {
        // Already built — by an earlier request, or by the user opening the tab
        // between this client asking and us getting here.
        if self.registry.get(session_id).is_some() {
            return Ok(());
        }
        self.gate
            .open(session_id, || async {
                // Re-checked inside the gate: the winner of a race may have
                // finished between the check above and the claim.
                if self.registry.get(session_id).is_some() {
                    return Ok(());
                }
                let (reply, answer) = oneshot::channel();
                let request =
                    OpenRequest { session_id: session_id.to_string(), reply };
                // `try_send` rather than `send`: a full queue or a departed UI
                // should fail now rather than park this RPC forever holding the
                // connection's request slot.
                self.tx
                    .try_send(request)
                    .map_err(|_| "the desktop is not accepting requests".to_string())?;
                // A dropped sender means the UI gave up without replying; nothing
                // else will ever answer, so treat it as a failure rather than wait.
                answer.await.map_err(|_| "the desktop did not answer".to_string())?
            })
            .await
    }
}

/// Serve `open` requests on the UI thread for the lifetime of the app.
///
/// Building panes needs a window, so this runs where one can be resolved. The
/// project is built but **not activated**: a client reaching for a session must
/// not move the desktop out from under whoever is sitting at it.
pub fn serve_opens(
    rx: tokio::sync::mpsc::Receiver<OpenRequest>,
    catalog: Arc<DesktopSessionCatalog>,
    cx: &mut gpui::App,
) {
    let mut rx = rx;
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        while let Some(OpenRequest { session_id, reply }) = rx.recv().await {
            // `AsyncApp::update` returns the closure's value directly in this
            // gpui, so the build's own reason reaches the client unflattened.
            let opened = cx.update(|cx| open_session_project(&catalog, &session_id, cx));
            // A send failure means the caller gave up (its RPC timed out, or the
            // connection dropped). The session is built either way, which is the
            // outcome that matters for the next request.
            let _ = reply.send(opened);
        }
    })
    .detach();
}

/// Build the panes of whichever project owns `session_id`.
fn open_session_project(
    catalog: &DesktopSessionCatalog,
    session_id: &str,
    cx: &mut gpui::App,
) -> Result<(), String> {
    let (project_id, _) = catalog
        .persisted()
        .into_iter()
        .find(|(_, session)| session.session_id == session_id)
        .ok_or_else(|| "no such session on this desktop".to_string())?;
    let project = catalog
        .projects
        .list_ordered(usize::from(u8::MAX))
        .map_err(|_| "the project list could not be read".to_string())?
        .into_iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| "the session's project is no longer known".to_string())?;

    // The first registered window, like the launch bridge: a desktop with several
    // open gives no signal about which one a remote client meant, and asking
    // would defeat the point of reaching a session from away.
    let windows = crate::platform::window_registry::all_windows(cx);
    let (_, workspace) = windows.into_iter().next().ok_or("no window is open".to_string())?;
    let window = cx.windows().into_iter().next().ok_or("no window is open".to_string())?;
    let root = PathBuf::from(&project.root_path);

    window
        .update(cx, |_, window, cx| {
            let panes = workspace.update(cx, |workspace, cx| {
                workspace.build_project_panes_if_absent(&project.id, &root, window, cx)
            });
            // Registration happens while the chat view is constructed, inside the
            // build above, so by the time it returns the session is either in the
            // registry or it never will be.
            let cx: &gpui::App = cx;
            registration_outcome(&catalog.registry, session_id, || {
                panes
                    .read(cx)
                    .agent_chat_view_by_remote_id(session_id, cx)
                    .and_then(|view| view.read(cx).last_error())
            })
        })
        .map_err(|_| "the window could not build the session".to_string())?
}

/// Whether the build actually produced a live session, and why not if it didn't.
///
/// A chat view registers its session as it is constructed, but only if its agent
/// connected: a missing binary or a refused resume leaves the view showing an
/// error and nothing in the registry. Reporting success regardless would hand the
/// client a session it cannot use, and the failure would surface one request later
/// as "unknown session" — true, but not the reason. `reason` is a closure because
/// reading it means walking the project's panes, which is wasted work on the path
/// that succeeds.
fn registration_outcome(
    registry: &SessionRegistry,
    session_id: &str,
    reason: impl FnOnce() -> Option<String>,
) -> Result<(), String> {
    if registry.get(session_id).is_some() {
        return Ok(());
    }
    Err(reason().unwrap_or_else(|| "the session's agent could not be started".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::{AgentConnection, PermissionDecision};

    struct StubConn;
    impl AgentConnection for StubConn {
        fn send_user_message(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn resolve_permission(&self, _id: &str, _d: PermissionDecision) -> anyhow::Result<()> {
            Ok(())
        }
        fn shutdown(&self) {}
    }

    #[test]
    fn a_registered_session_is_reported_open() {
        let registry = SessionRegistry::new();
        registry.register("s1".into(), Arc::new(StubConn));
        let outcome = registration_outcome(&registry, "s1", || {
            panic!("a live session must not pay for the failure path")
        });
        assert_eq!(outcome, Ok(()));
    }

    /// The whole point of the check: a build that spawned nothing must not report
    /// success, or the client is told to use a session that does not exist.
    #[test]
    fn a_build_that_registered_nothing_reports_the_views_reason() {
        let registry = SessionRegistry::new();
        let outcome = registration_outcome(&registry, "s1", || {
            Some("Failed to resume agent: no such file".into())
        });
        assert_eq!(outcome, Err("Failed to resume agent: no such file".into()));
    }

    /// No view means no reason to relay — the client still learns it failed.
    #[test]
    fn a_missing_view_still_fails_with_something_sayable() {
        let registry = SessionRegistry::new();
        let outcome = registration_outcome(&registry, "s1", || None);
        assert_eq!(outcome, Err("the session's agent could not be started".into()));
    }
}
