//! The project-listing seam: enumerating the desktop's projects so a client can
//! start a session in one without typing its path.
//!
//! Like [`SessionLauncher`](crate::SessionLauncher), the data lives in the GPUI
//! app layer (the recent-projects list the sidebar renders), not in the
//! spawn-free registry — so the dispatcher talks to this trait and the app
//! supplies the implementation. The paired complement to `create`: this tells the
//! phone *where* it may start a session; the launcher then starts it there.

use oximux_remote_proto::ProjectSummaryWire;

/// Listing the projects the desktop knows.
///
/// Gated exactly like [`SessionLauncher`](crate::SessionLauncher): the list is
/// only useful for creating a session and it exposes absolute host paths, so a
/// device that may not create sessions may not enumerate them. Returning an empty
/// list is legitimate — a desktop with no recent projects, or one that chooses to
/// expose none.
#[async_trait::async_trait]
pub trait ProjectProvider: Send + Sync {
    /// The projects offered as new-session targets, newest-first (the order the
    /// desktop itself presents them). Each carries a display name and the absolute
    /// host path a client hands back to
    /// [`Request::CreateSession`](oximux_remote_proto::proto::Request::CreateSession).
    async fn projects(&self) -> Vec<ProjectSummaryWire>;

    /// Open `path` (absolute; validated by the implementation, which owns the
    /// filesystem question) as a project in the desktop, and bring it forward.
    ///
    /// The default is [`OpenProjectError::Unsupported`]: a host with no
    /// windows — headless `oximux serve` — has nothing to open it in.
    async fn open(&self, _path: &str) -> Result<OpenedProject, OpenProjectError> {
        Err(OpenProjectError::Unsupported)
    }
}

/// What [`ProjectProvider::open`] opened.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedProject {
    pub project: ProjectSummaryWire,
    /// The linked worktree selected in it, when the folder was one the host
    /// lists.
    pub worktree: Option<String>,
}

/// Why a folder could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenProjectError {
    /// This host has no windows to open a project in.
    Unsupported,
    /// The path does not name an existing directory.
    NotADirectory,
    /// The desktop could not do it right now (no window to open it in, or its
    /// UI did not answer).
    Unavailable,
    /// Registering the project failed.
    Failed(String),
}
