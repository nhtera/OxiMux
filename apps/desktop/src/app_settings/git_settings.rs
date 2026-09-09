//! App-side loader + persistence for [`GitSettings`], plus the cached git
//! username the branch-name previews resolve against.
//!
//! **What is cached is the username, not the answer.** An earlier shape cached
//! the *resolved prefix*, which meant the settings pane's own preview could not
//! reflect the control the user was operating: the working copy changed on
//! every keystroke while the preview read a global that only a completed async
//! re-resolution would move. Caching the one genuinely expensive input instead
//! — `git config user.name`, a subprocess, occasionally repo-scoped — lets
//! every caller run the real resolver
//! ([`resolve_prefix_with`](oximux_worktree_ops::branch_name::resolve_prefix_with))
//! synchronously, against whichever settings it means: the pane against its
//! unsaved working copy, everyone else against the saved global.
//!
//! That is what makes "the preview and the branch actually created agree" a
//! property of the code. There is one resolver and one cached input; two
//! callers holding the same settings cannot compute different names.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{App, AsyncApp, Global};
use oximux_settings::git::GitSettings;
use oximux_worktree_ops::branch_name::{branch_name, resolve_prefix_with};

/// `git config user.name` per repository root, as last resolved.
///
/// Keyed by root rather than held as one value because `user.name` is commonly
/// set per repository — a work identity in one checkout, a personal one in
/// another — and because each window owns its own active project. A single
/// app-wide value would let the window you switched projects in decide what
/// the *other* window's branches are called.
///
/// `None` for a root means "asked, and there is no usable name" — a distinct
/// answer from an absent key, which means "not asked yet".
#[derive(Debug, Clone, Default)]
pub struct GitUsernames {
    by_root: HashMap<PathBuf, Option<String>>,
    /// The name resolved with no project in hand, for surfaces that paint
    /// before any project is open (the settings pane at first launch).
    global: Option<String>,
}

impl Global for GitUsernames {}

/// Monotonic stamp so a slow resolution cannot overwrite a newer one.
///
/// Two refreshes are in flight routinely — `install` fires one with no project
/// and the first project activation fires another — and only the
/// `Git username` mode has await points, so it is exactly the mode this
/// matters for that races. Without the stamp the boot-time answer (resolved
/// against the current directory, which for a GUI launch is `/`) can land
/// *after* the project's and stick.
static GENERATION: AtomicU64 = AtomicU64::new(0);

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

/// The cached username for `root`, falling back to the project-less one.
fn username(root: Option<&Path>, cx: &App) -> Option<String> {
    let cache = cx.try_global::<GitUsernames>()?;
    match root.and_then(|r| cache.by_root.get(r)) {
        Some(name) => name.clone(),
        None => cache.global.clone(),
    }
}

/// The prefix `settings` currently resolves to for `root`.
pub fn prefix_for(settings: &GitSettings, root: Option<&Path>, cx: &App) -> Option<String> {
    resolve_prefix_with(settings, username(root, cx).as_deref())
}

/// The branch name `settings` would give a worktree with this slug.
pub fn branch_for(settings: &GitSettings, root: Option<&Path>, slug: &str, cx: &App) -> String {
    branch_name(prefix_for(settings, root, cx).as_deref(), slug)
}

/// [`branch_for`] against the saved settings — what every create path uses.
pub fn branch_for_slug(slug: &str, root: Option<&Path>, cx: &App) -> String {
    branch_for(&settings(cx), root, slug, cx)
}

/// Persist `settings` and swap the global.
///
/// No re-resolution here: the username cache is the only thing that could need
/// refreshing, and changing a *setting* cannot change the user's git identity.
/// Every reader recomputes from the new settings on its next paint.
pub fn save(settings: &GitSettings, cx: &mut App) -> std::io::Result<()> {
    let path =
        settings_path().ok_or_else(|| std::io::Error::other("no app data dir for git.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written before the global is swapped, so a failed write leaves memory
    // agreeing with disk rather than promising a prefix the headless host —
    // which reads the file, not the global — would never use.
    std::fs::write(&path, settings.to_toml_string())?;
    cx.set_global(settings.clone());
    Ok(())
}

/// Resolve `git config user.name` for `project_root` and cache it.
///
/// Called at boot and on every project activation. Cheap to repeat: one
/// subprocess, off the paint path, and the answer only changes when the user
/// edits their git config.
pub fn refresh_username(project_root: Option<PathBuf>, cx: &mut App) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    cx.spawn(async move |cx: &mut AsyncApp| {
        let name = read_user_name(project_root.as_deref()).await;
        cx.update(|cx| {
            // A resolution that started earlier and finished later must not
            // overwrite a newer one — see `GENERATION`.
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            let mut cache = cx.try_global::<GitUsernames>().cloned().unwrap_or_default();
            match project_root {
                Some(root) => {
                    cache.by_root.insert(root, name);
                }
                None => cache.global = name,
            }
            cx.set_global(cache);
        });
    })
    .detach();
}

/// One `git config user.name`, or `None` for every way that can fail.
///
/// A project-less caller falls through to the current directory, which is what
/// git itself would consult for a global `user.name`.
async fn read_user_name(project_root: Option<&Path>) -> Option<String> {
    let root = project_root.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok())?;
    let repo = oximux_git::Repository::open(&root).await.ok()?;
    repo.user_name().await.ok().flatten()
}

/// Load settings and install both globals. Call once from the app's `run`
/// closure, before the first window paints.
///
/// The username cache starts empty rather than seeded, and an empty cache
/// resolves `Git username` to the shipped prefix — its documented degradation.
/// Nothing here spawns a subprocess on the boot path.
pub fn install(cx: &mut App) {
    cx.set_global(load());
    cx.set_global(GitUsernames::default());
    refresh_username(None, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::git::{BranchPrefixMode, DEFAULT_PREFIX};

    fn custom(prefix: &str) -> GitSettings {
        GitSettings {
            branch_prefix: BranchPrefixMode::Custom,
            custom_prefix: prefix.to_string(),
            ..GitSettings::shipped()
        }
    }

    fn by_mode(mode: BranchPrefixMode) -> GitSettings {
        GitSettings { branch_prefix: mode, ..GitSettings::shipped() }
    }

    /// The state every headless test is in: no globals installed at all.
    /// Answering `None` there would silently mint bare branches in a suite
    /// that has always asserted `oximux/<slug>`.
    #[gpui::test]
    fn an_uninstalled_cache_resolves_to_the_shipped_prefix(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert_eq!(branch_for_slug("feat", None, cx), format!("{DEFAULT_PREFIX}/feat"));
        });
    }

    /// The blocker this module was restructured to fix: the settings pane
    /// previews against its unsaved working copy, so the preview line answers
    /// on the same frame as the keystroke that changed it.
    #[gpui::test]
    fn an_unsaved_working_copy_previews_without_being_saved(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(GitSettings::shipped());
            cx.set_global(GitUsernames::default());
            // Saved settings still say `oximux`; the working copy says `team`.
            assert_eq!(branch_for_slug("feat", None, cx), "oximux/feat");
            assert_eq!(branch_for(&custom("team"), None, "feat", cx), "team/feat");
        });
    }

    /// The agreement claim as a property rather than a promise: given the same
    /// settings and the same root, two callers cannot compute different names,
    /// because both run the one resolver over the one cached input.
    #[gpui::test]
    fn two_readers_of_the_same_settings_cannot_disagree(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(GitUsernames {
                global: Some("Ada Lovelace".to_string()),
                ..Default::default()
            });
            cx.set_global(by_mode(BranchPrefixMode::GitUsername));

            let previewed = branch_for_slug("fix-login", None, cx);
            let created = branch_for_slug("fix-login", None, cx);
            assert_eq!(previewed, created);
            assert_eq!(previewed, "ada-lovelace/fix-login");
        });
    }

    /// `user.name` is commonly per repository, and each window has its own
    /// active project. A per-root cache is what stops the window you last
    /// switched projects in from naming the other window's branches.
    #[gpui::test]
    fn each_repository_gets_its_own_username(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = GitUsernames::default();
            cache.by_root.insert(PathBuf::from("/work"), Some("acme-alice".to_string()));
            cache.by_root.insert(PathBuf::from("/oss"), Some("alice".to_string()));
            cache.global = Some("fallback".to_string());
            cx.set_global(cache);
            cx.set_global(by_mode(BranchPrefixMode::GitUsername));

            assert_eq!(branch_for_slug("fix", Some(Path::new("/work")), cx), "acme-alice/fix");
            assert_eq!(branch_for_slug("fix", Some(Path::new("/oss")), cx), "alice/fix");
            // A root nobody has resolved yet falls back rather than guessing.
            assert_eq!(branch_for_slug("fix", Some(Path::new("/new")), cx), "fallback/fix");
        });
    }

    /// An unset `user.name` is an ordinary state, not a reason to mint `/slug`.
    #[gpui::test]
    fn a_root_with_no_username_degrades_to_the_shipped_prefix(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = GitUsernames::default();
            cache.by_root.insert(PathBuf::from("/bare"), None);
            cx.set_global(cache);
            cx.set_global(by_mode(BranchPrefixMode::GitUsername));
            assert_eq!(
                branch_for_slug("fix", Some(Path::new("/bare")), cx),
                format!("{DEFAULT_PREFIX}/fix")
            );
        });
    }

    /// The settings pane and the New Workspace dialog must answer with the
    /// same prefix for the same project.
    ///
    /// Found live, not by a test: the pane resolved against the global git
    /// config while the dialog resolved against the project's, so one setting
    /// previewed `oximux/…` in Settings and `ada-lovelace/…` in the dialog.
    /// Both were individually correct, which is exactly why it read as a bug.
    /// The pane now carries its window's active project root.
    #[gpui::test]
    fn the_pane_and_the_dialog_agree_for_the_same_project(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut cache = GitUsernames::default();
            cache.by_root.insert(PathBuf::from("/proj"), Some("Ada Lovelace".to_string()));
            // A different, global identity — the value the pane used to show.
            cache.global = Some("someone-else".to_string());
            cx.set_global(cache);

            // Saved AND working copy both on `Git username`, so the only
            // variable left is the root — which is what this is about.
            let settings = by_mode(BranchPrefixMode::GitUsername);
            cx.set_global(settings.clone());
            let root = Path::new("/proj");
            let pane = branch_for(&settings, Some(root), "fix-login", cx);
            let dialog = branch_for_slug("fix-login", Some(root), cx);
            assert_eq!(pane, dialog);
            assert_eq!(pane, "ada-lovelace/fix-login");
            // Without the root the pane would have shown the global identity —
            // the disagreement this test exists to prevent.
            assert_eq!(branch_for(&settings, None, "fix-login", cx), "someone-else/fix-login");
        });
    }

    #[gpui::test]
    fn the_none_mode_mints_a_bare_slug(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(GitUsernames::default());
            assert_eq!(branch_for(&by_mode(BranchPrefixMode::None), None, "fix", cx), "fix");
        });
    }
}
