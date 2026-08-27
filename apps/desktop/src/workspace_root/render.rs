use super::*;

/// Static registry slug for an import-provider preset id, for the `&'static str`
/// `adapter_id` the spawn layer + settings lookups expect.
fn import_preset_slug(id: &str) -> &'static str {
    match id {
        "opencode" => "opencode",
        "copilot" => "copilot",
        "pi" => "pi",
        "omp" => "omp",
        _ => "custom",
    }
}

impl Render for WorkspaceRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // Push sidebar data down before LeftRail::render runs in the tree.
        self.refresh_left_rail(cx);

        // Tell the panes whether a modal/overlay is covering them this frame,
        // so any embedded browser webview (a native view above the GPU canvas)
        // hides instead of floating over the modal. Set before the panes
        // render below.
        let panes_covered = self.palette.read(cx).is_open()
            || self.project_picker.read(cx).is_open()
            || self.settings_modal.read(cx).is_open()
            || self.workspace_dialog.read(cx).is_open()
            || self.add_project_dialog.read(cx).is_open()
            || self.adapter_picker.read(cx).is_open()
            || self.pane_actions.read(cx).is_open()
            || self.tab_context_menu.read(cx).is_open()
            || self.file_tree_context_menu.read(cx).is_open()
            || self.git_row_context_menu.read(cx).is_open()
            || self.commit_context_menu.read(cx).is_open()
            || self.terminal_context_menu.read(cx).is_open()
            || self.row_menu.read(cx).is_open()
            || self.project_menu.read(cx).is_open()
            || self.dashboard_status_menu.read(cx).is_open()
            || self.options_menu.read(cx).is_open()
            || self.floating_terminal_visible
            || self.confirm_dialog.is_some()
            || self.rename_tab_dialog.is_some()
            || self.push_stash_dialog.is_some();
        cx.set_global(crate::shell::browser_view::WebviewSuppressed(panes_covered));
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        // Refresh toast tokens each render (same push-down doctrine as the
        // rail/pane surfaces); store-only, no notify.
        self.toast_layer.update(cx, |layer, _| {
            layer.set_tokens(theme, density, typography.clone());
        });
        self.dictation_hud.update(cx, |hud, _| {
            hud.set_tokens(theme, density, typography.clone());
        });

        // Status-bar pane count = visible pane-group leaves in the active
        // project (1 when no splits, N after Cmd+D).
        let active_panes = self.active_project_panes();
        let pane_count = active_panes
            .as_ref()
            .map(|p| p.read(cx).manager().in_order_groups().len())
            .unwrap_or(0);

        // Aggregate agent count across every group in the active project:
        // spawned `Agent` tabs PLUS plain terminals running a hand-launched
        // agent (detected from the terminal title). Users care about total
        // agents in flight, not just the foreground group's.
        let agent_count = active_panes
            .as_ref()
            .map(|p| {
                let panes = p.read(cx);
                panes.agent_count(cx) + panes.ambient_agent_count(cx)
            })
            .unwrap_or(0);

        // True TTY count = terminal + agent tabs across every group.
        let tty_count = active_panes
            .as_ref()
            .map(|p| p.read(cx).tty_count(cx))
            .unwrap_or(0);

        // Listening ports, as of the last scan. Read from the panel rather
        // than recomputed: the scan is the only thing that talks to the
        // kernel, and a render pass must never be the second caller.
        let port_count = self.ports_panel.read(cx).count();

        // Route poll state to the status bar via the RightSidebar getter.
        // Non-git projects keep their PollState pinned at Loading forever
        // (no poller exists), so gate on `has_repo` to keep the status
        // bar from showing a perpetual "loading git…" placeholder.
        let git_state = self.right_sidebar.as_ref().and_then(|s| {
            let sidebar = s.read(cx);
            if sidebar.has_repo() {
                Some(sidebar.latest_poll_state().clone())
            } else {
                None
            }
        });

        // Activity-bar tabs are composed here so top_bar / right_sidebar stay
        // decoupled. Tabs only render when the sidebar is open.
        let (right_open, right_tabs) = match self.right_sidebar.as_ref() {
            Some(sidebar_entity) => {
                let sidebar_ref = sidebar_entity.read(cx);
                let open = sidebar_ref.open;
                let tabs_element = if open {
                    let tabs = sidebar_ref.visible_tabs();
                    let active = sidebar_ref.active_tab;
                    Some(
                        render_tab_buttons(active, &tabs, sidebar_entity, theme).into_any_element(),
                    )
                } else {
                    None
                };
                (open, tabs_element)
            }
            None => (false, None),
        };

        // Push current chrome width into the active ProjectPanes so PTY
        // grids match the actual visible area. ProjectPanes forwards the
        // value into each group it owns.
        // Read the live rail width through the entity so pane grids
        // reflow on every resize-drag tick (set_width's cx.notify
        // triggers a render which re-runs this read).
        let left_chrome = if self.left_rail_open {
            f32::from(self.left_rail.read(cx).width())
        } else {
            0.0
        };
        // Phase 13: the sidebar width is now state, not the
        // DEFAULT_PANEL_WIDTH const. Read the live width through the
        // sidebar entity so PTY grids reflow correctly on every drag
        // tick (set_panel_width's cx.notify triggers a render, which
        // re-runs this read).
        let right_chrome = if right_open {
            self.right_sidebar
                .as_ref()
                .map(|s| f32::from(s.read(cx).panel_width()))
                .unwrap_or_else(|| f32::from(DEFAULT_PANEL_WIDTH))
        } else {
            0.0
        };
        if let Some(panes) = active_panes.as_ref() {
            let chrome = left_chrome + right_chrome;
            panes.update(cx, |p, cx| p.set_chrome_width(chrome, cx));
        }

        // Push the active workspace's tint down so its tab strip's active-tab
        // edge carries the identifier hue. Resolved from the (already-refreshed)
        // left-rail snapshot, so no separately-cached copy can desync.
        let ws_tint = self.left_rail.read(cx).active_workspace_tint();
        if let Some(panes) = active_panes.as_ref() {
            panes.update(cx, |p, _cx| p.set_workspace_tint(ws_tint));
        }

        // Top-row pane groups hoist their per-group strips INTO the
        // top-bar row (mirroring tree column widths). Lower vertical-
        // split rows render their strips inline above their bodies.
        let workspace_tab_strip: Option<AnyElement> = active_panes
            .as_ref()
            .and_then(|panes| panes.read(cx).topmost_tab_strip((*panes).clone(), cx));
        // Drag-claim band (below) is only needed when there are tab
        // chips to protect from AppKit title-bar drag hijack. When
        // there are no tabs (welcome state), skipping the band trims
        // ~22px of dead chrome and aligns the empty-state chrome with
        // the reference editor's compact single-row header.
        let has_tabs = workspace_tab_strip.is_some();

        // Center column body: active project's ProjectPanes (which renders
        // its group tree internally), or welcome placeholder when no
        // project is active.
        let center_body: AnyElement = match active_panes.clone() {
            Some(view) => view.into_any_element(),
            None => main_area::view(theme, density, typography).into_any_element(),
        };

        // Per-column header layout — single 30-px chrome band at top
        // with each column owning its own header segment side-by-side:
        //   - left_header  : traffic-light gutter + wordmark + left toggle
        //   - center_header: collapsed clusters when rails closed;
        //                    otherwise just a flex spacer (the tab
        //                    strip lives in its OWN row below — see
        //                    `strip_row` further down)
        //   - right_header : activity tabs + right toggle (only when
        //                    sidebar open)
        //
        // Because each header is per-column, the activity tabs naturally
        // dock at the LEFT edge of the right sidebar (NOT at the far
        // right of the window).
        //
        // The per-pane tab strip is hoisted into a SEPARATE row inside
        // the center column, BELOW center_header — keeps tab chips at
        // `y > 30`, safely outside AppKit's title-bar drag zone (top
        // ~28px). Required for chip drag-reorder to work — empirically
        // verified: putting chips at `y < 28` (e.g. inside
        // center_header at y=1..29) breaks GPUI drag delivery.

        // Activity tabs route through the OPEN right column's header
        // when the sidebar is open; otherwise they collapse into the
        // center header (so the user can still see them).
        let (center_right_tabs, right_column_tabs) = if right_open {
            (None, right_tabs)
        } else {
            (right_tabs, None)
        };

        // Title-bar Update pill: staged (downloaded + verified) update only.
        // Rendered by whichever header currently hosts the left chrome
        // cluster, so it survives collapsing the rail.
        // Always `None` where there is no updater, which renders no pill.
        #[cfg(any(target_os = "macos", windows))]
        let update_ready_version = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|state| state.ready_version().map(str::to_string));
        #[cfg(not(any(target_os = "macos", windows)))]
        let update_ready_version: Option<String> = None;

        let left_column = if self.left_rail_open {
            Some(
                div()
                    .flex()
                    .flex_col()
                    .h_full()
                    .flex_shrink_0()
                    .child(top_bar::left_header(
                        update_ready_version.is_some(),
                        theme,
                        density,
                        typography,
                    ))
                    .child(self.left_rail.clone()),
            )
        } else {
            None
        };

        // Tab strip row height matches the chrome row (h_top_bar) for
        // visual symmetry — two equal-height rows stacked.
        // Chips inside still render at their natural 28px height
        // (`TAB_STRIP_HEIGHT_PX`); `items_center` on the outer row
        // gives them top/bottom breathing room inside the 36px band.
        let strip_row_height_px = density.h_top_bar;
        let center_column = {
            // IDE-style command center in the center chrome zone (the tab
            // strip lives in its OWN row below — not here). Resting label is the
            // active project name; click opens Quick Open.
            let command_center = top_bar::command_center(
                self.active_project.as_ref().map(|p| p.name.clone()),
                theme,
                density,
                typography,
            )
            .into_any_element();
            let header = top_bar::center_header(
                self.left_rail_open,
                right_open,
                update_ready_version.is_some(),
                Some(command_center),
                center_right_tabs,
                has_tabs, // suppress header bottom border when the tab strip renders below
                theme,
                density,
                typography,
            );
            let strip_row = workspace_tab_strip.map(|strip| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .h(px(strip_row_height_px))
                    .bg(theme.bg_panel)
                    // No bottom border here: the hoisted tab strip already
                    // paints its own bottom border (the focused-group accent),
                    // so a border on this wrapper would double it into a
                    // parallel hairline a couple px below.
                    .child(strip)
            });
            let body = div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .child(center_body);
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .child(header)
                .when_some(strip_row, |s, r| s.child(r))
                .child(body)
        };

        let right_column = match (self.right_sidebar.clone(), right_open) {
            (Some(sidebar), true) => Some(
                div()
                    .flex()
                    .flex_col()
                    .h_full()
                    .flex_shrink_0()
                    .child(top_bar::right_header(right_column_tabs, theme, density))
                    .child(sidebar),
            ),
            _ => None,
        };

        let mut row = div().flex().flex_row().flex_1().min_h(px(0.)).w_full();
        if let Some(col) = left_column {
            row = row.child(col);
        }
        row = row.child(center_column);
        if let Some(col) = right_column {
            row = row.child(col);
        }

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            // Phase 13: route sidebar-resize drag ticks. The handle
            // itself lives inside RightSidebar (left edge of the
            // column); the move listener has to live on a parent
            // wide enough to keep the cursor inside its bounds for
            // the duration of the drag — `size_full` qualifies. The
            // handler reads the cursor's window-relative x and the
            // listener-div's LIVE bounds width (`ev.bounds.size`),
            // so an OS-window resize mid-drag immediately shifts the
            // clamp ceiling without staring at a stale snapshot. The
            // payload's `window_width` is a fallback for the rare
            // frame where bounds are still zero (pre-layout).
            .on_drag_move::<crate::shell::right_sidebar::resize::SidebarResizePayload>(
                cx.listener(
                    |this, ev: &DragMoveEvent<
                        crate::shell::right_sidebar::resize::SidebarResizePayload,
                    >, _window, cx| {
                        let Some(sidebar) = this.right_sidebar.clone() else {
                            return;
                        };
                        let live_width = f32::from(ev.bounds.size.width);
                        let window_width = if live_width > 0.0 {
                            live_width
                        } else {
                            ev.drag(cx).window_width
                        };
                        let cursor_x = f32::from(ev.event.position.x);
                        sidebar.update(cx, |s, cx| {
                            crate::shell::right_sidebar::resize::apply_drag_move(
                                s,
                                cursor_x,
                                window_width,
                                cx,
                            );
                        });
                    },
                ),
            )
            // Route left-rail resize drag ticks. The handle lives on the
            // rail's right edge; the move listener sits on this full-size
            // row so the cursor stays inside its bounds for the whole
            // drag. The rail's left edge is pinned at window x=0, so the
            // new width is simply the cursor's window x.
            .on_drag_move::<crate::shell::left_rail::resize::LeftRailResizePayload>(
                cx.listener(
                    |this,
                     ev: &DragMoveEvent<
                        crate::shell::left_rail::resize::LeftRailResizePayload,
                    >,
                     _window,
                     cx| {
                        let cursor_x = f32::from(ev.event.position.x);
                        this.left_rail.update(cx, |rail, cx| {
                            crate::shell::left_rail::resize::apply_drag_move(rail, cursor_x, cx);
                        });
                    },
                ),
            )
            // Route commit-graph height-resize drag ticks. The handle lives
            // at the graph section's top edge inside the SCM panel; like the
            // sidebar/rail handles its move listener has to sit on this
            // full-size row so the cursor stays inside the listener's bounds
            // for the whole drag (and so it keeps firing while the cursor
            // travels over the graph's own commit rows — a listener nested
            // inside the SCM panel stops firing over child entities). The
            // window height for the clamp ceiling comes off this row's live
            // bounds. Reaches the graph through right_sidebar → source_control.
            .on_drag_move::<crate::shell::source_control::graph::GraphResizePayload>(
                cx.listener(
                    |this,
                     ev: &DragMoveEvent<
                        crate::shell::source_control::graph::GraphResizePayload,
                    >,
                     _window,
                     cx| {
                        let cursor_y = f32::from(ev.event.position.y);
                        let window_height = f32::from(ev.bounds.size.height);
                        let Some(sidebar) = this.right_sidebar.clone() else {
                            return;
                        };
                        sidebar.update(cx, |s, cx| {
                            if let Some(panel) = s.source_control.clone() {
                                panel.update(cx, |p, cx| {
                                    p.commit_graph.update(cx, |g, cx| {
                                        g.apply_graph_drag(cursor_y, window_height, cx);
                                    });
                                });
                            }
                        });
                    },
                ),
            )
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _window, cx| {
                this.left_rail_open = !this.left_rail_open;
                cx.notify();
            }))
            // Interface zoom. No `cx.notify()` on any of the three: the setter
            // refreshes every window, which is the point — one view notifying
            // itself would leave the other fifty at the old size.
            .on_action(cx.listener(|_this, _: &UiZoomIn, _window, cx| {
                crate::appearance_settings::zoom_in(cx);
            }))
            .on_action(cx.listener(|_this, _: &UiZoomOut, _window, cx| {
                crate::appearance_settings::zoom_out(cx);
            }))
            .on_action(cx.listener(|_this, _: &UiZoomReset, _window, cx| {
                crate::appearance_settings::zoom_reset(cx);
            }))
            // ⌘E dictates into whatever text pane is focused. A focused chat
            // composer consumes this first (its own handler stops propagation);
            // reaching here means a terminal/editor/other pane is focused, so
            // route to the global HUD (or toast if the pane can't take text).
            .on_action(cx.listener(|this, _: &ToggleDictation, window, cx| {
                this.dictate_focused_pane(window, cx);
            }))
            .on_action(cx.listener(
                |this, action: &SendTextToActiveAgent, _window, cx| {
                    // Resolve the routing target on the spot: the active
                    // project's first agent session, preferring the
                    // currently-focused tab. No-op when nothing is open.
                    let Some(panes) = this.active_project_panes() else {
                        tracing::debug!("send-to-agent: no active project");
                        return;
                    };
                    let Some(session_id) = panes.read(cx).target_agent_session(cx) else {
                        tracing::debug!("send-to-agent: no agent session available");
                        return;
                    };
                    let runtime = this.cli_runtime.clone();
                    let text = action.text.clone();
                    // `send_agent_paste` offloads the PTY write via
                    // `tokio::spawn_blocking`, which needs a live Tokio reactor.
                    // GPUI's background executor has none, so run on the app
                    // runtime (entered on the main thread for the app's life;
                    // this listener fires there) — same rationale as
                    // `run_diff_refresh_round`. `background_spawn` here aborts
                    // the process with "no reactor running".
                    let Ok(handle) = tokio::runtime::Handle::try_current() else {
                        tracing::warn!(?session_id, "no tokio runtime; send-to-agent dropped");
                        return;
                    };
                    handle.spawn(async move {
                        // Bracketed-paste-wrap so a MULTI-LINE selection /
                        // command output lands as one reviewable block instead
                        // of the agent's readline executing each line.
                        if let Err(err) = runtime.send_agent_paste(session_id, &text).await {
                            tracing::warn!(?session_id, %err, "send-to-agent failed");
                        }
                    });
                },
            ))
            .on_action(cx.listener(
                |this, action: &CreateWorktreeWorkspaceForActiveChat, _window, cx| {
                    // A *New Agent* worktree draft sent: make the worktree a
                    // first-class `Workspace` (DB row + git worktree via
                    // `create_workspace_with_rollback`, full rollback on failure)
                    // so it gets a sidebar card + ⌘J entry, then hand the outcome
                    // back to the chat that raised it. The leaf owns no
                    // `WorkspaceRepo`; this is the single seam that does.
                    let Some(project) = this.active_project.clone() else {
                        tracing::warn!("create-worktree-workspace: no active project");
                        return;
                    };
                    let Some(view) = this
                        .active_project_panes()
                        .and_then(|panes| panes.read(cx).active_agent_chat_view(cx))
                    else {
                        tracing::warn!("create-worktree-workspace: no active chat view");
                        return;
                    };
                    let weak_view = view.downgrade();
                    let slug = action.slug.clone();
                    let project_root = std::path::PathBuf::from(&project.root_path);
                    // Sibling `oximux-wt-<slug>` path — the same convention the
                    // manual "New Worktree" form and the retired git-only path
                    // used, so both land in one place.
                    let worktree_path = std::path::PathBuf::from(
                        crate::shell::worktree_panel::list_render::suggest_worktree_path(
                            &project_root,
                            &slug,
                        ),
                    );
                    let workspace_repo = this.app_state.workspace_repo.clone();
                    let project_id = project.id.clone();
                    cx.spawn(async move |weak_root, cx| {
                        use crate::shell::workspace_ops::{
                            ChatWorktreeOutcome, CreateOutcome, create_workspace_with_rollback,
                        };
                        // `name` = slug: the human label derived from the branch
                        // slug (collision handled inside `insert`).
                        let outcome = create_workspace_with_rollback(
                            &project_root,
                            &project_id,
                            &slug,
                            &slug,
                            &worktree_path,
                            None,
                            &workspace_repo,
                        )
                        .await;
                        let chat_outcome = match outcome {
                            CreateOutcome::Created(ws) => ChatWorktreeOutcome::Created {
                                path: std::path::PathBuf::from(&ws.worktree_path),
                                branch: ws.branch.clone(),
                            },
                            CreateOutcome::GitFailed(msg) => ChatWorktreeOutcome::GitFailed(msg),
                            CreateOutcome::StorageFailedRollbackClean(err) => {
                                ChatWorktreeOutcome::GitFailed(err.to_string())
                            }
                            CreateOutcome::StorageFailedRollbackDirty {
                                insert_error,
                                rollback_error,
                            } => ChatWorktreeOutcome::GitFailed(format!(
                                "{insert_error}; rollback: {rollback_error}"
                            )),
                        };
                        // Refresh the rail so the new workspace card + ⌘J entry
                        // appear without a manual reload.
                        let _ = weak_root.update(cx, |root, cx| root.mark_rail_dirty(cx));
                        // Hand the outcome to the chat: on success it rebinds cwd
                        // and resumes the staged send; on failure it surfaces the
                        // error banner with Retry / continue-without-worktree.
                        let _ = weak_view
                            .update(cx, |v, cx| v.on_worktree_create_outcome(chat_outcome, cx));
                    })
                    .detach();
                },
            ))
            .on_action(cx.listener(|this, _: &OpenQuickOpen, window, cx| {
                // Mutex with every other full-window overlay (close-then-open).
                this.close_modal_overlays(cx);
                let root = this
                    .active_project
                    .as_ref()
                    .map(|p| std::path::PathBuf::from(&p.root_path));
                this.palette.update(cx, |p, cx| {
                    p.open(PaletteMode::QuickOpen, window, cx);
                    // Lazily build the file index for the active project; the
                    // call is a no-op when already loaded for this project.
                    if let Some(root) = root {
                        p.kick_file_index(root, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.close_modal_overlays(cx);
                this.palette
                    .update(cx, |p, cx| p.open(PaletteMode::Commands, window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenSessionHistory, window, cx| {
                this.close_modal_overlays(cx);
                // Default the picker to the active project's sessions (root +
                // worktrees), mirroring the agent CLI's same-repo /resume; an
                // empty scope opens the all-projects view.
                let scope = this.active_project_scope_paths();
                this.session_history
                    .update(cx, |m, cx| m.open(scope, window, cx));
            }))
            // Root fallback for the composer chord. `OpenComposerBar` is
            // handled at the PaneGroup level, so a Cmd+I dispatched while
            // focus sits outside the pane tree (left rail, a just-closed
            // modal, fresh launch) would otherwise silently no-op. GPUI
            // consumes the action at the deepest handler, so when the pane
            // IS focused this fallback never runs — no double-toggle.
            .on_action(cx.listener(|this, action: &OpenComposerBar, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                let Some(group) = panes.read(cx).active_group() else {
                    return;
                };
                group.update(cx, |g, cx| g.on_open_composer_bar(action, window, cx));
            }))
            .on_action(cx.listener(|this, action: &ResumeAgentSession, window, cx| {
                let resumption = if action.fork {
                    oximux_core::SessionResumption::Fork {
                        id: action.session_id.clone(),
                    }
                } else {
                    oximux_core::SessionResumption::Resume {
                        id: action.session_id.clone(),
                    }
                };
                // An import-provider row (OpenCode/Copilot/Pi) resumes as a
                // Custom PTY running the provider's own resume TUI; native rows
                // resume through their adapter. `adapter`/`adapter_id`/
                // `custom_command` are set together so the two paths can't drift.
                // An import-provider row whose handle the resume seam REFUSES
                // (today: an omp id that is not a full canonical UUID — only
                // reachable via a torn rollout header) must stop here with a
                // toast that names the provider. Falling through would try the
                // row's `Custom` adapter with no command and surface a
                // baffling "Start custom agent" plumbing error instead.
                if let Some(preset) = action.preset_id.as_deref()
                    && oximux_settings::import_resume_command(preset, &action.resume_handle)
                        .is_none()
                {
                    crate::shell::toast::toast_op_error(
                        cx,
                        "Resume session",
                        &format!(
                            "the {preset} session id {:?} is not resumable (corrupted entry?) — try re-opening the history picker",
                            action.resume_handle
                        ),
                    );
                    return;
                }
                let (adapter, adapter_id, resumption, custom_command) = match action
                    .preset_id
                    .as_deref()
                    .and_then(|id| {
                        oximux_settings::import_resume_command(id, &action.resume_handle)
                            .map(|cmd| (import_preset_slug(id), cmd))
                    }) {
                    Some((slug, cmd)) => (
                        oximux_core::AgentAdapter::Custom,
                        slug,
                        oximux_core::SessionResumption::None,
                        Some(cmd),
                    ),
                    None => (
                        action.adapter,
                        crate::shell::session_history::picker::adapter_slug(action.adapter),
                        resumption,
                        None,
                    ),
                };
                // Codex index rows carry no cwd — root the relaunch at the
                // active project so the agent lands in the right worktree.
                let cwd = if action.cwd.is_empty() {
                    this.active_project_panes()
                        .map(|panes| panes.read(cx).cwd().clone())
                        .or_else(|| {
                            this.active_project
                                .as_ref()
                                .map(|p| std::path::PathBuf::from(&p.root_path))
                        })
                        .unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        })
                } else {
                    std::path::PathBuf::from(&action.cwd)
                };
                this.spawn_agent_tab(
                    adapter,
                    adapter_id,
                    cwd,
                    None,
                    None,
                    None,
                    resumption,
                    custom_command,
                    // A history relaunch reuses the adapter's default profile;
                    // the persisted history row records no profile.
                    None,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenWorkspaceJump, window, cx| {
                this.close_modal_overlays(cx);
                // Snapshot all workspaces + attention state, push into the
                // palette, then open it in jump mode.
                let items = this.build_workspace_jump_items(cx);
                this.palette.update(cx, |p, cx| {
                    p.set_workspace_items(items, cx);
                    p.open(PaletteMode::WorkspaceJump, window, cx);
                });
            }))
            .on_action(cx.listener(
                |this, action: &ActivateWorkspaceFromJump, window, cx| {
                    this.activate_workspace_from_jump(
                        action.workspace_id.clone(),
                        action.project_id.clone(),
                        action.worktree_path.clone(),
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &oximux_editor::RevealInExplorer, _window, cx| {
                    this.reveal_path_in_explorer(std::path::PathBuf::from(&action.path), cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::actions::NavWorkspaceBack, window, cx| {
                    this.nav_workspace_back(window, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::actions::NavWorkspaceForward, window, cx| {
                    this.nav_workspace_forward(window, cx);
                },
            ))
            .on_action(cx.listener(|this, _: &crate::actions::ReloadCustomCommands, _window, cx| {
                this.reload_custom_commands(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenWorkspaceCreate, window, cx| {
                let projects = this.app_state.recent_projects.clone();
                let active = this.active_project.clone();
                this.close_modal_overlays(cx);
                this.workspace_dialog
                    .update(cx, |d, cx| d.open_create(projects, active, window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenAddProjectDialog, window, cx| {
                this.close_modal_overlays(cx);
                this.add_project_dialog
                    .update(cx, |d, cx| d.open(window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenProjectPicker, window, cx| {
                if this.project_picker.read(cx).is_open() {
                    this.project_picker.update(cx, |p, cx| p.close(cx));
                    return;
                }
                let projects = this.app_state.recent_projects.clone();
                this.close_modal_overlays(cx);
                this.project_picker
                    .update(cx, |p, cx| p.open(projects, window, cx));
            }))
            .on_action(cx.listener(|_this, _: &RestartToUpdate, _window, cx| {
                // Only reachable from the Update pill, which does not render
                // without an updater to stage anything.
                #[cfg(any(target_os = "macos", windows))]
                crate::updater::restart_to_update(cx);
                #[cfg(not(any(target_os = "macos", windows)))]
                let _ = cx;
            }))
            .on_action(cx.listener(|this, _: &crate::actions::ToggleWhatsNew, _window, cx| {
                this.whats_new_open = !this.whats_new_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                // Toggle: a second Cmd+, (or cog click) closes it.
                if this.settings_modal.read(cx).is_open() {
                    this.settings_modal.update(cx, |m, cx| m.close(cx));
                    return;
                }
                this.close_modal_overlays(cx);
                this.settings_modal.update(cx, |m, cx| m.open(window, cx));
            }))
            .on_action(cx.listener(|this, _: &crate::actions::OpenAbout, window, cx| {
                this.open_about(window, cx);
            }))
            .on_action(cx.listener(
                |this, _: &crate::actions::CheckForUpdates, window, cx| {
                    // The pane first, then the check. The About pane is the
                    // only surface that renders update status, so starting a
                    // check without opening it would look like the menu item
                    // did nothing at all.
                    this.open_about(window, cx);
                    #[cfg(any(target_os = "macos", windows))]
                    crate::updater::check_now(cx);
                },
            ))
            .on_action(cx.listener(|this, _: &ShowWelcomeWizard, window, cx| {
                if this.onboarding.read(cx).is_open() {
                    return;
                }
                this.close_modal_overlays(cx);
                this.onboarding.update(cx, |wizard, cx| wizard.open(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleFloatingTerminal, window, cx| {
                this.toggle_floating_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleChatTerminalView, window, cx| {
                this.toggle_chat_terminal_view(window, cx);
            }))
            .on_action(
                cx.listener(|this, action: &RequestOpenAdapterPicker, window, cx| {
                    // Anchor precedence:
                    //   `Some(px)` → mouse path; popover lands under cursor.
                    //   `None`     → keyboard path; use the post-rail fallback.
                    let fallback_anchor = if this.left_rail_open {
                        this.density.w_left_rail + ADAPTER_PICKER_LEFT_INSET
                    } else {
                        ADAPTER_PICKER_LEFT_INSET
                    };
                    let left_anchor = action.x.unwrap_or(fallback_anchor);
                    // Mutex: only one full-window popover can hold the click-outside path.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.git_row_context_menu.update(cx, |m, cx| m.close(cx));
                    this.adapter_picker
                        .update(cx, |p, cx| p.open(left_anchor, window, cx));
                }),
            )
            .on_action(cx.listener(|this, _: &OpenPaneActions, _window, cx| {
                // Right-edge anchor: matches the "..." button position relative to
                // the right column when sidebar is open / center toggle when closed.
                // Reads the live sidebar width (Phase 13) so the anchor tracks
                // a freshly-dragged sidebar without staring at the old default.
                let (r_open, r_width) = match this.right_sidebar.as_ref() {
                    Some(s) => {
                        let read = s.read(cx);
                        (read.open, f32::from(read.panel_width()))
                    }
                    None => (false, f32::from(DEFAULT_PANEL_WIDTH)),
                };
                let right_anchor = if r_open {
                    r_width
                } else {
                    top_bar::TOGGLE_BUTTON_WIDTH
                };
                let has_siblings = this
                    .active_project_panes()
                    .map(|p| p.read(cx).manager().in_order_groups().len() > 1)
                    .unwrap_or(false);
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                this.pane_actions.update(cx, |p, cx| {
                    p.open(
                        PaneActionsAnchor::TopRight {
                            right_px: right_anchor,
                        },
                        has_siblings,
                        cx,
                    )
                });
            }))
            .on_action(
                cx.listener(|this, action: &OpenPaneActionsAt, _window, cx| {
                    // Per-pane "..." click carries the cursor's absolute window
                    // coords. The menu shifts itself left by its own width so
                    // the card stays inside the right edge when chips sit near
                    // it (see pane_actions::PaneActionsAnchor::Chip).
                    let has_siblings = this
                        .active_project_panes()
                        .map(|p| p.read(cx).manager().in_order_groups().len() > 1)
                        .unwrap_or(false);
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.pane_actions.update(cx, |p, cx| {
                        p.open(
                            PaneActionsAnchor::Chip {
                                x_px: action.x,
                                y_px: action.y,
                            },
                            has_siblings,
                            cx,
                        )
                    });
                }),
            )
            // Four-direction split actions. SplitHorizontal / SplitVertical
            // are aliases preserved for the legacy Cmd+D / Cmd+Shift+D
            // bindings; they map to Right / Down respectively.
            .on_action(cx.listener(|this, action: &ActivateGroupTab, window, cx| {
                // Global workspace strip chip click. The chip's own
                // group already set its inner active tab; here we route
                // workspace focus to the chip's group so the body shows
                // its content.
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                let tab_idx = action.tab_idx as usize;
                panes.update(cx, |p, cx| {
                    p.set_active_group(group_id, window, cx);
                    if let Some(group) = p.group(group_id) {
                        group.update(cx, |g, cx| g.set_active(tab_idx, window, cx));
                    }
                });
            }))
            .on_action(
                cx.listener(|this, action: &OpenTabContextMenuAt, _window, cx| {
                    // Tab chip right-click. Carries enough state (group id,
                    // tab index, click coords) for the shared TabContextMenu
                    // to mutate the right group even if focus moves before
                    // the user picks an item.
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                    let panes_ref = panes.read(cx);
                    let Some(group) = panes_ref.group(group_id) else {
                        return;
                    };
                    let group_ref = group.read(cx);
                    let tab_count = group_ref.tabs().len();
                    let tab_idx = action.tab_idx as usize;
                    let is_pinned = group_ref.is_pinned(tab_idx);
                    // Derive kind-specific payload (editor path) so the menu
                    // can render Copy Path / Reveal in Finder rows without
                    // walking back into the entity at click time.
                    let tab_kind = match group_ref.tabs().get(tab_idx).map(|t| &t.kind) {
                        Some(crate::shell::pane_group::PaneGroupTabKind::Editor { path }) => {
                            let project_root = this
                                .active_project
                                .as_ref()
                                .map(|p| std::path::PathBuf::from(&p.root_path));
                            crate::shell::tab_context_menu::TabContextKind::Editor {
                                path: path.clone(),
                                project_root,
                            }
                        }
                        _ => crate::shell::tab_context_menu::TabContextKind::Terminal,
                    };
                    // Tear-off is available only for single-leaf relay-backed
                    // terminal tabs. Multi-leaf split terminals are excluded
                    // (v1 scope: each leaf would need independent detach+mount
                    // in the destination). Editor and diff tabs are excluded
                    // because their content is window-bound.
                    let can_tear_off = group_ref
                        .tabs()
                        .get(tab_idx)
                        .map(|tab| {
                            if let crate::shell::pane_content::PaneContent::Terminal(tree) =
                                &tab.content
                            {
                                // Relay-backed only — the in-process fallback
                                // backend has no external id.
                                let active_has_external_id = tree
                                    .active_view()
                                    .map(|v| v.read(cx).external_id().is_some())
                                    .unwrap_or(false);
                                tab_can_tear_off(tree, active_has_external_id)
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    // For the compact view-options menu (eye button): whether the
                    // agent-chat tab can open its companion terminal — and WHY not,
                    // so the hint distinguishes "no session yet" from "no
                    // interactive terminal for this agent". `None` when not a chat.
                    let chat_terminal_available = group_ref.tabs().get(tab_idx).and_then(|tab| {
                        if let crate::shell::pane_content::PaneContent::AgentChat(v) = &tab.content {
                            Some(v.read(cx).terminal_availability())
                        } else {
                            None
                        }
                    });
                    // Whether the chat tab is currently showing its companion
                    // terminal — flips the view-options row label toward the
                    // opposite destination ("Switch to Chat View" when in terminal).
                    let chat_in_terminal_view = group_ref.tabs().get(tab_idx).is_some_and(|tab| {
                        matches!(&tab.content, crate::shell::pane_content::PaneContent::AgentChat(v)
                            if v.read(cx).view_mode()
                                == crate::shell::agent_chat::ChatViewMode::Terminal)
                    });
                    let view_only = action.view_only;
                    let weak = group.downgrade();
                    let x = action.x;
                    let y = action.y;
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| {
                        m.open(
                            x,
                            y,
                            weak,
                            group_id,
                            tab_idx,
                            tab_count,
                            tab_kind,
                            is_pinned,
                            can_tear_off,
                            view_only,
                            chat_terminal_available,
                            chat_in_terminal_view,
                            cx,
                        )
                    });
                }),
            )
            .on_action(
                cx.listener(|this, action: &OpenTerminalContextMenuAt, _window, cx| {
                    // Terminal GRID right-click. Resolve the exact view (by
                    // session id, so splits target the right pane) + its tab
                    // location, then open the shared menu. Bail silently when
                    // the view vanished between right-click and dispatch.
                    let Some((view, group_id, tab_idx)) =
                        this.resolve_terminal_view_by_session(action.session_id, cx)
                    else {
                        return;
                    };
                    let x = action.x;
                    let y = action.y;
                    let has_selection = action.has_selection;
                    let link = action.link.clone();
                    // Close peer overlays so two context menus never co-exist.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.git_row_context_menu.update(cx, |m, cx| m.close(cx));
                    this.commit_context_menu.update(cx, |m, cx| m.close(cx));
                    this.terminal_context_menu.update(cx, |m, cx| {
                        m.open(x, y, view, group_id, tab_idx, has_selection, link, cx)
                    });
                }),
            )
            .on_action(
                cx.listener(|this, action: &OpenFileTreeContextMenuAt, _window, cx| {
                    // File-tree row right-click — opens the shared
                    // `FileTreeContextMenu` with the clicked path. Directory
                    // rows get a reduced item set (Reveal + Copy Path only).
                    let path = std::path::PathBuf::from(&action.path);
                    let project_root = this
                        .active_project
                        .as_ref()
                        .map(|p| std::path::PathBuf::from(&p.root_path));
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| {
                        m.open(action.x, action.y, path, project_root, action.is_dir, cx)
                    });
                }),
            )
            .on_action(cx.listener(
                |this, action: &crate::actions::OpenFileTreeBackgroundMenuAt, _window, cx| {
                    // Right-click in the empty area below the file tree —
                    // opens the smaller "New File / New Folder" menu rooted
                    // at the workspace root.
                    let root = std::path::PathBuf::from(&action.root);
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu
                        .update(cx, |m, cx| m.open_background(action.x, action.y, root, cx));
                },
            ))
            .on_action(cx.listener(
                |this, action: &OpenGitRowContextMenuAt, _window, cx| {
                    // Git-row right-click — opens the shared
                    // `GitRowContextMenu` with the right scope variant
                    // (Single / Multi / Folder) derived from the
                    // payload. Close peer overlays first so two
                    // context menus can never share screen real
                    // estate.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));

                    // Resolve the GitPanel weak handle + workdir from
                    // the active right-sidebar. Bail silently when the
                    // sidebar is unmounted (right-click can't have
                    // landed on a non-rendered surface, but defensive).
                    let Some(sc) = this
                        .right_sidebar
                        .as_ref()
                        .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
                    else {
                        return;
                    };
                    let (panel, workdir) = {
                        let sc_ref = sc.read(cx);
                        (
                            sc_ref.git_panel.downgrade(),
                            Some(sc_ref.repo.workdir().to_path_buf()),
                        )
                    };

                    let path = std::path::PathBuf::from(&action.path);
                    let target = if action.is_folder {
                        let leaves: Vec<std::path::PathBuf> = action
                            .folder_leaves
                            .iter()
                            .map(std::path::PathBuf::from)
                            .collect();
                        GitRowContextTarget::Folder {
                            leaves,
                            is_staged_section: action.is_staged,
                            is_untracked_section: action.is_untracked,
                        }
                    } else if !action.selection_paths.is_empty() {
                        // Multi-select right-click. Section flags
                        // ride from the right-clicked row — when the
                        // selection spans sections, the right-clicked
                        // row's section wins for dispatch (cleanest
                        // mental model: user right-clicked from
                        // Staged → action targets Staged).
                        let paths: Vec<std::path::PathBuf> = action
                            .selection_paths
                            .iter()
                            .map(std::path::PathBuf::from)
                            .collect();
                        GitRowContextTarget::Multi {
                            paths,
                            all_staged: action.is_staged,
                            all_untracked: action.is_untracked,
                        }
                    } else {
                        GitRowContextTarget::Single {
                            path,
                            is_staged: action.is_staged,
                        }
                    };

                    this.git_row_context_menu.update(cx, |m, cx| {
                        m.open(action.x, action.y, target, panel, workdir, cx);
                    });
                },
            ))
            .on_action(cx.listener(
                |this, action: &OpenCommitContextMenuAt, _window, cx| {
                    // Commit-graph row right-click — opens
                    // `CommitContextMenu` with the right-clicked
                    // commit's full + short OID. Close peer overlays
                    // first so the menu z-band stays single-occupancy.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.git_row_context_menu.update(cx, |m, cx| m.close(cx));

                    // Resolve the active source-control panel's
                    // CommitArea weak handle. Bail silently if the
                    // right sidebar isn't mounted (defensive — the
                    // action can't have been dispatched without a
                    // commit-graph row painted, but the lookup chain
                    // is fail-safe).
                    let Some(commit_area_weak) = this
                        .right_sidebar
                        .as_ref()
                        .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
                        .map(|sc| sc.read(cx).commit_area.downgrade())
                    else {
                        return;
                    };

                    this.commit_context_menu.update(cx, |m, cx| {
                        m.open(
                            action.x,
                            action.y,
                            action.sha.clone(),
                            action.short_sha.clone(),
                            commit_area_weak,
                            cx,
                        );
                    });
                },
            ))
            .on_action(
                cx.listener(|this, action: &crate::actions::FindInFolder, window, cx| {
                    // Switch right sidebar to Search and seed its include
                    // glob with `<rel>/**`. Resolution: prefer relative-to-
                    // workspace-root; fall back to the file name.
                    let target = std::path::PathBuf::from(&action.path);
                    let glob = this
                        .active_project
                        .as_ref()
                        .map(|p| std::path::PathBuf::from(&p.root_path))
                        .and_then(|root| {
                            target
                                .strip_prefix(root.as_path())
                                .ok()
                                .map(|r| r.to_path_buf())
                        })
                        .map(|rel| {
                            let s = rel.to_string_lossy().into_owned();
                            if s.is_empty() {
                                String::from("**")
                            } else {
                                format!("{s}/**")
                            }
                        })
                        .unwrap_or_else(|| String::from("**"));
                    this.seed_search_include_and_switch(glob, window, cx);
                }),
            )
            .on_action(cx.listener(
                |_this, action: &crate::actions::OpenInVSCode, _window, _cx| {
                    // `code <path>` requires the VS Code shell integration
                    // (`Shell Command: Install 'code' command in PATH`).
                    // Errors land in tracing — no UI surface because the
                    // user already left the cockpit by choosing this action.
                    let path = std::path::PathBuf::from(&action.path);
                    if let Err(err) = std::process::Command::new("code").arg(&path).spawn() {
                        tracing::warn!(
                            ?err,
                            path = %path.display(),
                            "open in vs code failed (is `code` on PATH?)"
                        );
                    }
                },
            ))
            .on_action(cx.listener(
                |_this, action: &crate::actions::OpenInFinder, _window, cx| {
                    // `open_with_system` on a directory opens the file manager
                    // AT the target. Distinct from `reveal_path` which opens it
                    // with the path selected in its parent — used for the
                    // workspace-root overflow item.
                    let path = std::path::PathBuf::from(&action.path);
                    cx.open_with_system(&path);
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeNewFile, window, cx| {
                    this.start_inline_create(
                        std::path::PathBuf::from(&action.parent),
                        false,
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeNewFolder, window, cx| {
                    this.start_inline_create(
                        std::path::PathBuf::from(&action.parent),
                        true,
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeRename, window, cx| {
                    this.start_inline_file_rename(
                        std::path::PathBuf::from(&action.path),
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeDelete, window, cx| {
                    this.mount_file_delete_confirm(
                        std::path::PathBuf::from(&action.path),
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeDuplicate, _window, cx| {
                    this.duplicate_file_entry(std::path::PathBuf::from(&action.path), cx);
                },
            ))
            .on_action(
                cx.listener(|this, action: &OpenFileFromContextMenu, window, cx| {
                    // File-tree menu "Open" / "Open to the Side" row. The
                    // menu has already closed itself; this handler routes
                    // to ProjectPanes via the same code paths the drag-drop
                    // flow uses (open_file_in_group / split_and_open_file).
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let path = std::path::PathBuf::from(&action.path);
                    if !is_openable_text_file(&path) {
                        tracing::info!(
                            file = %path.display(),
                            "open-from-context-menu: refusing non-text file"
                        );
                        return;
                    }
                    let split_right = action.split_right;
                    panes.update(cx, |p, cx| {
                        let target = p.manager().active_group_id();
                        if split_right {
                            p.split_and_open_file(
                                target,
                                crate::shell::pane_group::tab_drag_zones::Zone::Right,
                                path,
                                window,
                                cx,
                            );
                        } else {
                            p.open_file_in_group(target, path, window, cx);
                        }
                    });
                }),
            )
            .on_action(cx.listener(
                |this, action: &crate::actions::RequestRenameTabAt, window, cx| {
                    // Tab right-click "Change Title…": open a RenameTabDialog
                    // bound to (group_id, tab_idx). Callback mutates the
                    // target group's custom_title via set_tab_title.
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                    let tab_idx = action.tab_idx as usize;
                    let panes_ref = panes.read(cx);
                    let Some(group) = panes_ref.group(group_id) else {
                        return;
                    };
                    let initial = group.read(cx).visible_title(tab_idx).unwrap_or_default();
                    let weak_root: gpui::WeakEntity<WorkspaceRoot> = cx.weak_entity();
                    let weak_group = group.downgrade();
                    let on_commit: crate::shell::rename_tab_dialog::RenameCallback =
                        std::rc::Rc::new(move |outcome, _window, cx| {
                            use crate::shell::rename_tab_dialog::RenameOutcome;
                            // Mutate first, then drop the dialog regardless of
                            // outcome so Cancel actually dismisses the modal.
                            if let Some(g) = weak_group.upgrade() {
                                match outcome {
                                    RenameOutcome::Save(value) => g.update(cx, |g, cx| {
                                        g.set_tab_title(tab_idx, Some(value), cx)
                                    }),
                                    RenameOutcome::Reset => {
                                        g.update(cx, |g, cx| g.set_tab_title(tab_idx, None, cx))
                                    }
                                    RenameOutcome::Cancel => {}
                                }
                            }
                            let _ = weak_root.update(cx, |this, cx| {
                                this.rename_tab_dialog = None;
                                cx.notify();
                            });
                        });
                    let theme = this.theme;
                    let density = this.density;
                    let typography = this.typography.clone();
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    let dialog = cx.new(|cx| {
                        crate::shell::rename_tab_dialog::RenameTabDialog::new(
                            "Change Tab Title".into(),
                            initial,
                            on_commit,
                            theme,
                            density,
                            typography,
                            window,
                            cx,
                        )
                    });
                    // Focus the dialog's input AFTER it's mounted so the user
                    // can type immediately. Focusing before assignment is a
                    // no-op (the element isn't in the tree yet).
                    dialog.read(cx).input_focus_handle(cx).focus(window, cx);
                    this.rename_tab_dialog = Some(dialog);
                    cx.notify();
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::TogglePinTabAt, _window, cx| {
                    // Tab right-click "Pin Tab" / "Unpin Tab": flip the
                    // pinned flag and re-cluster the chip inside the
                    // group's tab_order. Reading the live `pinned` from
                    // the group at dispatch time keeps the menu's stale
                    // snapshot from producing a wrong toggle (e.g. two
                    // rapid clicks).
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                    let tab_idx = action.tab_idx as usize;
                    let panes_ref = panes.read(cx);
                    let Some(group) = panes_ref.group(group_id) else {
                        return;
                    };
                    let group = group.clone();
                    group.update(cx, |g, cx| g.toggle_pin(tab_idx, cx));
                },
            ))
            .on_action(
                cx.listener(|this, action: &MoveTabToNewWindow, window, cx| {
                    this.handle_move_tab_to_new_window(action.group_id, action.tab_idx, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &SplitHorizontal, window, cx| {
                this.split_active_pane_group(Axis::Horizontal, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitVertical, window, cx| {
                this.split_active_pane_group(Axis::Vertical, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split_active_pane_group(Axis::Horizontal, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split_active_pane_group(Axis::Vertical, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitLeft, window, cx| {
                this.split_active_pane_group(Axis::Horizontal, SplitInsert::Before, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitUp, window, cx| {
                this.split_active_pane_group(Axis::Vertical, SplitInsert::Before, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApplyLayoutStacked, window, cx| {
                use crate::shell::pane_group::layout_presets::Preset;
                this.reshape_active_project_layout(Preset::Stacked, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApplyLayoutHorizontal, window, cx| {
                use crate::shell::pane_group::layout_presets::Preset;
                this.reshape_active_project_layout(Preset::Horizontal, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApplyLayoutBottomTerminal, window, cx| {
                use crate::shell::pane_group::layout_presets::Preset;
                this.reshape_active_project_layout(Preset::BottomTerminal, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseGroup, window, cx| {
                this.close_active_pane_group(window, cx);
            }))
            // Fallback CloseTab handler. The primary listener lives on each
            // `PaneGroup`'s root div, but on the FIRST FRAME after a new tab
            // is created (e.g. opening a file from the explorer) the new
            // view's focus_handle isn't yet in the rendered dispatch tree —
            // `window.focus_node_id_in_rendered_frame` falls back to root,
            // skipping the PaneGroup's listener. Routing Cmd+W through the
            // active group from the root anchor closes the race. When the
            // dispatch tree IS in the right state the PaneGroup listener
            // catches CloseTab first and stops propagation, so this
            // fallback only fires when actually needed (no double-close).
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.on_close_tab(&CloseTab, window, cx));
                    }
                });
            }))
            // Root-level fallback for NewTab. Like CloseTab above, the primary
            // listener lives on the active PaneGroup, which is NOT an ancestor
            // of the focused node when a full-window overlay (command palette)
            // holds focus. Dispatching NewTab from the palette would otherwise
            // reach no handler. Routing through the active group from the root
            // anchor makes the palette "New Tab" entry work; when a pane is
            // focused the PaneGroup listener catches it first and stops
            // propagation, so this fallback only fires when needed.
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.on_new_tab(&NewTab, window, cx));
                    }
                });
            }))
            // Root-level handler for NewBrowserTab (⌘⇧B) — routes to the
            // active group like the NewTab fallback so the keybind works
            // regardless of which surface holds focus.
            .on_action(cx.listener(|this, _: &NewBrowserTab, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.on_new_browser_tab(&NewBrowserTab, window, cx));
                    }
                });
            }))
            // Root-level fallback for Search (scrollback search overlay). The
            // primary listener lives on the focused TerminalView, which is not
            // on the dispatch path when the command palette holds focus. Route
            // to the active group's active terminal so the palette "Search
            // Pane" entry opens the overlay; a focused terminal consumes the
            // action first when no overlay is up.
            .on_action(cx.listener(|this, action: &Search, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.open_search_active_terminal(action, window, cx));
                    }
                });
            }))
            .on_action(cx.listener(|this, action: &SplitGroupAt, window, cx| {
                // Tab right-click "Split X" → target a SPECIFIC group
                // (the right-clicked one), not the focused one. We
                // first activate that group so the split lands on it,
                // then perform the directional split. Matches the
                // per-tab Split menu behavior of the design reference.
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                let axis = if action.axis == 0 {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let insert = if action.insert_before {
                    SplitInsert::Before
                } else {
                    SplitInsert::After
                };
                panes.update(cx, |p, cx| {
                    p.set_active_group(group_id, window, cx);
                    p.split_active_group(axis, insert, window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &DismissOverlay, window, cx| {
                // An in-flight drag takes priority: Escape cancels it (clears
                // the active drag, no drop side-effect) and consumes the key.
                // Only when no drag is active does Escape fall through to
                // overlay dismissal, preserving its existing behaviour.
                if cx.stop_active_drag(window) {
                    return;
                }
                // Close every transient overlay so a single Escape dismisses
                // whichever popover is currently visible. Modal dialogs
                // (project picker / workspace create / palette) own their
                // own focus and currently ignore this.
                //
                // When NOTHING is open, propagate instead of consuming: a
                // matched key binding swallows the keystroke before key
                // listeners ever run, so a no-op here would eat every
                // Escape a focused terminal needs — for its PTY (TUIs, the
                // agent CLI's panels, vim) and for the search overlay.
                let any_open = this.pane_actions.read(cx).is_open()
                    || this.tab_context_menu.read(cx).is_open()
                    || this.file_tree_context_menu.read(cx).is_open()
                    || this.git_row_context_menu.read(cx).is_open()
                    || this.commit_context_menu.read(cx).is_open()
                    || this.terminal_context_menu.read(cx).is_open()
                    || this.adapter_picker.read(cx).is_open()
                    || this.row_menu.read(cx).is_open()
                    || this.project_menu.read(cx).is_open()
                    || this.session_history.read(cx).is_open()
                    || this.usage_popover_open;
                if !any_open {
                    cx.propagate();
                    return;
                }
                this.pane_actions.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                this.git_row_context_menu.update(cx, |m, cx| m.close(cx));
                this.commit_context_menu.update(cx, |m, cx| m.close(cx));
                this.terminal_context_menu.update(cx, |m, cx| m.close(cx));
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.row_menu.update(cx, |m, cx| m.close(cx));
                this.project_menu.update(cx, |m, cx| m.close(cx));
                // The session-history picker is counted in `any_open` above (so
                // Escape is consumed rather than leaking to the terminal even
                // when the modal is open-but-unfocused). It must therefore be
                // closed here too — its own `on_key_down` escape arm never fires
                // once the binding consumes the key.
                this.session_history.update(cx, |m, cx| m.close(cx));
                if this.usage_popover_open {
                    this.usage_popover_open = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleRightSidebar, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.toggle(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectFilesTab, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.select_tab(RightTab::Files, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectExplorerTab, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.select_tab(RightTab::Explorer, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectSearchTab, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.select_tab(RightTab::Search, cx));
                }
            }))
            .on_action(
                cx.listener(|this, _: &SelectSourceControlTab, _window, cx| {
                    if let Some(rs) = &this.right_sidebar {
                        rs.update(cx, |s, cx| s.select_tab(RightTab::SourceControl, cx));
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &SelectHistoryTab, window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| {
                        s.select_tab(RightTab::History, cx);
                        if !s.open {
                            s.toggle(cx);
                        }
                        s.focus_history(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, action: &OpenChatSession, window, cx| {
                this.open_chat_session(action, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenCommitDialog, window, cx| {
                // Cmd+K: jump to Source Control tab and focus the commit input.
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| {
                        s.select_tab(RightTab::SourceControl, cx);
                        if !s.open {
                            s.toggle(cx);
                        }
                        s.focus_commit_subject(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &crate::actions::RefreshSourceControl, _window, cx| {
                // Cmd+R: refresh the commit graph's first page against
                // the active worktree. Same call site as the header-strip
                // refresh button; routes through the workspace so the
                // chord works from any focused pane in the window. Silent
                // no-op when the right sidebar is closed or the SCM tab
                // isn't mounted — the alternative (popping the sidebar
                // open on every Cmd+R) would surprise users who bound the
                // chord to "refresh this window" muscle memory.
                let Some(rs) = &this.right_sidebar else {
                    return;
                };
                let Some(sc) = rs.read(cx).source_control.as_ref().cloned() else {
                    return;
                };
                sc.update(cx, |panel, cx| {
                    panel.commit_graph.update(cx, |g, cx| g.refresh(cx));
                });
            }))
            .child(row)
            .child({
                // Fetch the SCM panel's cached primary action so the status
                // bar renders the same resolved verb — no second resolver.
                let scm_panel = self.right_sidebar.as_ref().and_then(|rs| {
                    rs.read(cx).source_control.clone()
                });
                let primary = scm_panel
                    .as_ref()
                    .and_then(|sc| sc.read(cx).last_primary_action());
                // Clone for the closure; the outer `window` is plumbed via
                // `update` (not `update_in`) per the GPUI memory note.
                let scm_for_click = scm_panel.clone();
                let weak_for_usage = cx.entity().downgrade();
                let weak_for_ports = cx.entity().downgrade();
                #[cfg(any(target_os = "macos", windows))]
                let update_ready = cx
                    .try_global::<crate::updater::UpdaterState>()
                    .and_then(|state| state.ready_version().map(str::to_string));
                #[cfg(not(any(target_os = "macos", windows)))]
                let update_ready: Option<String> = None;
                let settings_for_update = self.settings_modal.downgrade();
                status_bar::view(
                    theme,
                    density,
                    typography,
                    pane_count,
                    tty_count,
                    agent_count,
                    port_count,
                    git_state.as_ref(),
                    primary,
                    self.usage_state.as_ref(),
                    update_ready,
                    move |window, cx| {
                        if let Some(sc) = scm_for_click.clone() {
                            sc.update(cx, |panel, cx| {
                                panel.trigger_primary_action(window, cx);
                            });
                        }
                    },
                    move |window, cx| {
                        WorkspaceRoot::toggle_usage_popover(&weak_for_usage, window, cx);
                    },
                    move |window, cx| {
                        // Everything about the update — notes, the restart
                        // button, the automatic-checks toggle — lives in one
                        // pane. The pill just takes them there.
                        if let Some(modal) = settings_for_update.upgrade() {
                            modal.update(cx, |m, cx| {
                                m.open_to_pane(
                                    crate::shell::settings_modal::SettingsPane::About,
                                    window,
                                    cx,
                                );
                            });
                        }
                    },
                    move |_window, cx| {
                        // Open the sidebar if it is closed, then select Ports:
                        // clicking a metric that says something is serving and
                        // getting no visible change would read as a dead chip.
                        if let Some(root) = weak_for_ports.upgrade() {
                            root.update(cx, |this, cx| {
                                let Some(rs) = this.right_sidebar.clone() else {
                                    return;
                                };
                                rs.update(cx, |sidebar, cx| {
                                    sidebar.open = true;
                                    sidebar.select_tab(
                                        crate::shell::right_sidebar::tab::RightTab::Ports,
                                        cx,
                                    );
                                });
                                cx.notify();
                            });
                        }
                    },
                )
            })
            // Usage-meter popover — anchored above the status bar's right
            // corner. The transparent full-window backdrop closes it on any
            // outside click; z-band above the floating terminal, below the
            // palette overlays that follow.
            .when(self.usage_popover_open, |parent| {
                let Some(state) = self.usage_state.as_ref() else {
                    return parent;
                };
                let weak_close = cx.entity().downgrade();
                let card = crate::shell::usage_meter::render_usage_popover(
                    state,
                    oximux_agents::session_log::now_unix_ms(),
                    theme,
                    density,
                    typography,
                );
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            move |_ev, _window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.usage_popover_open = false;
                                    cx.notify();
                                });
                            },
                        )
                        .child(
                            div()
                                .absolute()
                                .right(px(8.0))
                                .bottom(px(density.h_status_bar + 6.0))
                                // Clicks on the card must not bubble to the
                                // backdrop's dismiss handler — the user may
                                // click while reading the numbers.
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    |_ev, _window, cx| cx.stop_propagation(),
                                )
                                .child(card),
                        ),
                )
            })
            // Floating ("PiP") terminal — sits above the workspace panels but
            // below every popover / modal that follows. Retained across hides
            // (the PTY persists); only rendered while `*_visible` is set.
            .children(
                self.floating_terminal_visible
                    .then(|| self.floating_terminal.clone())
                    .flatten(),
            )
            // "What's New" popover — release notes for the staged update,
            // anchored under the title-bar Update pill. Gated on the updater
            // actually being Ready so a stale open flag (update applied or
            // discarded meanwhile) renders nothing rather than an empty card.
            .when(self.whats_new_open, |parent| {
                #[cfg(any(target_os = "macos", windows))]
                let ready = cx.try_global::<crate::updater::UpdaterState>().and_then(
                    |state| match &state.status {
                        oximux_auto_update::UpdateStatus::Ready { version, notes } => {
                            Some((version.clone(), notes.clone()))
                        }
                        _ => None,
                    },
                );
                #[cfg(not(any(target_os = "macos", windows)))]
                let ready: Option<(String, String)> = None;
                let Some((version, notes)) = ready else {
                    return parent;
                };
                let weak_close = cx.entity().downgrade();
                let card = crate::shell::chrome::whats_new::view(
                    &version,
                    &notes,
                    theme,
                    density,
                    typography,
                );
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_mouse_down(gpui::MouseButton::Left, move |_ev, _window, cx| {
                            let _ = weak_close.update(cx, |this, cx| {
                                this.whats_new_open = false;
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .absolute()
                                .left(px(80.0))
                                .top(px(density.h_top_bar + 6.0))
                                // Clicks on the card must not bubble to the
                                // backdrop's dismiss handler — the user will
                                // scroll and click while reading the notes.
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    |_ev, _window, cx| cx.stop_propagation(),
                                )
                                .child(card),
                        ),
                )
            })
            // Pane Actions dropdown — appended before the palette so the
            // palette (more rare, larger) wins z-order when both are open.
            .child(self.pane_actions.clone())
            // Tab right-click context menu — same z-band as pane_actions.
            // Mutually exclusive with it via close-on-open logic in the
            // OpenPaneActionsAt / OpenTabContextMenuAt action handlers.
            .child(self.tab_context_menu.clone())
            // File-tree right-click context menu — same z-band; mutually
            // exclusive via close-on-open in OpenFileTreeContextMenuAt.
            .child(self.file_tree_context_menu.clone())
            // Git-row right-click context menu — same z-band; mutually
            // exclusive via close-on-open in OpenGitRowContextMenuAt.
            .child(self.git_row_context_menu.clone())
            // Commit-graph row right-click menu — same z-band as the
            // peer context menus; mutually exclusive via close-on-open
            // in OpenCommitContextMenuAt.
            .child(self.commit_context_menu.clone())
            // Terminal grid right-click menu — same z-band as the peer
            // context menus; mutually exclusive via close-on-open in
            // OpenTerminalContextMenuAt.
            .child(self.terminal_context_menu.clone())
            // Adapter picker — same z-band as pane_actions; only one of
            // them can be open at a time so order between them is moot.
            .child(self.adapter_picker.clone())
            // Project picker (Cmd+O). Below the palette so an
            // accidentally-opened palette during picker use wins z-order;
            // the action handlers also close conflicting overlays.
            .child(self.project_picker.clone())
            // Workspace dialog (Cmd+Shift+N create + sidebar rename).
            .child(self.workspace_dialog.clone())
            // Per-row action popover (sidebar Rename / Archive / Delete).
            .child(self.row_menu.clone())
            // Per-project-header action popover (Reveal / Copy / Remove).
            .child(self.project_menu.clone())
            // Agents-page status-filter dropdown.
            .child(self.dashboard_status_menu.clone())
            // Projects-header display-options dropdown.
            .child(self.options_menu.clone())
            .child(self.add_project_dialog.clone())
            // Type-to-confirm dialog for destructive workspace ops. Built
            // per-request; `None` when idle. Wrapped in a full-window
            // overlay here so the inner `ConfirmDialog` card stays pure.
            .when_some(self.confirm_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Rename-tab modal — same overlay pattern as confirm_dialog.
            .when_some(self.rename_tab_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Push-stash form modal — same overlay pattern.
            .when_some(self.push_stash_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Palette modal — appended above the rest of the chrome.
            .child(self.palette.clone())
            // Session-history picker — same z-level as the palette.
            .child(self.session_history.clone())
            // Settings modal — appended last so it paints above all other
            // children (last child = topmost z-layer in GPUI).
            .child(self.settings_modal.clone())
            // Onboarding wizard — above the settings modal: on a fresh boot it
            // must own the window until finished or skipped.
            .child(self.onboarding.clone())
            // Toasts paint above even the modals so a transient event (commit
            // failed, agent done) is never hidden behind an open dialog. The
            // layer is non-interactive and bottom-right, so it doesn't steal
            // clicks from whatever is beneath it.
            .child(self.toast_layer.clone())
            // Voice-dictation HUD: a pass-through, bottom-center floating
            // "Listening…" pill shown while dictating into a terminal/editor
            // pane. Renders nothing when idle; never steals clicks.
            .child(self.dictation_hud.clone())
            // gpui-component notification layer. `Root::render` does not mount
            // it automatically, so leaf views that call `push_notification`
            // (e.g. the editor breadcrumb's copy/reveal actions) need it here
            // or their toasts never paint.
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}
