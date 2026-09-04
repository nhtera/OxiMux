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
use oximux_remote_host::catalog::{
    DormantChoice, DormantChoices, DormantSession, DormantTranscript, OpenGate, SessionCatalog,
};

use crate::session_restore::persisted_terminals::{PersistedTabKind, PersistedTabs};

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

    /// Every project's persisted layout, in order.
    ///
    /// The layout is the index of what exists on disk — which sessions there are,
    /// and the transcript each was last saved with — so both listing and reading
    /// are answered from here without building anything.
    fn layouts(&self) -> Vec<(String, PersistedTabs)> {
        let Ok(projects) = self.projects.list_ordered(usize::from(u8::MAX)) else {
            return Vec::new();
        };
        projects
            .into_iter()
            .filter_map(|project| {
                match crate::project_panes_factory::load_persisted_tabs(
                    &self.settings,
                    &project.id,
                    &self.window_id,
                ) {
                    crate::project_panes_factory::LoadedTabs::Snapshot(s) => Some((project.id, s)),
                    _ => None,
                }
            })
            .collect()
    }

    /// Every persisted chat session, paired with the project that owns it.
    ///
    /// A chat with no agent session id has never completed a turn: there is
    /// nothing to resume and no stable id to name it by, so it is not offered.
    fn persisted(&self) -> Vec<(String, DormantSession)> {
        let mut found = Vec::new();
        for (project_id, snapshot) in self.layouts() {
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
                    project_id.clone(),
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

    /// The persisted tab that proves `session_id` exists, and the transcript blob
    /// saved beside it.
    ///
    /// Two separate things: the tab is the session's existence, while the blob is
    /// its contents and may legitimately be missing — a chat that never settled a
    /// turn has a tab pointing at a session id and nothing saved under it.
    fn saved(
        &self,
        session_id: &str,
    ) -> Option<(PersistedTabKind, Option<crate::persisted_chat::PersistedChatTranscript>)> {
        self.layouts().into_iter().find_map(|(_, snapshot)| {
            let tab = snapshot.tabs.iter().find(|tab| match &tab.kind {
                PersistedTabKind::AgentChat { session_id: Some(id), .. } => id == session_id,
                _ => false,
            })?;
            let saved =
                snapshot.chat_transcripts.iter().find(|t| t.session_id == session_id).cloned();
            Some((tab.kind.clone(), saved))
        })
    }
}

/// Whether `session_id` exists as far as this desktop's storage can tell —
/// the predicate the schedule orphan sweep runs against.
///
/// Two sources, either sufficient, matching how each host defines existence:
/// a **persisted tab** naming the session (this catalog's own notion — the
/// transcript blob is contents and may legitimately be missing for a chat
/// that never settled a turn), or an `agent_chat:` **blob** under the id (a
/// session a headless host created in the shared data dir leaves no tab
/// here, and disabling a schedule aimed at it would be the sweep firing on
/// its own blind spot). Erring toward existence is the right bias for a
/// safety net: a false "exists" merely defers to the fire path's own error,
/// while a false "gone" silently disables a working schedule.
pub fn session_known_in_storage(
    settings: &oximux_storage::SettingsRepo,
    projects: &oximux_storage::ProjectRepo,
    window_id: &str,
    session_id: &str,
) -> bool {
    if settings
        .get(&crate::persisted_chat::chat_settings_key(session_id))
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    let Ok(rows) = projects.list_ordered(usize::from(u8::MAX)) else {
        // Unreadable storage must not read as "every session is gone".
        return true;
    };
    rows.into_iter().any(|project| {
        match crate::project_panes_factory::load_persisted_tabs(settings, &project.id, window_id) {
            crate::project_panes_factory::LoadedTabs::Snapshot(snapshot) => {
                snapshot.tabs.iter().any(|tab| {
                    matches!(
                        &tab.kind,
                        PersistedTabKind::AgentChat { session_id: Some(id), .. } if id == session_id
                    )
                })
            }
            _ => false,
        }
    })
}

fn model_choice(choice: oximux_agents::thread::ModelChoice) -> DormantChoice {
    DormantChoice { id: choice.wire, label: choice.label, description: choice.description }
}

/// A mode has no description — the wire carries one for symmetry with models, and
/// the picker renders a single-line row without it.
fn mode_choice(choice: oximux_agents::thread::ModeChoice) -> DormantChoice {
    DormantChoice { id: choice.wire, label: choice.label, description: None }
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

    fn transcript(&self, session_id: &str) -> Option<DormantTranscript> {
        let (tab, saved) = self.saved(session_id)?;
        let entries_json = saved
            .as_ref()
            .and_then(|t| serde_json::to_string(&t.entries).ok())
            .unwrap_or_else(|| "[]".to_string());
        // The transcript's own model over the tab's: it is written with the
        // history, so it describes what actually produced these entries.
        let model = saved.and_then(|t| t.model).or(match tab {
            PersistedTabKind::AgentChat { model, .. } => model,
            _ => None,
        });
        Some(DormantTranscript { entries_json, model })
    }

    fn choices(&self, session_id: &str) -> Option<DormantChoices> {
        let (_, saved) = self.saved(session_id)?;
        // A session saved before its pickers were recorded reads as empty, which
        // is the same answer a backend offering no choices gives — the pickers
        // are not shown, rather than shown broken.
        let Some(saved) = saved else {
            return Some(DormantChoices::default());
        };
        Some(DormantChoices {
            models: saved.choices.models.into_iter().map(model_choice).collect(),
            modes: saved.choices.modes.into_iter().map(mode_choice).collect(),
            current_model: saved.choices.current_model,
            current_mode: saved.choices.current_mode,
        })
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
            // A restored chat view is built dormant (no subprocess, nothing
            // registered) and normally connects on its first render — but a
            // remote open targets a project the desktop may not be showing, so
            // no render is coming. Connect it here; the resume respawn is what
            // registers the session in the registry. No-op for a view that is
            // already live.
            let dormant_view = panes.read(cx).agent_chat_view_by_remote_id(session_id, cx);
            if let Some(view) = dormant_view {
                // retry_failed: a phone re-opening a session whose deferred
                // connect failed earlier is an explicit retry — re-attempt the
                // spawn instead of replaying the stale error forever.
                view.update(cx, |v, cx| v.ensure_connected(true, cx));
            }
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
    use oximux_agents::thread::{AgentConnection, ModeChoice, ModelChoice, PermissionDecision};

    use crate::persisted_chat::{PersistedChatTranscript, PersistedChoices};
    use crate::session_restore::persisted_terminals::{PersistedTab, PersistedTabs};

    const WINDOW: &str = "main";

    /// A catalog over one project holding one saved chat, with its own in-memory
    /// storage — the layout read is the whole point of these tests, so nothing
    /// about it is stubbed.
    fn catalog_over(transcript: PersistedChatTranscript) -> DesktopSessionCatalog {
        let db = oximux_storage::open_memory().expect("memory db");
        let settings = Arc::new(oximux_storage::SettingsRepo::new(db.clone()));
        let projects = Arc::new(oximux_storage::ProjectRepo::new(db));
        let project = projects.insert("thing", "/repo/thing", "main").expect("project");

        let snapshot = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "Fix auth".into(),
                kind: PersistedTabKind::AgentChat {
                    cwd: "/repo/thing".into(),
                    model: Some("sonnet".into()),
                    session_id: Some(transcript.session_id.clone()),
                    draft: None,
                    queued: vec![],
                    unbound: false,
                },
                ..PersistedTab::default()
            }],
            chat_transcripts: vec![transcript],
            ..PersistedTabs::default()
        };
        crate::project_panes_factory::save_persisted_tabs(
            &settings,
            &project.id,
            WINDOW,
            &snapshot,
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        DesktopSessionCatalog::new(
            Arc::new(SessionRegistry::new()),
            settings,
            projects,
            WINDOW.to_string(),
            tx,
        )
    }

    fn saved_chat(choices: PersistedChoices) -> PersistedChatTranscript {
        PersistedChatTranscript {
            session_id: "cold-1".into(),
            model: Some("sonnet".into()),
            entries: vec![oximux_agents::thread::ThreadEntry::User {
                text: "what did you change?".into(),
                images: vec![],
                checkpoint: None,
            }],
            slash_commands: vec![],
            session_meta: Default::default(),
            thinking_level: Default::default(),
            provider: oximux_agents::thread::Transport::StreamJson,
            acp_command: None,
            acp_args: vec![],
            adapter_id: None,
            launch_profile: None,
            codex_posture: None,
            pi_posture: None,
            omp_posture: None,
            claude_fast_mode: None,
            choices,
        }
    }

    /// The history a client opens a conversation to read, served without building
    /// anything.
    #[test]
    fn a_saved_session_serves_its_history_from_disk() {
        let catalog = catalog_over(saved_chat(PersistedChoices::default()));

        let transcript = catalog.transcript("cold-1").expect("the saved session is readable");
        let entries: serde_json::Value =
            serde_json::from_str(&transcript.entries_json).expect("entries are valid json");
        assert_eq!(entries[0]["User"]["text"], "what did you change?");
        assert_eq!(transcript.model.as_deref(), Some("sonnet"));
        assert!(catalog.transcript("no-such-session").is_none(), "and nothing else is");
    }

    /// The pickers a client asks for the moment it opens a conversation, from the
    /// same saved copy — so asking for them does not spawn the agent that reading
    /// the history just avoided.
    #[test]
    fn a_saved_session_serves_its_pickers_from_disk() {
        let catalog = catalog_over(saved_chat(PersistedChoices {
            models: vec![ModelChoice {
                wire: "claude-opus-5".into(),
                label: "Opus 5".into(),
                description: Some("Most capable".into()),
            }],
            modes: vec![ModeChoice { wire: "plan".into(), label: "Plan".into() }],
            current_model: Some("claude-opus-5".into()),
            current_mode: Some("plan".into()),
        }));

        let choices = catalog.choices("cold-1").expect("the saved session has pickers");
        assert_eq!(choices.models.len(), 1);
        assert_eq!(choices.models[0].id, "claude-opus-5");
        assert_eq!(choices.models[0].description.as_deref(), Some("Most capable"));
        assert_eq!(choices.modes[0].label, "Plan", "a mode carries no description");
        assert_eq!(choices.modes[0].description, None);
        assert_eq!(choices.current_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(choices.current_mode.as_deref(), Some("plan"));
    }

    /// A session saved before pickers were recorded still opens — it simply offers
    /// nothing, which is what a backend with no choices looks like too. Answering
    /// `None` here would instead report the session as unknown.
    #[test]
    fn a_session_saved_without_pickers_still_opens() {
        let catalog = catalog_over(saved_chat(PersistedChoices::default()));

        let choices = catalog.choices("cold-1").expect("the session is still known");
        assert!(choices.models.is_empty(), "no picker rather than a broken one");
        assert!(choices.modes.is_empty());
    }

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

    /// The orphan-sweep predicate must honour BOTH notions of existence: a
    /// tab with no blob (a desktop chat that never settled a turn), and a
    /// blob with no tab (a session a headless host created in the shared
    /// data dir). Only a session with neither is gone.
    #[test]
    fn session_known_covers_tab_only_and_blob_only_sessions() {
        let db = oximux_storage::open_memory().expect("memory db");
        let settings = oximux_storage::SettingsRepo::new(db.clone());
        let projects = oximux_storage::ProjectRepo::new(db);
        let project = projects.insert("thing", "/repo/thing", "main").expect("project");

        // Tab-only: names a session id, saves no transcript beside it.
        let snapshot = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "Fresh chat".into(),
                kind: PersistedTabKind::AgentChat {
                    cwd: "/repo/thing".into(),
                    model: None,
                    session_id: Some("tab-only".into()),
                    draft: None,
                    queued: vec![],
                    unbound: false,
                },
                ..PersistedTab::default()
            }],
            ..PersistedTabs::default()
        };
        crate::project_panes_factory::save_persisted_tabs(&settings, &project.id, WINDOW, &snapshot);

        // Blob-only: the settings key a headless host writes, no tab anywhere.
        settings
            .set(&crate::persisted_chat::chat_settings_key("blob-only"), "{}")
            .expect("seed blob");

        assert!(
            session_known_in_storage(&settings, &projects, WINDOW, "tab-only"),
            "a never-settled chat's tab is its existence"
        );
        assert!(
            session_known_in_storage(&settings, &projects, WINDOW, "blob-only"),
            "a serve-created session in the shared data dir must not read as gone"
        );
        assert!(
            !session_known_in_storage(&settings, &projects, WINDOW, "neither"),
            "a session with no tab and no blob is genuinely gone"
        );
    }
}
