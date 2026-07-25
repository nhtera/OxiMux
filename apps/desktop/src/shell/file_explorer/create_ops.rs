//! Inline new-file / new-folder creation for the file explorer.
//!
//! Mirrors `rename_ops`: instead of transforming an existing row, an extra
//! placeholder row is injected under the target parent directory (or at the
//! repo root for the background menu). That row renders an editable `Input`;
//! Enter commits (creating the file/folder on disk), Escape / click-outside
//! cancels. The placeholder is identified in the flat row list by a sentinel
//! path (`create_sentinel`) that can never collide with a real entry because
//! it embeds a NUL byte.
//!
//! Driven by `WorkspaceRoot::start_inline_create` (Explorer context-menu
//! New File / New Folder) → `FileExplorer::start_create`.

use crate::shell::file_explorer::FileExplorer;
use gpui::{AppContext, Context, Entity, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};
use std::path::{Path, PathBuf};

/// In-flight inline create. Held in `FileExplorer::creating` so the row
/// builder mounts the input on the injected placeholder row.
pub struct CreateState {
    /// Directory the new entry will be created in.
    pub parent: PathBuf,
    /// `true` → New Folder (mkdir), `false` → New File (touch).
    pub is_dir: bool,
    /// Editable text for the new basename.
    pub input: Entity<InputState>,
    /// Subscription to `InputEvent::PressEnter`. Held so dropping the state
    /// (cancel/commit) tears the subscription down — no stale commit on the
    /// next create's first Enter.
    pub _press_enter_sub: Subscription,
}

/// Sentinel path marking the injected placeholder row for `parent`. Embeds a
/// NUL byte so it can never equal a real filesystem path; the row builder
/// matches on it to mount the create input.
pub fn create_sentinel(parent: &Path) -> PathBuf {
    parent.join("\u{0}__oximux_new__")
}

impl FileExplorer {
    /// Enter inline-create mode under `parent`. Expands `parent` (so the
    /// placeholder shows as a child), builds a focused `InputState`, subscribes
    /// to its PressEnter, and stores the bundle in `self.creating`.
    pub fn start_create(
        &mut self,
        parent: PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A fresh create supersedes any in-flight rename or create.
        self.cancel_rename(cx);
        self.creating = None;

        // Expand the parent (unless it's the repo root, whose children are
        // always shown) so the placeholder row is visible beneath it.
        if parent != self.repo_root && !self.expanded.contains(&parent) {
            self.expanded.insert(parent.clone());
            let needs_load = self
                .cache
                .get(&parent)
                .map(|c| !c.loaded && !c.loading)
                .unwrap_or(true);
            if needs_load {
                let repo_root = self.repo_root.clone();
                let task = self.spawn_load_dir(parent.clone(), repo_root, false, cx);
                self.push_task(task);
            }
        }

        let placeholder = if is_dir { "New folder name" } else { "New file name" };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.update(cx, |s, cx| s.focus(window, cx));
        let press_enter_sub = cx.subscribe_in(
            &input,
            window,
            |me, _input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    me.commit_create(window, cx);
                }
            },
        );
        self.creating = Some(CreateState {
            parent,
            is_dir,
            input,
            _press_enter_sub: press_enter_sub,
        });
        self.recompute_rows();
        cx.notify();
    }

    /// Drop the inline create without touching the filesystem. Called by
    /// Escape, click-outside, and `start_create` when a new request arrives.
    pub fn cancel_create(&mut self, cx: &mut Context<Self>) {
        if self.creating.is_some() {
            self.creating = None;
            self.recompute_rows();
            cx.notify();
        }
    }

    /// Create the file/folder from the current `creating` state at
    /// (parent + typed value), refresh the cache, and reveal it. Cancels
    /// silently on empty input, path separators, or a name collision; logs
    /// and cancels on a filesystem error (the inline UX has no per-row error
    /// band).
    pub fn commit_create(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.creating.as_ref() else {
            return;
        };
        let parent = state.parent.clone();
        let is_dir = state.is_dir;
        let typed = state.input.read(cx).value().to_string();
        let trimmed = typed.trim();
        if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') {
            self.cancel_create(cx);
            return;
        }
        let target = parent.join(trimmed);
        if target.exists() {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                path = %target.display(),
                "create aborted: target already exists"
            );
            self.cancel_create(cx);
            return;
        }
        let result = if is_dir {
            std::fs::create_dir(&target)
        } else {
            std::fs::File::create(&target).map(|_| ())
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    target: "oximux_app::file_explorer",
                    path = %target.display(),
                    is_dir,
                    "create succeeded"
                );
                self.creating = None;
                self.selected = Some(target.clone());
                // Reload so the new entry appears, then scroll it into view.
                self.manual_refresh(cx);
                self.reveal_path(target, cx);
                cx.notify();
            }
            Err(err) => {
                tracing::warn!(
                    target: "oximux_app::file_explorer",
                    path = %target.display(),
                    error = %err,
                    "create failed"
                );
                self.cancel_create(cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_is_stable_and_collision_proof() {
        let parent = PathBuf::from("/repo/src");
        let s = create_sentinel(&parent);
        // Same parent → same sentinel (the row builder relies on equality).
        assert_eq!(s, create_sentinel(&parent));
        // Embeds a NUL, so it can't equal any real path under the parent.
        assert!(s.to_string_lossy().contains('\u{0}'));
        assert_ne!(s, parent.join("real-file.rs"));
    }
}
