//! Serving the desktop's projects to a paired device as new-session targets.
//!
//! Unlike [`launch_bridge`](crate::remote_control::launch_bridge), this needs no
//! GPUI hop: the project list is durable data the storage layer already owns, not
//! live view state. So the provider holds the SQLite [`ProjectRepo`] directly and
//! reads it off the async reactor. It is the read-side complement to the launcher
//! — this says *where* the phone may start a session, the launcher then starts it.
//!
//! Opening a folder (`oximux open`) is the one project verb that does need the
//! UI, and goes through [`ProjectOpener`]'s bridge.

use oximux_remote_host::{OpenProjectError, OpenedProject, ProjectProvider};
use oximux_remote_proto::ProjectSummaryWire;
use oximux_storage::ProjectRepo;

use super::open_bridge::ProjectOpener;

/// How many projects to offer the phone — the desktop's own recent-projects cap,
/// so the remote list matches the sidebar rather than dumping the full history.
const PROJECT_LIMIT: usize = 20;

/// Every project, in practice, when matching a folder to one already known.
const KNOWN_PROJECTS_LIMIT: usize = 10_000;

/// Serves the desktop's recent projects (name + absolute path) over remote control.
pub struct RepoProjects {
    repo: ProjectRepo,
    opener: Option<ProjectOpener>,
}

impl RepoProjects {
    pub fn new(repo: ProjectRepo) -> Self {
        Self { repo, opener: None }
    }

    /// Also open folders, through the UI bridge.
    pub fn with_opener(mut self, opener: ProjectOpener) -> Self {
        self.opener = Some(opener);
        self
    }
}

#[async_trait::async_trait]
impl ProjectProvider for RepoProjects {
    async fn projects(&self) -> Vec<ProjectSummaryWire> {
        let repo = self.repo.clone();
        // The read locks a SQLite mutex, so keep it off the async reactor. A read
        // failure degrades to an empty list rather than failing the RPC — the
        // phone then simply offers no quick-start projects, which is honest.
        tokio::task::spawn_blocking(move || {
            repo.list_ordered(PROJECT_LIMIT)
                .unwrap_or_default()
                .into_iter()
                .map(|p| ProjectSummaryWire { name: p.name, path: p.root_path })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    async fn open(&self, path: &str) -> Result<OpenedProject, OpenProjectError> {
        let Some(opener) = &self.opener else {
            return Err(OpenProjectError::Unsupported);
        };
        let (repo, path) = (self.repo.clone(), path.to_owned());
        // The filesystem walk and the project read both block: off the reactor.
        let (root, worktree) = tokio::task::spawn_blocking(move || {
            let known: Vec<String> =
                repo.list_ordered(KNOWN_PROJECTS_LIMIT).unwrap_or_default().into_iter().map(|p| p.root_path).collect();
            super::open_bridge::resolve(&path, &known)
        })
        .await
        .map_err(|_| OpenProjectError::Unavailable)??;
        opener.open(root, worktree).await
    }
}
