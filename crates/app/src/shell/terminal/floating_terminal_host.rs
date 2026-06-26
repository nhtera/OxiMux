//! `WorkspaceRoot`-side host logic for the floating terminal card.
//!
//! The card itself (`floating_terminal.rs`) is PTY-free; everything that
//! needs a spawn, the relay, a pane group, or a dialog lives here:
//! toggle (with lazy restore of the persisted tab set), new-tab spawns,
//! expand-to-pane, and the rename dialog. Split out of `workspace_root.rs`
//! to keep both files within budget — same pattern as `workspace_ops`.

use std::path::PathBuf;

use gpui::{AppContext, Context, Window};

use crate::shell::context_env::SurfaceIds;
use crate::shell::floating_terminal::{FloatingTabSpec, FloatingTerminal, FloatingTerminalEvent};
use crate::shell::floating_terminal_persistence::{FloatingTabsBlob, tabs_key};
use crate::shell::toast::ToastKind;
use crate::workspace_root::WorkspaceRoot;

impl WorkspaceRoot {
    /// Toggle the in-window floating terminal. The first dispatch after
    /// launch restores the persisted tab set (relay reattach where the
    /// daemon still holds the PTY, respawn-by-cwd otherwise) or spawns one
    /// fresh tab; later dispatches just flip visibility — the entity stays
    /// alive across hides so the PTYs survive.
    pub(crate) fn toggle_floating_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.floating_terminal.is_some() {
            self.floating_terminal_visible = !self.floating_terminal_visible;
            // Hiding while keyboard focus sits inside the card leaves
            // focus on an element that is no longer rendered — action
            // dispatch then has no focus path, so every chord (including
            // the one that re-shows this card) silently no-ops until the
            // user clicks somewhere. Hand focus back to the active pane.
            if !self.floating_terminal_visible {
                crate::shell::workspace_ops::refocus_active_pane(self, window, cx);
            }
            cx.notify();
            return;
        }
        let key = tabs_key(&self.window_id);
        let blob = FloatingTabsBlob::load(Some(&self.app_state.settings_repo), &key);
        let mut specs = self.floating_specs_from_blob(&blob);
        let mut active = blob.active;
        if specs.is_empty() {
            let Some(spec) = self.fresh_floating_spec(cx) else {
                tracing::warn!("floating terminal: PTY spawn failed; not showing");
                self.push_toast(
                    ToastKind::Error,
                    "Floating terminal failed to start — no PTY available",
                    cx,
                );
                return;
            };
            specs.push(spec);
            active = 0;
        }
        self.mount_floating_terminal(specs, active, key, window, cx);
    }

    /// Resolve the persisted tab set into mountable specs. Reattach is
    /// attempted only when the blob's relay session matches the live
    /// daemon (the pane-restore guard); otherwise — and on any miss — the
    /// tab respawns fresh at its persisted cwd. A tab that can neither
    /// reattach nor respawn is dropped with a log, never an error.
    fn floating_specs_from_blob(&self, blob: &FloatingTabsBlob) -> Vec<FloatingTabSpec> {
        if blob.tabs.is_empty() {
            return Vec::new();
        }
        let snap = crate::shell::terminal_view::relay_state_snapshot();
        let session_ok = matches!(
            (&blob.relay_session, &snap.session_id),
            (Some(persisted), Some(live)) if persisted == live
        );
        let mut specs = Vec::new();
        for tab in &blob.tabs {
            let ids = SurfaceIds::fresh(tab.cwd.clone());
            let reattached = tab.external_id.as_deref().and_then(|ext| {
                (session_ok && snap.live_external_ids.contains(ext))
                    .then(|| crate::shell::terminal_view::attach_pty_existing(ext))
                    .flatten()
            });
            let (backend, session_id) = match reattached {
                Some(pair) => pair,
                None => {
                    match crate::shell::terminal_view::spawn_local_pty(
                        PathBuf::from(&tab.cwd),
                        ids.env(),
                    ) {
                        Some(pair) => pair,
                        None => {
                            tracing::warn!(cwd = %tab.cwd, "floating restore: spawn failed; dropping tab");
                            continue;
                        }
                    }
                }
            };
            specs.push(FloatingTabSpec {
                backend,
                session_id,
                ids,
                cwd: tab.cwd.clone(),
                custom_title: tab.custom_title.clone(),
            });
        }
        specs
    }

    /// One fresh tab spec at the active pane group's cwd (the current
    /// worktree), falling back to the home dir when no project is active.
    /// `None` when no PTY backend is available — callers toast.
    fn fresh_floating_spec(&self, cx: &Context<Self>) -> Option<FloatingTabSpec> {
        let cwd = self
            .active_project_panes()
            .map(|p| p.read(cx).cwd().clone())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let cwd_str = cwd.to_string_lossy().into_owned();
        let ids = SurfaceIds::fresh(cwd_str.clone());
        let (backend, session_id) = crate::shell::terminal_view::spawn_local_pty(cwd, ids.env())?;
        Some(FloatingTabSpec {
            backend,
            session_id,
            ids,
            cwd: cwd_str,
            custom_title: None,
        })
    }

    /// Create the card entity from resolved specs and wire its events.
    fn mount_floating_terminal(
        &mut self,
        specs: Vec<FloatingTabSpec>,
        active: usize,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings_repo = Some(self.app_state.settings_repo.clone());
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let entity = cx.new(|cx| {
            FloatingTerminal::new(
                specs,
                active,
                key,
                settings_repo,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        self._floating_terminal_sub = Some(cx.subscribe_in(
            &entity,
            window,
            |this, _entity, event: &FloatingTerminalEvent, window, cx| match event {
                FloatingTerminalEvent::Close => {
                    this.floating_terminal = None;
                    this.floating_terminal_visible = false;
                    this._floating_terminal_sub = None;
                    cx.notify();
                }
                FloatingTerminalEvent::NewTabRequested => {
                    this.push_floating_tab(window, cx);
                }
                FloatingTerminalEvent::ExpandActiveRequested => {
                    this.expand_floating_tab(window, cx);
                }
                FloatingTerminalEvent::RenameRequested { idx } => {
                    this.open_floating_rename_dialog(*idx, window, cx);
                }
            },
        ));
        self.floating_terminal = Some(entity);
        self.floating_terminal_visible = true;
        cx.notify();
    }

    /// `+` button / Cmd+T while floating-focused: spawn a PTY at the active
    /// workspace cwd and append it as a tab.
    fn push_floating_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ft) = self.floating_terminal.clone() else {
            return;
        };
        let Some(spec) = self.fresh_floating_spec(cx) else {
            self.push_toast(
                ToastKind::Error,
                "Floating tab failed to start — no PTY available",
                cx,
            );
            return;
        };
        ft.update(cx, |f, cx| f.push_tab(spec, window, cx));
    }

    /// `⤢` button: move the active floating tab's view (PTY intact — same
    /// entity, no respawn) into the active pane group as a regular tab,
    /// then activate + focus it there. The card drops itself via `Close`
    /// when this empties it.
    fn expand_floating_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ft) = self.floating_terminal.clone() else {
            return;
        };
        let Some(panes) = self.active_project_panes() else {
            self.push_toast(
                ToastKind::Info,
                "Open a project to expand the floating terminal into a pane",
                cx,
            );
            return;
        };
        let Some(group) = panes.read(cx).active_group() else {
            return;
        };
        let Some((title, view, session_id)) = ft.update(cx, |f, cx| f.take_active_tab(cx)) else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_restored_terminal_tab(title, view, cx);
        });
        panes.update(cx, |p, cx| {
            p.activate_terminal_session(session_id, window, cx);
        });
    }

    /// Double-click on a tab chip: reuse the pane-tab rename dialog,
    /// committing back into the floating tab's custom title.
    fn open_floating_rename_dialog(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ft) = self.floating_terminal.clone() else {
            return;
        };
        let initial = ft.read(cx).tab_title(idx);
        let weak_root: gpui::WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let weak_ft = ft.downgrade();
        let on_commit: crate::shell::rename_tab_dialog::RenameCallback =
            std::rc::Rc::new(move |outcome, _window, cx| {
                use crate::shell::rename_tab_dialog::RenameOutcome;
                if let Some(f) = weak_ft.upgrade() {
                    match outcome {
                        RenameOutcome::Save(value) => {
                            let title = value.to_string();
                            f.update(cx, |f, cx| f.set_tab_title(idx, Some(title), cx))
                        }
                        RenameOutcome::Reset => {
                            f.update(cx, |f, cx| f.set_tab_title(idx, None, cx))
                        }
                        RenameOutcome::Cancel => {}
                    }
                }
                let _ = weak_root.update(cx, |this, cx| {
                    this.rename_tab_dialog = None;
                    cx.notify();
                });
            });
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| {
            crate::shell::rename_tab_dialog::RenameTabDialog::new(
                "Rename Floating Tab".into(),
                initial.into(),
                on_commit,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        // Focus the dialog's input AFTER it's mounted so the user can type
        // immediately (focus before assignment is a no-op).
        dialog.read(cx).input_focus_handle(cx).focus(window, cx);
        self.rename_tab_dialog = Some(dialog);
        cx.notify();
    }
}
