//! `oximux open` / `oximux .`: a folder crossing from the dispatcher onto the
//! GPUI thread to be opened as a project, and the answer coming back.
//!
//! The same shape as [`launch_bridge`](super::launch_bridge) — an mpsc queue the
//! UI drains, a oneshot riding along for the reply — because opening a project
//! switches what a window shows, which only the view layer can do. Everything
//! that does not need the UI (is the path a directory, which project does it
//! belong to) is settled on the dispatcher's side before the hop.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oximux_remote_host::{OpenProjectError, OpenedProject};
use oximux_remote_proto::ProjectSummaryWire;
use tokio::sync::{mpsc, oneshot};

/// One folder to open, already resolved to its project.
pub struct OpenRequest {
    /// The project root: a git work tree's top level (a linked worktree's main
    /// repository), or the folder itself when it is in no repository.
    pub root: PathBuf,
    /// The linked worktree the folder was in, to select once the project opens.
    pub worktree: Option<PathBuf>,
    pub reply: oneshot::Sender<Result<OpenedProject, OpenProjectError>>,
}

/// Human speed, like launches: someone typing `oximux .`.
const QUEUE: usize = 4;

/// How long a request waits for a window. `oximux open` launches the app when
/// it is not running and sends as soon as the control socket answers, which
/// can be before the first window has been restored.
const WINDOW_WAIT: Duration = Duration::from_secs(10);

/// The dispatcher's end of the bridge.
#[derive(Clone)]
pub struct ProjectOpener {
    tx: mpsc::Sender<OpenRequest>,
}

impl ProjectOpener {
    /// Open the project at `root` (from [`resolve`]), selecting `worktree`.
    pub async fn open(&self, root: PathBuf, worktree: Option<PathBuf>) -> Result<OpenedProject, OpenProjectError> {
        let (reply, answer) = oneshot::channel();
        // `try_send`: a full queue or a dropped receiver fails now rather than
        // parking the caller.
        self.tx
            .try_send(OpenRequest { root, worktree, reply })
            .map_err(|_| OpenProjectError::Unavailable)?;
        answer.await.map_err(|_| OpenProjectError::Unavailable)?
    }
}

/// Build both ends. The receiver is drained by [`serve_opens`].
pub fn open_bridge() -> (ProjectOpener, mpsc::Receiver<OpenRequest>) {
    let (tx, rx) = mpsc::channel(QUEUE);
    (ProjectOpener { tx }, rx)
}

/// The project to open for `path`, and the linked worktree to select.
///
/// A project the desktop already knows wins: the deepest of `known_roots` that
/// holds `path` (so a subfolder registered as its own project stays that
/// project), compared by resolved path, and handed back spelled as it was
/// registered — registration matches roots as exact strings, so another
/// spelling of the same directory (a symlink, `/tmp` vs `/private/tmp`) would
/// add a second row. Otherwise the project `path` belongs to ([`project_of`]).
///
/// Reads the filesystem: call it off the async reactor.
pub fn resolve(path: &str, known_roots: &[String]) -> Result<(PathBuf, Option<PathBuf>), OpenProjectError> {
    let folder = Path::new(path).canonicalize().map_err(|_| OpenProjectError::NotADirectory)?;
    if !folder.is_dir() {
        return Err(OpenProjectError::NotADirectory);
    }
    let known: Vec<(PathBuf, &String)> = known_roots
        .iter()
        .filter_map(|known| Some((Path::new(known).canonicalize().ok()?, known)))
        .collect();
    let deepest_holding = |dir: &Path| {
        known
            .iter()
            .filter(|(resolved, _)| dir.starts_with(resolved))
            .max_by_key(|(resolved, _)| resolved.components().count())
            .map(|(_, spelled)| PathBuf::from(spelled.as_str()))
    };
    let (root, worktree) = project_of(&folder);
    // A linked worktree belongs to its main repository, whose directory does
    // not hold it, so only a plain folder or a primary work tree is matched by
    // containment; the worktree's project is matched by its root alone.
    if worktree.is_none()
        && let Some(known) = deepest_holding(&folder)
    {
        return Ok((known, None));
    }
    let root = root.canonicalize().unwrap_or(root);
    let spelled = known.iter().find(|(resolved, _)| *resolved == root).map(|(_, spelled)| PathBuf::from(spelled.as_str()));
    Ok((spelled.unwrap_or(root), worktree))
}

/// The project `folder` belongs to, and the linked worktree it is in.
///
/// The nearest ancestor holding a `.git` is the work tree's top level, as
/// `git rev-parse --show-toplevel` would answer, without a git process. When
/// that `.git` is a file the top level is a linked worktree, and the project is
/// its main repository ([`oximux_git::main_worktree_of`]); a submodule's
/// `.git` file names no shared directory and stays its own root.
pub fn project_of(folder: &Path) -> (PathBuf, Option<PathBuf>) {
    for dir in folder.ancestors() {
        let git = dir.join(".git");
        if git.is_dir() {
            return (dir.to_path_buf(), None);
        }
        if git.is_file() {
            return match oximux_git::main_worktree_of(dir) {
                Some(main) => (main, Some(dir.to_path_buf())),
                None => (dir.to_path_buf(), None),
            };
        }
    }
    (folder.to_path_buf(), None)
}

/// Drain open requests on the GPUI thread: the frontmost workspace window opens
/// the project and comes forward with the app.
pub fn serve_opens(rx: mpsc::Receiver<OpenRequest>, cx: &mut gpui::App) {
    let mut rx = rx;
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        while let Some(OpenRequest { root, worktree, reply }) = rx.recv().await {
            let deadline = std::time::Instant::now() + WINDOW_WAIT;
            let target = loop {
                if let Some(target) = cx.update(frontmost_window) {
                    break Some(target);
                }
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                cx.background_executor().timer(Duration::from_millis(200)).await;
            };
            let Some((handle, workspace)) = target else {
                let _ = reply.send(Err(OpenProjectError::Unavailable));
                continue;
            };
            let _ = cx.update(|cx| {
                cx.activate(true);
                handle.update(cx, |_, window, cx| {
                    window.activate_window();
                    workspace.update(cx, |root_view, cx| {
                        root_view.open_folder(
                            root,
                            worktree,
                            move |opened| {
                                let _ = reply.send(opened.map_err(OpenProjectError::Failed).map(
                                    |(project, worktree)| OpenedProject {
                                        project: ProjectSummaryWire { name: project.name, path: project.root_path },
                                        worktree,
                                    },
                                ));
                            },
                            window,
                            cx,
                        );
                    });
                })
            });
            // A window that closed in between drops `reply` unanswered, which
            // the caller reads as unavailable.
        }
    })
    .detach();
}

/// The frontmost tracked workspace window and its root, if any is open.
fn frontmost_window(
    cx: &mut gpui::App,
) -> Option<(gpui::AnyWindowHandle, gpui::Entity<crate::workspace_root::WorkspaceRoot>)> {
    use crate::platform::window_registry::{all_windows, front_rank, window_handle};
    let mut windows = all_windows(cx);
    windows.sort_by_key(|(id, _)| front_rank(cx, id));
    windows.into_iter().find_map(|(id, workspace)| Some((window_handle(cx, &id)?, workspace)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_opens_the_project_it_belongs_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("mkdir");
        std::fs::create_dir_all(repo.join("src/deep")).expect("mkdir");
        assert_eq!(project_of(&repo.join("src/deep")), (repo.clone(), None));
        assert_eq!(project_of(&repo), (repo, None));

        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).expect("mkdir");
        assert_eq!(project_of(&plain), (plain.clone(), None));
    }

    #[test]
    fn a_linked_worktree_opens_its_main_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("main");
        let shared = main.join(".git");
        let meta = shared.join("worktrees/feature");
        std::fs::create_dir_all(&meta).expect("mkdir");
        std::fs::write(meta.join("commondir"), "../..").expect("write");
        let linked = dir.path().join("feature");
        std::fs::create_dir_all(linked.join("src")).expect("mkdir");
        std::fs::write(linked.join(".git"), format!("gitdir: {}\n", meta.display())).expect("write");

        let (root, worktree) = project_of(&linked.join("src"));
        assert_eq!(root.canonicalize().unwrap(), main.canonicalize().unwrap());
        assert_eq!(worktree, Some(linked));
    }

    #[test]
    fn a_submodule_is_its_own_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        // A submodule's `.git` file points into the parent's `modules/`, which
        // carries no `commondir`.
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").expect("write");
        assert_eq!(project_of(&sub), (sub, None));
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_refused() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let missing = file.path().with_extension("missing");
        for path in [file.path(), missing.as_path()] {
            assert_eq!(resolve(&path.to_string_lossy(), &[]).unwrap_err(), OpenProjectError::NotADirectory);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_known_project_keeps_the_spelling_the_desktop_registered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join(".git")).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        // Registered through the link; opened by its real path.
        let known = vec![link.to_string_lossy().into_owned()];
        assert_eq!(resolve(&real.to_string_lossy(), &known).unwrap().0, link);
        // Unknown: the canonical root.
        assert_eq!(resolve(&real.to_string_lossy(), &[]).unwrap().0, real.canonicalize().unwrap());
    }

    #[test]
    fn a_subfolder_registered_as_a_project_stays_that_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let web = repo.join("packages/web");
        std::fs::create_dir_all(repo.join(".git")).expect("mkdir");
        std::fs::create_dir_all(web.join("src")).expect("mkdir");
        let known = vec![repo.to_string_lossy().into_owned(), web.to_string_lossy().into_owned()];
        // Inside the subproject: it, not the repository.
        assert_eq!(resolve(&web.join("src").to_string_lossy(), &known).unwrap().0, web);
        // Elsewhere in the repository: the repository.
        assert_eq!(resolve(&repo.join("packages").to_string_lossy(), &known).unwrap().0, repo);
    }
}
