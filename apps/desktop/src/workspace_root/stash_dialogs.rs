//! Host-side modals for the stash section.
//!
//! Every stash dialog lives here rather than in `ops.rs`, which is already
//! past the file-size lint's warn line; this plan adds mount functions in four
//! separate phases and all of them land in this file.
//!
//! # Why the panel cannot mount these itself
//!
//! The panel is a child of the source-control sidebar and knows nothing about
//! the window's modal layer. So it emits an event, the root subscribes, and
//! the root owns the confirm step. The panel exposes no way to drop a stash
//! that does not pass through here.
//!
//! # Three guards, because a dialog can outlive its project
//!
//! Sidebars are cached per project (`workspace/workspace_ops.rs:733`), so
//! switching projects does not destroy the panel or its `Repository` — the
//! old one stays alive in the cache. A dialog callback holding a strong
//! `Entity<StashPanel>` therefore keeps working perfectly after the switch,
//! against the OLD project's repo: open the Drop dialog in project A, switch
//! to B, confirm, and a stash dies in A while the toast lands in B's window.
//!
//! No single guard closes that:
//!
//! 1. `set_active_project` tears the dialog slots down synchronously when the
//!    project id changes, so the prompt is gone before it can be answered.
//! 2. The destructive callbacks re-check the active project id at FIRE time.
//!    Guard 1 depends on where the teardown sits in a function that has
//!    already been moved once for exactly this reason; this one does not
//!    depend on ordering at all.
//! 3. A weak panel handle, which covers only the case where the panel really
//!    has been dropped — never the cached-and-alive one above.

use super::*;

impl WorkspaceRoot {
    /// Mount the confirm dialog for a row's `Drop`.
    ///
    /// The copy names the stash — message, branch, age — because the index
    /// the user clicked tells them nothing about which work is about to
    /// disappear. It does not soften the warning: there is no Undo button,
    /// deliberately, since `git stash store` pushes a restored stash to the
    /// TOP of the stack and would silently reorder every entry away from the
    /// `stash@{N}` addresses the user just read. The recovery sha goes in the
    /// success toast as plain text instead.
    pub(crate) fn mount_drop_stash_dialog(
        &mut self,
        panel: &Entity<StashPanel>,
        ev: &DropStashRequested,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = panel.downgrade();
        let weak_root = cx.entity().downgrade();
        // The project this prompt is asking about. Re-checked at fire time,
        // not trusted from mount time — see the module doc's guard 2.
        let project_id = self.active_project.as_ref().map(|p| p.id.clone());
        let sha = ev.sha.clone();
        // The address the row was painted with, forwarded as a tiebreaker
        // only — `drop_confirmed` still resolves by sha and uses this solely
        // to tell two entries that share one commit apart. See `ops.rs`.
        let painted = ev.stash_ref.index;
        let message = ev.message.clone();
        // Cloned per call because `ConfirmCallback` is an `Rc<dyn Fn>`. The
        // dialog itself `take`s the callback so it fires at most once, but
        // `drop_confirmed` is idempotent regardless: a repeat re-resolves the
        // sha, finds it gone, and toasts instead of dropping a neighbour.
        let on_confirm: ConfirmCallback = Rc::new(move |_window, cx| {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            // Refuse outright if the window moved on. Dropping a stash in a
            // repo the user is no longer looking at is unrecoverable from the
            // UI and invisible until they switch back.
            let still_current = weak_root
                .upgrade()
                .map(|root| {
                    root.read(cx).active_project.as_ref().map(|p| p.id.clone()) == project_id
                })
                .unwrap_or(false);
            if !still_current {
                tracing::warn!(
                    target: "oximux_app::stash_panel",
                    "drop confirm fired after a project switch; ignored",
                );
                return;
            }
            let (sha, message) = (sha.clone(), message.clone());
            panel.update(cx, |p, cx| p.drop_confirmed(sha, painted, message, cx));
        });

        let prompt = ConfirmPrompt {
            title: "Drop this stash?".into(),
            body: describe_stash(ev).into(),
            on_confirm,
            confirm_label: Some("Drop".into()),
            on_cancel: None,
            secondary: None,
        };
        // Refusal means a live prompt is already up; nothing here to undo.
        let _ = self.mount_confirm_dialog(prompt, window, cx);
    }

    /// Mount a `PushStashDialog` for the SCM panel's stash-push
    /// request. Wires `on_confirm` to call `StashPanel::push` with
    /// the user-supplied message + include-untracked toggle. Installs
    /// an observer that drops the dialog from the slot once the user
    /// confirms or cancels.
    ///
    /// First-open-wins: a double-click on the header `+` button (or
    /// any sequence that re-fires `PushStashRequested` while the
    /// dialog is already mounted) is ignored. Replacing the slot
    /// would silently drop a half-typed form.
    pub(crate) fn mount_push_stash_dialog(
        &mut self,
        panel: Entity<StashPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = panel.downgrade();
        let on_confirm: PushCallback = Rc::new(move |msg, include_untracked, _window, cx| {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| p.push(msg, include_untracked, cx));
        });
        self.mount_push_dialog(
            PushStashPrompt {
                on_confirm,
                on_cancel: Some(noop_cancel()),
                scope: None,
            },
            window,
            cx,
        );
    }

    /// Mount the same dialog scoped to the CHANGES panel's selection.
    ///
    /// The stash runs on `StashPanel`, not on the panel that asked for it, so
    /// the stash list refreshes as part of the op — see
    /// `StashPanel::push_paths`. The host is the only place that can see both
    /// panels, which is exactly why this fan-out lives here.
    ///
    /// Silently returns when the source-control surface is not mounted: the
    /// request can only have come from the file list inside it, so that means
    /// the sidebar was torn down between the click and this call.
    pub(crate) fn mount_stash_selection_dialog(
        &mut self,
        git_panel: Entity<GitPanel>,
        ev: &StashSelectedRequested,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(stash_panel) = self
            .right_sidebar
            .as_ref()
            .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
            .map(|sc| sc.read(cx).stash_panel.clone())
        else {
            return;
        };

        let scope = PushStashScope {
            paths: ev.paths.clone(),
            untracked_count: ev.untracked_count,
            has_rename: !ev.rename_pairs.is_empty(),
            needs_untracked: ev.needs_untracked,
        };

        // Sidebars are cached per project, so the panels stay alive across a
        // switch and a strong handle would happily stash in the repo the user
        // has stopped looking at. Re-checked at FIRE time, like the drop
        // dialog's guard 2 — see the module doc.
        let project_id = self.active_project.as_ref().map(|p| p.id.clone());
        let weak_root = cx.entity().downgrade();
        let weak_stash = stash_panel.downgrade();
        let weak_git = git_panel.downgrade();
        let paths = ev.paths.clone();
        let rename_pairs = ev.rename_pairs.clone();

        let on_confirm: PushCallback = Rc::new(move |msg, include_untracked, _window, cx| {
            let Some(stash_panel) = weak_stash.upgrade() else {
                return;
            };
            let still_current = weak_root
                .upgrade()
                .map(|root| {
                    root.read(cx).active_project.as_ref().map(|p| p.id.clone()) == project_id
                })
                .unwrap_or(false);
            if !still_current {
                tracing::warn!(
                    target: "oximux_app::git_panel",
                    "stash-selection confirm fired after a project switch; ignored",
                );
                return;
            }
            // Only on success: a failed push leaves every file exactly where
            // it was, and dropping the selection would make the retry manual.
            let on_success = {
                let weak_git = weak_git.clone();
                Rc::new(move |cx: &mut gpui::App| {
                    let _ = weak_git.update(cx, |panel, cx| panel.clear_selection(cx));
                }) as OnOpSuccess
            };
            let (paths, rename_pairs) = (paths.clone(), rename_pairs.clone());
            stash_panel.update(cx, |p, cx| {
                p.push_paths(
                    msg,
                    include_untracked,
                    paths,
                    rename_pairs,
                    Some(on_success),
                    cx,
                );
            });
        });

        self.mount_push_dialog(
            PushStashPrompt {
                on_confirm,
                on_cancel: Some(noop_cancel()),
                scope: Some(scope),
            },
            window,
            cx,
        );
    }

    /// Put `prompt` in the single push-dialog slot and wire its teardown.
    ///
    /// First-open-wins: a double-click on the header `+` button, or a stash
    /// request arriving while a form is already up, is ignored. Replacing the
    /// slot would silently drop a half-typed message.
    fn mount_push_dialog(
        &mut self,
        prompt: PushStashPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.push_stash_dialog.is_some() {
            return;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| PushStashDialog::new(prompt, theme, density, typography, window, cx));

        // Drop the dialog the moment the user resolves it. Replacing
        // `_push_stash_dialog_observer` cancels any previous observer
        // tied to a stale dialog.
        self._push_stash_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.push_stash_dialog = None;
                    root._push_stash_dialog_observer = None;
                    cx.notify();
                }
            },
        ));

        self.push_stash_dialog = Some(dialog);
        cx.notify();
    }
}

/// The dialog's cancel hook. A no-op on the panel side — the dialog flips
/// `cancelled` and the slot observer drops it — but wired anyway so future
/// telemetry (e.g. counting abandoned pushes) has a hook point.
fn noop_cancel() -> CancelCallback {
    Rc::new(|_window, _cx| {})
}

/// Dialog body naming the stash about to be dropped.
///
/// One paragraph, no newlines: `ConfirmDialog` renders the body as a single
/// 360px-wide text node and every other prompt in the app is written the same
/// way. A destructive dialog is the wrong place to be the first caller to find
/// out how GPUI shapes an embedded `\n`.
///
/// Branch and age are each omitted rather than shown empty. An entry written
/// by `git stash store` carries no `On <branch>:` prefix at all (verified), so
/// a blank branch is normal data, and `“msg” — , 3 months ago` would read as a
/// bug.
fn describe_stash(ev: &DropStashRequested) -> String {
    let message = if ev.message.trim().is_empty() {
        "(no message)"
    } else {
        ev.message.trim()
    };
    let mut line = format!("“{message}”");
    if !ev.branch.trim().is_empty() {
        line.push_str(&format!(" — {}", ev.branch.trim()));
    }
    if !ev.relative.trim().is_empty() {
        line.push_str(&format!(", {}", ev.relative.trim()));
    }
    format!(
        "{line}. Dropping does not apply it. This removes the stash's reflog \
         entry; the commit itself lingers until git collects it, and the toast \
         will show the sha."
    )
}
