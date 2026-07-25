//! Inline rename for the file explorer.
//!
//! Matches the UX shown in the design reference: the file row
//! transforms into an editable input field in-place. Enter commits,
//! Escape cancels, clicking another row cancels. Stem (basename
//! without extension) is pre-selected via `SelectAll` action dispatch
//! — gpui-component's `InputState` doesn't expose a public selection
//! range setter, so we get "whole filename selected" rather than
//! strict stem-only selection. Close enough: typing still replaces
//! everything, and the user can arrow-key to keep the extension.
//!
//! Driven by `WorkspaceRoot::start_inline_file_rename` (Explorer
//! context-menu Rename) → `FileExplorer::start_rename`. The actual
//! `std::fs::rename` lives in `commit_rename` so failure handling
//! stays inside the explorer (the same surface that owns the row
//! refresh).

use crate::shell::file_explorer::FileExplorer;
use gpui::{AppContext, Context, Entity, SharedString, Subscription, Window};
use gpui_component::input::{InputEvent, InputState, Position};
use std::path::PathBuf;

/// In-flight inline rename. Held in `FileExplorer::renaming` so the
/// `uniform_list` row builder can swap the matching row's label for
/// an `Input` widget bound to the same `input_state`.
pub struct RenameState {
    /// Absolute path of the file being renamed. Compared against each
    /// `TreeNode.path` in the row-build loop to decide which row gets
    /// the input instead of the static label.
    pub path: PathBuf,
    /// Editable text. Kept as `Entity<InputState>` so the row can mount
    /// `Input::new(&input)` and let the input's own widget code handle
    /// focus/selection/IME without us reimplementing any of it.
    pub input: Entity<InputState>,
    /// Original basename — used by `commit_rename` to detect a no-op
    /// (user pressed Enter without changing anything) and to derive
    /// the parent directory for the destination path.
    pub original_name: String,
    /// Subscription to `InputEvent::PressEnter` from the input. Held
    /// here so dropping `RenameState` (cancel/commit) tears down the
    /// subscription and we don't fire a stale commit on the next
    /// rename's first Enter press.
    pub _press_enter_sub: Subscription,
}

impl FileExplorer {
    /// Enter inline-rename mode for `path`. Builds a fresh `InputState`
    /// pre-filled with the basename, subscribes to its PressEnter event,
    /// focuses it, and stores the bundle in `self.renaming`.
    ///
    /// Aborts silently if the path has no basename (filesystem root).
    /// If a different rename is already in flight, that one is dropped
    /// without committing — matches macOS Finder's "second Rename
    /// supersedes first" behavior.
    pub fn start_rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(original_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        // Stem length for selection — `file_stem` returns "test" for
        // "test.txt", ".gitignore" for ".gitignore" (Path treats a
        // leading-dot name as all-stem), and "file.tar" for
        // "file.tar.gz". Selects the part before the last dot.
        let stem_chars: u32 = path
            .file_stem()
            .map(|s| s.to_string_lossy().chars().count() as u32)
            .unwrap_or(original_name.chars().count() as u32);
        let has_extension = path.extension().is_some();
        let initial = SharedString::from(original_name.clone());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("New name"));
        input.update(cx, |s, cx| {
            s.set_value(initial, window, cx);
            // Focus so keystrokes land in the input immediately; the
            // user just chose "Rename" — they shouldn't need to click.
            s.focus(window, cx);
            // Park the cursor at the stem/extension boundary so the
            // `SelectToStartOfLine` dispatch below selects only the
            // stem. Skip for files without an extension — they get
            // SelectAll instead so the whole name highlights.
            if has_extension && stem_chars > 0 {
                s.set_cursor_position(
                    Position {
                        line: 0,
                        character: stem_chars,
                    },
                    window,
                    cx,
                );
            }
        });
        // Wire Enter via the input's emitted event. gpui-component's
        // single-line input does `cx.propagate()` after emitting
        // `PressEnter`, so we'd also fire if we hooked the parent key
        // handler — but subscribing to the event is more robust (the
        // event carries focus-context, doesn't double-fire on IME
        // commits, and doesn't depend on key bubbling).
        let press_enter_sub = cx.subscribe_in(
            &input,
            window,
            |me, _input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    me.commit_rename(window, cx);
                }
            },
        );
        // Dispatch the selection action against the now-focused input.
        // With cursor at the stem boundary, `SelectToStartOfLine`
        // extends the selection backward to column 0 — exactly the
        // stem (e.g. "test" highlighted in "test.txt").
        // Files without an extension get `SelectAll` so the whole
        // name highlights (the user almost certainly wants to retype
        // the whole thing).
        if has_extension && stem_chars > 0 {
            window.dispatch_action(Box::new(gpui_component::input::SelectToStartOfLine), cx);
        } else {
            window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
        }
        self.renaming = Some(RenameState {
            path,
            input,
            original_name,
            _press_enter_sub: press_enter_sub,
        });
        cx.notify();
    }

    /// Drop the inline rename without touching the filesystem. Called
    /// by Escape, by click-outside, and by `start_rename` when a new
    /// rename request arrives mid-flight.
    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            self.renaming = None;
            cx.notify();
        }
    }

    /// Run `std::fs::rename` from the current `renaming` state's path
    /// to (parent + typed value), refresh the explorer cache, clear
    /// the rename state. No-op (cancels silently) when the input is
    /// empty, contains path separators, or matches the original name.
    ///
    /// On filesystem failure: logs and cancels. The inline UX has no
    /// good way to surface OS errors (no error band per row), so a
    /// failed rename is treated as "user typed something wrong" and
    /// the row reverts. Future enhancement: route through the toaster
    /// when a notifier surface exists.
    pub fn commit_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.renaming.as_ref() else {
            return;
        };
        let original = state.path.clone();
        let original_name = state.original_name.clone();
        let typed = state.input.read(cx).value().to_string();
        let trimmed = typed.trim();
        if trimmed.is_empty()
            || trimmed.contains('/')
            || trimmed.contains('\\')
            || trimmed == original_name
        {
            self.cancel_rename(cx);
            return;
        }
        let Some(parent) = original.parent() else {
            self.cancel_rename(cx);
            return;
        };
        let target = parent.join(trimmed);
        // Refuse to clobber. macOS `fs::rename` silently overwrites on
        // POSIX, which would destroy data; check first and bail if
        // the target already exists.
        if target.exists() {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                from = %original.display(),
                to = %target.display(),
                "rename aborted: target already exists"
            );
            self.cancel_rename(cx);
            return;
        }
        match std::fs::rename(&original, &target) {
            Ok(()) => {
                tracing::info!(
                    target: "oximux_app::file_explorer",
                    from = %original.display(),
                    to = %target.display(),
                    "rename succeeded"
                );
                // Update the selection so the newly-renamed row stays
                // highlighted after the cache refresh paints over it.
                if self.selected.as_ref() == Some(&original) {
                    self.selected = Some(target.clone());
                }
                self.renaming = None;
                // Reload the parent directory so the row swaps from
                // old name → new name. `manual_refresh` reloads root +
                // every expanded subdir, which is overkill for a
                // single rename but cheap on the cached path and
                // covers the case where the renamed entry was inside
                // a deeply nested dir.
                self.manual_refresh(cx);
                cx.notify();
            }
            Err(err) => {
                tracing::warn!(
                    target: "oximux_app::file_explorer",
                    from = %original.display(),
                    to = %target.display(),
                    error = %err,
                    "rename failed"
                );
                self.cancel_rename(cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Validation rules duplicated here to pin the guards independently
    // of the live gpui pipeline (which needs a Window we can't spin
    // up in a unit test).
    use std::path::PathBuf;

    fn would_commit(typed: &str, original: &str, parent_exists: bool) -> bool {
        let trimmed = typed.trim();
        if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') {
            return false;
        }
        if trimmed == original {
            return false;
        }
        parent_exists
    }

    #[test]
    fn empty_input_does_not_commit() {
        assert!(!would_commit("   ", "foo.txt", true));
    }

    #[test]
    fn slash_input_does_not_commit() {
        assert!(!would_commit("bar/baz", "foo.txt", true));
    }

    #[test]
    fn unchanged_input_does_not_commit() {
        assert!(!would_commit("foo.txt", "foo.txt", true));
    }

    #[test]
    fn valid_changed_input_commits() {
        assert!(would_commit("bar.txt", "foo.txt", true));
    }

    #[test]
    fn parent_path_extraction() {
        let p = PathBuf::from("/tmp/a/b.txt");
        assert_eq!(p.parent(), Some(std::path::Path::new("/tmp/a")));
    }
}
