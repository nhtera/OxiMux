//! App-side loader + persistence for [`GitSettings`], plus the resolved branch
//! prefix the UI previews with.
//!
//! **Two globals, and the second one is the point.** [`GitSettings`] is what
//! the user configured; [`ResolvedBranchPrefix`] is what that currently
//! resolves to. They differ because `Git username` is a *rule* — it means
//! `git config user.name`, which costs a subprocess and can be repo-scoped,
//! and neither the workspace dialog nor the chat pill can await anything while
//! painting a preview line.
//!
//! So the resolution happens on the three events that can change the answer —
//! boot, a settings edit, a project switch — and both the preview and the
//! create read the same cached string. That is what makes "the preview and the
//! branch actually created always agree" a property of the code rather than a
//! promise: there is one value, and disagreeing with it would take two.

use std::path::{Path, PathBuf};

use gpui::{App, AsyncApp, Global};
use oximux_settings::git::GitSettings;

/// The branch prefix as currently resolved — `prefix: None` meaning a bare
/// slug — together with the repository it was resolved against.
///
/// The root is carried because `user.name` can be repo-scoped, so "re-resolve"
/// is only a well-formed request if something remembers *where*. Keeping it
/// here rather than asking the caller means the settings pane — which has no
/// idea which project is open — can still ask for a correct re-resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedBranchPrefix {
    pub prefix: Option<String>,
    pub root: Option<PathBuf>,
}

impl Global for ResolvedBranchPrefix {}

fn settings_path() -> Option<PathBuf> {
    crate::app_paths::data_dir().map(|d| d.join(GitSettings::FILE_NAME))
}

fn load() -> GitSettings {
    crate::app_paths::data_dir()
        .map(|d| GitSettings::load_from_dir(&d))
        .unwrap_or_else(GitSettings::shipped)
}

/// The configured settings.
pub fn settings(cx: &App) -> GitSettings {
    cx.try_global::<GitSettings>().cloned().unwrap_or_else(GitSettings::shipped)
}

/// The prefix to put in front of the next branch, already resolved.
///
/// Read by the previews *and* by the create paths — see the module note. Falls
/// back to the shipped prefix when the global has not been installed, which is
/// the state a test that never called [`install`] is in; answering `None` there
/// would silently mint bare branches.
pub fn resolved_prefix(cx: &App) -> Option<String> {
    match cx.try_global::<ResolvedBranchPrefix>() {
        Some(p) => p.prefix.clone(),
        None => Some(oximux_settings::git::DEFAULT_PREFIX.to_string()),
    }
}

/// The branch name the next worktree with this slug would get.
pub fn branch_for_slug(slug: &str, cx: &App) -> String {
    oximux_worktree_ops::branch_name::branch_name(resolved_prefix(cx).as_deref(), slug)
}

/// Persist `settings`, swap the global, and re-resolve the prefix.
///
/// Re-resolution is not optional here: switching to `Git username` changes the
/// preview line the user is looking at *while* they look at it, and a settings
/// pane whose own preview lags its control is worse than no preview.
pub fn save(settings: &GitSettings, cx: &mut App) -> std::io::Result<()> {
    cx.set_global(settings.clone());
    // Against the same repository as last time: the settings pane does not
    // know which project is open, and re-resolving against nothing would
    // silently answer for the global git config instead of this repo's.
    let root = cx.try_global::<ResolvedBranchPrefix>().and_then(|p| p.root.clone());
    refresh_prefix(root, cx);
    let path =
        settings_path().ok_or_else(|| std::io::Error::other("no app data dir for git.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}

/// Re-resolve the prefix against `project_root` and install the result.
///
/// `None` for the root means "no project open": there is no repository to ask
/// for a `user.name`, so `Git username` resolves against the global git config
/// via the current directory instead. That is the same answer git itself would
/// give, and it keeps the pane's preview honest before any project is opened.
pub fn refresh_prefix(project_root: Option<PathBuf>, cx: &mut App) {
    let settings = settings(cx);
    cx.spawn(async move |cx: &mut AsyncApp| {
        let prefix = resolve(&settings, project_root.as_deref()).await;
        cx.update(|cx| cx.set_global(ResolvedBranchPrefix { prefix, root: project_root }));
    })
    .detach();
}

/// Resolve without touching any global — the async half, off the paint path.
async fn resolve(settings: &GitSettings, project_root: Option<&Path>) -> Option<String> {
    use oximux_settings::git::BranchPrefixMode;
    use oximux_worktree_ops::branch_name::{resolve_prefix, resolve_prefix_with};

    // Only `Git username` needs a repository; asking for one in the other two
    // modes would cost a `Repository::open` whose answer is discarded, and
    // would make a project-less window resolve differently from one with a
    // project open — for a setting that does not depend on the project.
    if settings.branch_prefix != BranchPrefixMode::GitUsername {
        return resolve_prefix_with(settings, None);
    }
    // No project open: fall through to the current directory, which is what
    // git itself would consult for a global `user.name`.
    let root = project_root.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok());
    match root {
        Some(root) => match oximux_git::Repository::open(&root).await {
            Ok(repo) => resolve_prefix(settings, &repo).await,
            // Not a repository, or git is missing. `resolve_prefix_with(None)`
            // is the documented degradation — the shipped prefix — and going
            // through it keeps that rule in one place.
            Err(_) => resolve_prefix_with(settings, None),
        },
        None => resolve_prefix_with(settings, None),
    }
}

/// Load settings and install both globals. Call once from the app's `run`
/// closure, before the first window paints.
pub fn install(cx: &mut App) {
    let settings = load();
    // Seeded with the no-username answer so the first paint is already correct
    // for Custom and None, and the spawn below only ever corrects
    // `Git username` — whose seed is the shipped prefix, its documented
    // degradation, rather than a blank.
    let prefix = oximux_worktree_ops::branch_name::resolve_prefix_with(&settings, None);
    cx.set_global(settings);
    cx.set_global(ResolvedBranchPrefix { prefix, root: None });
    refresh_prefix(None, cx);
}
