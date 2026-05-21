//! `AppState` — boot-time hydration of recent projects, workspaces, and
//! Interrupted sessions out of `oximux-storage`.
//!
//! Phase 4 step 4 plumbing only: the data is loaded into memory but no UI
//! consumes it yet (steps 5–7 will render it). Held as a field on
//! `WorkspaceRoot` rather than `cx.set_global` — narrow surface, widen if
//! a non-`WorkspaceRoot` consumer ever appears.
//!
//! Threading: `hydrate` is a sync function that holds the `Db` mutex
//! through several round-trips. `main.rs` calls it inside
//! `tokio::task::spawn_blocking` so the GPUI runtime is never blocked.

use std::collections::HashMap;

use oximux_core::{AgentSession, AgentStatus, Project, Workspace};
use oximux_storage::{
    AgentSessionRepo, Db, PaneBufferRepo, PaneRelayIdRepo, PaneSessionRepo, ProjectRepo,
    SettingsRepo, StorageError, WorkspaceRepo,
};

/// Recent-projects fetch limit. The project picker (step 5) paginates if a
/// user has more than 20 projects; 20 is the conventional terminal-mux
/// "recent files" count.
const RECENT_PROJECTS_LIMIT: usize = 20;

/// In-memory snapshot of persisted state at startup + handles to every
/// repository.
///
/// Clone cost: the repo fields wrap `Db` (`Arc<Mutex<…>>`) so they are
/// O(1) `Arc::clone`. The snapshot fields (`recent_projects`, `workspaces`,
/// `interrupted_sessions`) clone their heap contents — clone once at
/// startup, then share by reference; never call `clone()` in a render loop.
///
/// `#[allow(dead_code)]` because steps 5–10 consume these fields one by
/// one; the struct ships fully populated so each downstream step is a
/// pure consumer change without revisiting hydration. Remove once step
/// 10 (settings round-trip) lands and every field has at least one
/// non-test reader.
#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    pub(crate) db: Db,
    pub(crate) project_repo: ProjectRepo,
    pub(crate) workspace_repo: WorkspaceRepo,
    pub(crate) agent_session_repo: AgentSessionRepo,
    pub(crate) pane_session_repo: PaneSessionRepo,
    pub(crate) pane_buffer_repo: PaneBufferRepo,
    pub(crate) pane_relay_id_repo: PaneRelayIdRepo,
    pub(crate) settings_repo: SettingsRepo,
    /// Most recently opened projects (newest `last_opened_at` first), capped
    /// at [`RECENT_PROJECTS_LIMIT`]. Empty on first launch — that is the
    /// step-5 empty-state trigger.
    pub(crate) recent_projects: Vec<Project>,
    /// Workspaces keyed by `project.id`. Only projects in
    /// [`Self::recent_projects`] are fetched at boot; step 5+ fetch on
    /// demand for non-recent projects.
    pub(crate) workspaces: HashMap<String, Vec<Workspace>>,
    /// Sessions that were `Running` at the last shutdown — already marked
    /// `Interrupted` in both the DB and in these in-memory copies. Step 9
    /// will surface them via the sidebar badge.
    pub(crate) interrupted_sessions: Vec<AgentSession>,
}

/// Load recent projects + their workspaces, then mark every alive-at-
/// shutdown agent session as [`AgentStatus::Interrupted`] and return them
/// in `interrupted_sessions`. The Interrupted side effect is colocated
/// with the read so shipping a hydration that "forgets" the mark is
/// impossible.
///
/// Idempotent: a second call on the same DB finds zero `Running` rows and
/// `interrupted_sessions` will be empty.
///
/// Returns [`StorageError`] on the first repo failure — caller (`main.rs`)
/// treats this as a hard-crash boot error.
pub fn hydrate(db: Db) -> Result<AppState, StorageError> {
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db.clone());
    let agent_session_repo = AgentSessionRepo::new(db.clone());
    let pane_session_repo = PaneSessionRepo::new(db.clone());
    let pane_buffer_repo = PaneBufferRepo::new(db.clone());
    let pane_relay_id_repo = PaneRelayIdRepo::new(db.clone());
    let settings_repo = SettingsRepo::new(db.clone());

    let recent_projects = project_repo.list_recent(RECENT_PROJECTS_LIMIT)?;

    let mut workspaces: HashMap<String, Vec<Workspace>> =
        HashMap::with_capacity(recent_projects.len());
    for project in &recent_projects {
        let list = workspace_repo.list_for_project(&project.id)?;
        workspaces.insert(project.id.clone(), list);
    }

    // Mark every alive-at-shutdown session Interrupted. The loop takes N
    // serial `with_conn` lock acquisitions — fine for v1 dogfood scale
    // (O(10) sessions); step 9 should batch via a transactional repo
    // method when realistic loads grow. Idempotent if interrupted by a
    // crash mid-iteration: next boot's `list_running_at_shutdown` returns
    // only the still-`Running` rows.
    let running = agent_session_repo.list_running_at_shutdown()?;
    for session in &running {
        agent_session_repo.update_status(&session.id, &AgentStatus::Interrupted)?;
    }
    // Update the in-memory copies to match the DB rows we just wrote.
    // Without this, consumers reading `app_state.interrupted_sessions[i]
    // .status` would see the stale `Running` snapshot.
    let interrupted_sessions: Vec<AgentSession> = running
        .into_iter()
        .map(|mut s| {
            s.status = AgentStatus::Interrupted;
            s
        })
        .collect();

    let workspace_total: usize = workspaces.values().map(Vec::len).sum();
    tracing::info!(
        n_projects = recent_projects.len(),
        n_workspaces = workspace_total,
        n_interrupted_marked = interrupted_sessions.len(),
        "AppState hydrated"
    );

    Ok(AppState {
        db,
        project_repo,
        workspace_repo,
        agent_session_repo,
        pane_session_repo,
        pane_buffer_repo,
        pane_relay_id_repo,
        settings_repo,
        recent_projects,
        workspaces,
        interrupted_sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Db {
        oximux_storage::open_memory().expect("open_memory")
    }

    #[test]
    fn hydrate_empty_db_returns_empty_state() {
        let db = fresh_db();
        let state = hydrate(db).expect("hydrate");
        assert!(state.recent_projects.is_empty());
        assert!(state.workspaces.is_empty());
        assert!(state.interrupted_sessions.is_empty());
    }

    #[test]
    fn hydrate_marks_running_sessions_interrupted() {
        let db = fresh_db();
        let project_repo = ProjectRepo::new(db.clone());
        let workspace_repo = WorkspaceRepo::new(db.clone());
        let agent_repo = AgentSessionRepo::new(db.clone());

        let project = project_repo.insert("p", "/p", "main").expect("project");
        let workspace = workspace_repo
            .insert(&project.id, "ws", "ws", "oximux/ws", "/p/ws")
            .expect("workspace");
        let alive_1 = agent_repo
            .insert(&workspace.id, "claude", None, None)
            .expect("session 1");
        let alive_2 = agent_repo
            .insert(&workspace.id, "codex", None, None)
            .expect("session 2");
        let done = agent_repo
            .insert(&workspace.id, "aider", None, None)
            .expect("session 3");
        // Freshly inserted rows are `Idle`; bump the two we want to test
        // up to `Running`, leave the third as Done so it doesn't match.
        agent_repo
            .update_status(&alive_1.id, &AgentStatus::Running)
            .expect("running 1");
        agent_repo
            .update_status(&alive_2.id, &AgentStatus::Running)
            .expect("running 2");
        agent_repo
            .update_status(&done.id, &AgentStatus::Done { code: Some(0) })
            .expect("done");

        let state = hydrate(db).expect("hydrate");

        assert_eq!(state.interrupted_sessions.len(), 2);
        // In-memory copies must match the DB rows we just wrote. Guards
        // against H2 (stale `Running` status in the returned vec).
        assert!(
            state
                .interrupted_sessions
                .iter()
                .all(|s| s.status == AgentStatus::Interrupted)
        );
        // The DB rows must actually be `interrupted` now — second
        // `list_running_at_shutdown` returns empty.
        assert!(
            state
                .agent_session_repo
                .list_running_at_shutdown()
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn hydrate_project_with_no_workspaces() {
        let db = fresh_db();
        let project = ProjectRepo::new(db.clone())
            .insert("p", "/p", "main")
            .expect("project");
        let state = hydrate(db).expect("hydrate");
        assert_eq!(state.recent_projects.len(), 1);
        // Key must be present with an empty vec, not absent.
        assert_eq!(state.workspaces.get(&project.id), Some(&vec![]));
    }

    #[test]
    fn hydrate_recent_projects_ordered_by_last_opened() {
        let db = fresh_db();
        let project_repo = ProjectRepo::new(db.clone());
        let _p1 = project_repo.insert("p1", "/p1", "main").expect("p1");
        let p2 = project_repo.insert("p2", "/p2", "main").expect("p2");
        let _p3 = project_repo.insert("p3", "/p3", "main").expect("p3");
        // Bump p2 forward — it should now lead the recent list.
        project_repo
            .update_last_opened_at(&p2.id)
            .expect("touch p2");

        let state = hydrate(db).expect("hydrate");

        assert_eq!(state.recent_projects.len(), 3);
        assert_eq!(state.recent_projects[0].id, p2.id);
        // p1 still has no last_opened_at; whether it lands at index 1 or 2
        // depends only on the `ORDER BY last_opened_at IS NULL` tiebreaker,
        // which leaves the two NULL rows in `created_at DESC` order.
    }

    #[test]
    fn hydrate_workspaces_keyed_by_project() {
        let db = fresh_db();
        let project_repo = ProjectRepo::new(db.clone());
        let workspace_repo = WorkspaceRepo::new(db.clone());
        let a = project_repo.insert("a", "/a", "main").expect("a");
        let b = project_repo.insert("b", "/b", "main").expect("b");
        workspace_repo
            .insert(&a.id, "wa", "wa", "oximux/wa", "/a/wa")
            .expect("wa");
        workspace_repo
            .insert(&b.id, "wb1", "wb1", "oximux/wb1", "/b/wb1")
            .expect("wb1");
        workspace_repo
            .insert(&b.id, "wb2", "wb2", "oximux/wb2", "/b/wb2")
            .expect("wb2");

        let state = hydrate(db).expect("hydrate");

        assert_eq!(state.workspaces.get(&a.id).map(Vec::len), Some(1));
        assert_eq!(state.workspaces.get(&b.id).map(Vec::len), Some(2));
    }

    #[test]
    fn hydrate_is_idempotent() {
        let db = fresh_db();
        let project_repo = ProjectRepo::new(db.clone());
        let workspace_repo = WorkspaceRepo::new(db.clone());
        let agent_repo = AgentSessionRepo::new(db.clone());
        let project = project_repo.insert("p", "/p", "main").expect("project");
        let workspace = workspace_repo
            .insert(&project.id, "ws", "ws", "oximux/ws", "/p/ws")
            .expect("workspace");
        let session = agent_repo
            .insert(&workspace.id, "claude", None, None)
            .expect("session");
        agent_repo
            .update_status(&session.id, &AgentStatus::Running)
            .expect("running");

        let first = hydrate(db.clone()).expect("first hydrate");
        assert_eq!(first.interrupted_sessions.len(), 1);

        let second = hydrate(db).expect("second hydrate");
        assert_eq!(second.interrupted_sessions.len(), 0);
    }
}
