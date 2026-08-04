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
    AgentSessionRepo, Db, DiffReviewNoteRepo, PaneBufferRepo, PaneRelayIdRepo, PaneSessionRepo,
    ProjectRepo, RemoteDeviceRepo, SettingsRepo, StorageError, WorkspaceRepo, WorktreeSettingsRepo,
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
    /// Per-worktree SCM scratch state (V006 `worktree_settings` table).
    /// GitPanel reads/writes `view_mode_override` keyed by the worktree's
    /// absolute path so the chosen view mode survives an app restart;
    /// future fields (`base_ref`, `commit_draft`) ride through the same
    /// repo handle.
    pub(crate) worktree_settings_repo: WorktreeSettingsRepo,
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

impl AppState {
    /// Accessor for the binary crate's relay supervisor boot path, which
    /// hands the repo to the crash-heartbeat death closure for stale-id
    /// cleanup. Internal callers use the field directly.
    pub fn pane_relay_id_repo(&self) -> &PaneRelayIdRepo {
        &self.pane_relay_id_repo
    }

    /// Accessor for the binary crate's boot path, which reads the
    /// open-windows manifest to decide how many windows to reopen.
    /// Internal callers use the field directly.
    pub fn settings_repo(&self) -> &SettingsRepo {
        &self.settings_repo
    }

    /// The paired-remote-device repo, built on demand for the boot path that
    /// backs remote control's auth store with durable persistence. Not a stored
    /// field: it is read once at startup, and a repo is just a `Db` handle.
    pub fn remote_device_repo(&self) -> RemoteDeviceRepo {
        RemoteDeviceRepo::new(self.db.clone())
    }

    /// The scheduled-run store, built on demand for the same reason as
    /// [`Self::remote_device_repo`]: it is taken once at startup and is just a
    /// handle on the shared connection.
    ///
    /// Shares the app's `Db` rather than opening its own connection — SQLite
    /// serializes writers, so a second connection would turn lock contention
    /// between the scheduler and the rest of the app into `SQLITE_BUSY`.
    pub fn schedule_store(&self) -> oximux_agents::schedule::ScheduleStore {
        oximux_agents::schedule::ScheduleStore::new(self.db.conn())
    }

    /// The team-run store, shared with remote control so a run opened from the
    /// CLI is the same run the desktop's host reports on.
    pub fn team_store(&self) -> oximux_agents::team::TeamStore {
        oximux_agents::team::TeamStore::new(self.db.conn())
    }

    /// The coordination blackboard, shared for the same reason — the board only
    /// coordinates if every host reads one set of keys.
    pub fn coord_store(&self) -> oximux_agents::coord::CoordStore {
        oximux_agents::coord::CoordStore::new(self.db.conn())
    }

    /// The project repository, shared for the remote-control project provider so a
    /// paired phone lists the same projects the desktop does. Clones the handle
    /// (shared `Db`), not a second connection — SQLite serializes writers.
    pub fn project_repo(&self) -> ProjectRepo {
        self.project_repo.clone()
    }

    /// The workspace repository, shared for the remote-control worktree service
    /// so a CLI-created worktree is the same row the desktop's sidebar lists.
    /// Clones the handle (shared `Db`), not a second connection.
    pub fn workspace_repo(&self) -> WorkspaceRepo {
        self.workspace_repo.clone()
    }
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
    let worktree_settings_repo = WorktreeSettingsRepo::new(db.clone());

    // Install the process-wide review-note repo handle so diff views (built
    // deep in the pane/sidebar tree) can persist + rehydrate notes without
    // threading a Db through every intermediate constructor.
    crate::shell::diff_view::note_repo_handle::init_note_repo(DiffReviewNoteRepo::new(db.clone()));

    // Install the process-wide ambient-agent state handle so a plain terminal
    // running a hand-typed agent can persist its last hook reading and re-seed
    // it on a warm re-attach — keeping the agent listed across a restart.
    crate::shell::ambient_state::init(settings_repo.clone());

    // Manual display order for the rail: by `sort_order`, not recency. Opening
    // a project does not reshuffle the list — the order is sticky until a drag
    // rewrites it. The picker reads the same snapshot.
    let recent_projects = project_repo.list_ordered(RECENT_PROJECTS_LIMIT)?;

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
    // crash mid-iteration: next boot's `list_unfinished_at_shutdown`
    // returns only the still-non-terminal rows.
    let running = agent_session_repo.list_unfinished_at_shutdown()?;
    // Mark each alive-at-shutdown session Interrupted AND stamp its `ended_at`
    // with the shutdown time, mirroring both into the in-memory copy. The
    // `ended_at` stamp is what lets the rail keep it as recent "sleeping"
    // history after the restart (so the agent and its persisted title survive
    // the quit); a status-only mark would leave `ended_at` NULL and the row
    // would be culled as stale.
    let interrupted_sessions: Vec<AgentSession> = running
        .into_iter()
        .map(|mut s| {
            let ts = agent_session_repo.mark_interrupted_at_shutdown(&s.id)?;
            s.status = AgentStatus::Interrupted;
            s.ended_at = Some(ts);
            Ok(s)
        })
        .collect::<Result<Vec<_>, oximux_storage::StorageError>>()?;

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
        worktree_settings_repo,
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
        // `list_unfinished_at_shutdown` returns empty.
        assert!(
            state
                .agent_session_repo
                .list_unfinished_at_shutdown()
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
    fn hydrate_recent_projects_manual_sticky_ignores_last_opened() {
        let db = fresh_db();
        let project_repo = ProjectRepo::new(db.clone());
        let p1 = project_repo.insert("p1", "/p1", "main").expect("p1");
        let p2 = project_repo.insert("p2", "/p2", "main").expect("p2");
        let p3 = project_repo.insert("p3", "/p3", "main").expect("p3");
        // Opening p2 must NOT float it to the top: the rail order is manual-
        // sticky (by sort_order = insertion order here), not recency.
        project_repo
            .update_last_opened_at(&p2.id)
            .expect("touch p2");

        let state = hydrate(db).expect("hydrate");

        assert_eq!(state.recent_projects.len(), 3);
        let ids: Vec<&str> = state
            .recent_projects
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        // Insertion order assigns sort_order 1.0, 2.0, 3.0; touching p2 leaves
        // it untouched.
        assert_eq!(ids, [p1.id.as_str(), p2.id.as_str(), p3.id.as_str()]);
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
