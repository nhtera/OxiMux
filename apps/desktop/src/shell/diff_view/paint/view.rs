use super::*;

/// Diff-body line count at or below which syntax highlighting runs inline on
/// the render thread. Optimized syntect is ~0.3 ms/line, so this caps the
/// synchronous cost around a tenth of a second — within the "feels instant"
/// window and with no uncolored flash. Larger highlightable diffs paint
/// uncolored immediately and colorize from a background task instead. Tunable.
const SYNC_HIGHLIGHT_THRESHOLD: usize = 250;

// --- DiffView top-level view (moved out of mod.rs in the Tier-1 slim) -----
// `impl Render for DiffView` + its loading/failed placeholder helpers. Lives
// here with the rest of the GPUI element construction; the prepared-row
// helpers it calls (prepare/mark_notes/render_rows/…) are defined above.

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // When no file is selected, collapse to a zero-height placeholder so
        // the source-control panel flows directly from the staged-files list
        // into the commit graph. Earlier builds rendered a full-width
        // "Select a file to view diff" stripe that wasted vertical space.
        if matches!(self.state, DiffViewState::Empty) {
            return div()
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(Self::on_expand_diff))
                .on_action(cx.listener(Self::on_retry_diff))
                .into_any_element();
        }

        // Editor-global font zoom, resolved once. `zoom` drives the scaled
        // typography; `zoom_factor` (zoomed code-font px over base) scales the
        // body row height. Both feed the `list()` height-cache sync below.
        let zoom = current_zoom(cx);
        let zoom_factor = body_zoom_factor(self.typography.t_body_sm, zoom);

        // Rebuild the cached row list only when stale. This keeps the
        // expensive work — walking every hunk, syntect highlighting each
        // line, word-diff pairing — OFF the per-frame render path; it runs
        // once per (diff, expanded) change, not on every scroll tick.
        // Done before borrowing `&self.state` for the body so the heavy
        // plan build doesn't tangle with the element tree below.
        if self.prepared.is_none() {
            // (1) Rebuild the expensive plan ONLY when it's genuinely stale —
            //     a fresh diff or a large-file expand (`invalidate_plan`).
            //     Fold / collapse-all / split / note toggles leave it intact
            //     (`invalidate_prepared`), so they skip syntect entirely and
            //     drop straight to the cheap flatten in (2). This is what kept
            //     Collapse/Expand-all from re-highlighting every line.
            if self.plan_cache.is_none() {
                // Small diffs highlight inline (fast, no uncolored flash). Larger
                // highlightable diffs build UNCOLORED here so the body paints
                // instantly, then a background pass (spawned below) rebuilds the
                // colored plan off the UI thread and swaps it in — syntect on a
                // near-budget diff is ~1 s of work that must never block paint.
                let mut spawn_async: Option<(Vec<oximux_core::FileDiff>, bool)> = None;
                let built = match &self.state {
                    DiffViewState::Ready {
                        diffs, expanded, ..
                    }
                    | DiffViewState::CommitReady {
                        diffs, expanded, ..
                    }
                    | DiffViewState::RangeReady {
                        diffs, expanded, ..
                    }
                    | DiffViewState::CombinedReady {
                        diffs, expanded, ..
                    } => {
                        let lines = diff_body_line_count(diffs);
                        let highlightable = lines <= SYNTAX_HIGHLIGHT_BUDGET_LINES;
                        let sync_highlight = highlightable && lines <= SYNC_HIGHLIGHT_THRESHOLD;
                        let plan = build_render_plan(diffs, *expanded, sync_highlight);
                        // Stageable change regions per file — full-file context
                        // makes one giant hunk, so the renderer docks a chip bar
                        // per region (git add -p granularity) using these.
                        let regions: Vec<Vec<oximux_core::ChangeRegion>> =
                            diffs.iter().map(oximux_core::change_regions).collect();
                        // Hollow-vs-solid gutter slivers: only the combined view
                        // carries per-file group tags, so staged files render
                        // hollow there. Single-file / commit / range views pass
                        // an empty slice → every sliver solid (one state).
                        let staged_per_file: Vec<bool> = match &self.state {
                            DiffViewState::CombinedReady { groups, .. } => groups
                                .iter()
                                .map(|g| matches!(g, FileGroup::Staged))
                                .collect(),
                            _ => Vec::new(),
                        };
                        let paths: Vec<String> =
                            diffs.iter().map(|d| d.path.display().to_string()).collect();
                        // Above the inline threshold but within budget → colorize
                        // off-thread; clone the inputs the background pass needs.
                        if highlightable && !sync_highlight {
                            spawn_async = Some((diffs.clone(), *expanded));
                        }
                        Some(Rc::new(PlanCache {
                            plan,
                            regions,
                            staged_per_file,
                            paths,
                        }))
                    }
                    _ => None,
                };
                self.plan_cache = built;
                // Background syntax-highlight pass for large diffs. Runs the
                // syntect work on a thread, then swaps the colored plan in on
                // the foreground — guarded by `plan_gen` so a newer diff load
                // discards a stale result.
                if let Some((diffs, expanded)) = spawn_async {
                    let gen_at_spawn = self.plan_gen;
                    let executor = cx.background_executor().clone();
                    self._highlight_task = Some(cx.spawn(async move |this, cx| {
                        let colored = executor
                            .spawn(async move { build_render_plan(&diffs, expanded, true) })
                            .await;
                        let _ = this.update(cx, |view, cx| {
                            if view.plan_gen != gen_at_spawn {
                                return; // superseded by a newer load/expand
                            }
                            if let Some(pc) = view.plan_cache.as_ref() {
                                // Only the per-line tokens changed; reuse the
                                // collapse-invariant regions/staged/paths.
                                view.plan_cache = Some(Rc::new(PlanCache {
                                    plan: colored,
                                    regions: pc.regions.clone(),
                                    staged_per_file: pc.staged_per_file.clone(),
                                    paths: pc.paths.clone(),
                                }));
                                // Re-flatten with colors. Same row count → the
                                // list `remeasure`s, keeping the scroll position.
                                view.invalidate_prepared();
                                cx.notify();
                            }
                        });
                    }));
                }
            }
            // (2) Flatten the cached plan into rows. Cheap — no string or
            //     highlight work; just walks the plan applying the current
            //     fold / collapse / split state, then bakes note markers.
            let rows = self.plan_cache.clone().map(|pc| {
                let rctx = RenderCtx {
                    theme: self.theme,
                    density: self.density,
                    typography: &self.typography,
                };
                let mut body = if self.split {
                    prepare_split(
                        &pc.plan,
                        &pc.regions,
                        &self.collapsed,
                        &self.expanded_folds,
                        &pc.staged_per_file,
                        &self.images,
                        &rctx,
                    )
                } else {
                    prepare(
                        &pc.plan,
                        &pc.regions,
                        &self.collapsed,
                        &self.expanded_folds,
                        &pc.staged_per_file,
                        &self.images,
                        &rctx,
                    )
                };
                // Bake each line's note marker once per rebuild (off the
                // per-frame path), parallel to the staged-sliver pass.
                mark_notes(&mut body, &pc.paths, &self.notes);
                Rc::new(body)
            });
            self.prepared = rows;
            // Sync the list's per-item height cache to the rebuilt rows. Same
            // row count → identity is stable (a note marker toggled, a hover
            // re-render) so `remeasure` keeps the scroll position. A different
            // count is a structural change → `reset` rebuilds the cache, which
            // drops the scroll position. For an in-place structural edit (a
            // fold expand/collapse: the list was already populated last frame,
            // and the change lands at or below the visible top) restore the
            // prior scroll so the reader stays put. A fresh diff passed through
            // an empty Loading frame, so it starts at the top.
            let new_len = self.prepared.as_ref().map(|r| r.len()).unwrap_or(0);
            if self.body_list.item_count() == new_len {
                self.body_list.remeasure();
            } else {
                let prev_scroll = self.body_list.logical_scroll_top();
                let was_populated = self.body_list_was_populated;
                self.body_list.reset(new_len);
                if was_populated && new_len > 0 {
                    self.body_list.scroll_to(gpui::ListOffset {
                        item_ix: prev_scroll.item_ix.min(new_len - 1),
                        offset_in_item: prev_scroll.offset_in_item,
                    });
                }
            }
            self.body_list_was_populated = new_len > 0;
            self.body_list_zoom = zoom_factor;
            // Recompute the horizontal-scroll measurement row + its char
            // count alongside the rebuild (off the per-frame path).
            let (widest, chars) = self
                .prepared
                .as_ref()
                .map(|r| (widest_row_index(r), widest_row_chars(r)))
                .unwrap_or((0, 0));
            self.prepared_widest = widest;
            self.prepared_widest_chars = chars;
            // Overview-ruler runs come from the same rebuild so the ruler
            // paints off a cached list, never rescanning rows per frame.
            self.overview = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(overview_runs(r)))
                .unwrap_or_default();
            // Row→file map + per-file headers for the sticky overlay, also
            // off the per-frame path.
            self.row_owner = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(build_row_owner(r)))
                .unwrap_or_default();
            self.headers = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(collect_headers(r)))
                .unwrap_or_default();
            // First-row-of-file index for the file rail's click-to-jump.
            self.first_row_of_file = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(build_first_row_of_file(r)))
                .unwrap_or_default();
            // A hunk op left a position to restore — apply it once the
            // rebuilt rows actually exist (NOT during the intermediate
            // Loading frame, where `prepared` is empty and the anchor would
            // be consumed before the reload lands), so staging doesn't snap
            // the reader to the top.
            let len = self.prepared.as_ref().map(|r| r.len()).unwrap_or(0);
            if len > 0
                && let Some(anchor) = self.pending_scroll_anchor.take()
            {
                self.body_list.scroll_to(ListOffset {
                    item_ix: anchor.min(len - 1),
                    offset_in_item: px(0.0),
                });
            }
        }

        let rctx = RenderCtx {
            theme: self.theme,
            density: self.density,
            typography: &self.typography,
        };
        // Body-only zoom: the code rows, file headers, sticky overlay, and
        // staging card share the editor's Cmd+/- font size. Scale a body-scoped
        // density (row height) + typography (all sizes) by the same factor so
        // larger code reads at the right size and every body pixel computation
        // stays aligned. The toolbar + rail keep base metrics.
        //
        // A zoom change with no rebuild leaves the list's cached row heights
        // stale (the rows are identical, only their height moved), so remeasure
        // once when the factor shifts — preserving the proportional scroll
        // position. A rebuild above already synced the cache at this zoom.
        if (zoom_factor - self.body_list_zoom).abs() > f32::EPSILON {
            self.body_list.remeasure();
            self.body_list_zoom = zoom_factor;
        }
        let mut body_density = self.density;
        body_density.h_row *= zoom_factor;
        let body_typography = scaled_typography(&self.typography, zoom);
        let body_rctx = RenderCtx {
            theme: self.theme,
            density: body_density,
            typography: &body_typography,
        };
        let hovered_region = self.hovered_region;
        let split = self.split;
        let split_h_offset = self.split_h_offset;
        // Weak handle routes chip / expand / copy clicks back into the
        // view from the virtualized list's App-scope render closure (which
        // doesn't carry a `Context<DiffView>`).
        let weak = cx.entity().downgrade();
        let body = match &self.state {
            DiffViewState::Empty => unreachable!("handled above"),
            DiffViewState::Loading { path, .. } => {
                loading_state(&path.display().to_string(), &rctx).into_any_element()
            }
            DiffViewState::Failed { path, error, .. } => {
                failed_state(&path.display().to_string(), error, &rctx, cx).into_any_element()
            }
            DiffViewState::Ready { .. } => {
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    split,
                    split_h_offset,
                    self.body_list.clone(),
                    &body_rctx,
                    weak,
                    self.recently_copied_file,
                )
                .into_any_element()
            }
            DiffViewState::CommitLoading {
                short_oid, subject, ..
            } => loading_state(
                &format!("Loading commit {short_oid}: {subject}…"),
                &rctx,
            )
            .into_any_element(),
            DiffViewState::CommitFailed {
                short_oid, error, ..
            } => failed_state(
                &format!("commit {short_oid}"),
                error,
                &rctx,
                cx,
            )
            .into_any_element(),
            DiffViewState::CommitReady { .. } => {
                // Read-only: the staging-card overlay is gated on `side`
                // (None here) so Stage/Unstage/Discard never appears on
                // commit-detail rows — nothing git supports against history.
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    split,
                    split_h_offset,
                    self.body_list.clone(),
                    &body_rctx,
                    weak,
                    self.recently_copied_file,
                )
                .into_any_element()
            }
            DiffViewState::RangeLoading { title, .. } => {
                loading_state(&format!("Loading {title}…"), &rctx).into_any_element()
            }
            DiffViewState::RangeFailed { title, error, .. } => {
                failed_state(title, error, &rctx, cx).into_any_element()
            }
            DiffViewState::RangeReady { .. } => {
                // Read-only, like commit detail: `side = None` gates out the
                // staging card overlay.
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    split,
                    split_h_offset,
                    self.body_list.clone(),
                    &body_rctx,
                    weak,
                    self.recently_copied_file,
                )
                .into_any_element()
            }
            DiffViewState::CombinedLoading { scope } => {
                loading_state(scope.title(), &rctx).into_any_element()
            }
            DiffViewState::CombinedFailed { scope, error } => {
                failed_state(scope.title(), error, &rctx, cx).into_any_element()
            }
            DiffViewState::CombinedReady { .. } => {
                // Stageable per-file-group: the card overlay resolves
                // `side_for_region` per the hovered file's tag (Unstaged →
                // Stage, Staged → Unstage, Committed → none).
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    split,
                    split_h_offset,
                    self.body_list.clone(),
                    &body_rctx,
                    weak,
                    self.recently_copied_file,
                )
                .into_any_element()
            }
        };
        // The body owns its own scrolling now: `Ready`/`CommitReady` return
        // a `uniform_list` (flex_1 + min_h(0)) that virtualizes rows and
        // reports an exact content height, so scrolling reaches the true
        // end of the diff. Non-Ready states return a centered placeholder.
        let scroll_body = body;
        // A one-row toolbar above the body carries the inline ↔ side-by-side
        // toggle (only when a diff is actually shown). The button label flips
        // to name the OTHER mode, like the reference editors.
        let has_body = matches!(
            self.state,
            DiffViewState::Ready { .. }
                | DiffViewState::CommitReady { .. }
                | DiffViewState::RangeReady { .. }
                | DiffViewState::CombinedReady { .. }
        );
        // The file rail is a multi-file affordance only — a single-file diff
        // has nothing to navigate, so the toggle and rail never appear.
        let multi_file = has_body && self.headers.len() > 1;
        let rail_open = self.rail_open;
        let toolbar = has_body.then(|| {
            let label = if split { "Inline" } else { "Side by side" };
            let hover_bg = self.theme.hover_overlay;
            let all_folded = self.all_folded();
            let fold_label = if all_folded { "Expand all" } else { "Collapse all" };
            // Review-notes cluster — a count label + Send / Copy / Clear,
            // shown only once the current scope carries at least one note.
            // Send pipes the markdown prompt to the active agent; Copy is the
            // clipboard escape hatch; Clear drops the scope's notes.
            let notes_cluster = (!self.notes.is_empty()).then(|| {
                let n = self.notes.len();
                let t_sm = self.typography.t_body_sm;
                let r_chip = self.density.r_chip;
                let fg_muted = self.theme.fg_muted;
                let action_chip = move |id: &'static str, text: &'static str| {
                    div()
                        .id(id)
                        .px(px(8.0))
                        .py(px(1.0))
                        .rounded(px(r_chip))
                        .text_size(px(t_sm))
                        .text_color(fg_muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .child(text)
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(t_sm))
                            .text_color(self.theme.fg_subtle)
                            .child(format!("Notes ({n})")),
                    )
                    .child(action_chip("diff-notes-send", "Send").on_click(cx.listener(
                        |view, _: &gpui::ClickEvent, window, cx| {
                            view.send_notes(window, cx);
                        },
                    )))
                    .child(action_chip("diff-notes-copy", "Copy").on_click(cx.listener(
                        |view, _: &gpui::ClickEvent, _window, cx| {
                            view.copy_notes(cx);
                        },
                    )))
                    .child(action_chip("diff-notes-clear", "Clear").on_click(cx.listener(
                        |view, _: &gpui::ClickEvent, _window, cx| {
                            view.clear_notes(cx);
                        },
                    )))
            });
            // Leading rail toggle (multi-file only), pushed apart from the
            // view controls by a flex spacer so it sits at the left edge.
            let rail_toggle = multi_file.then(|| {
                // Hover deliberately matches the persistent lit-state fill
                // (NOT `hover_overlay`): while the rail is open the button
                // must read "already pressed" under the pointer instead of
                // flickering between two different fills on enter/leave.
                let active_bg = self.theme.bg_panel_alt;
                let mut btn = div()
                    .id("diff-rail-toggle")
                    .flex()
                    .items_center()
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded(px(self.density.r_chip))
                    .cursor_pointer()
                    .hover(move |s| s.bg(active_bg))
                    .on_click(cx.listener(|view, _: &gpui::ClickEvent, _window, cx| {
                        view.toggle_rail(cx);
                    }))
                    .child(
                        Icon::default()
                            .path("icons/list-tree.svg")
                            .xsmall()
                            .text_color(if rail_open {
                                self.theme.fg_base
                            } else {
                                self.theme.fg_subtle
                            }),
                    );
                if rail_open {
                    btn = btn.bg(active_bg);
                }
                btn
            });
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(self.density.gap_inline))
                .flex_shrink_0()
                .h(px(self.density.h_row))
                .w_full()
                .px(px(self.density.pad_panel))
                .bg(self.theme.bg_panel)
                .border_b_1()
                .border_color(self.theme.border_inactive)
                .children(rail_toggle)
                // Changed-file count, next to the rail toggle (multi-file only).
                .children(multi_file.then(|| {
                    let n = self.headers.len();
                    div()
                        .text_size(px(self.typography.t_body_sm))
                        .text_color(self.theme.fg_muted)
                        .child(format!("{n} changed files"))
                }))
                // Spacer pushes the view controls to the right edge while the
                // rail toggle stays pinned left.
                .child(div().flex_1())
                .children(notes_cluster)
                .child(
                    // Collapse-all / Expand-all — folds every file to its
                    // header so a many-file diff reads as a tight index.
                    div()
                        .id("diff-collapse-all")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .px(px(8.0))
                        .py(px(1.0))
                        .rounded(px(self.density.r_chip))
                        .text_size(px(self.typography.t_body_sm))
                        .text_color(self.theme.fg_muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_click(cx.listener(move |view, _: &gpui::ClickEvent, _window, cx| {
                            view.set_all_folded(!all_folded, cx);
                        }))
                        .child(
                            Icon::default()
                                .path("icons/list-collapse.svg")
                                .xsmall()
                                .text_color(self.theme.fg_subtle),
                        )
                        .child(fold_label),
                )
                .child(
                    div()
                        .id("diff-split-toggle")
                        .px(px(8.0))
                        .py(px(1.0))
                        .rounded(px(self.density.r_chip))
                        .text_size(px(self.typography.t_body_sm))
                        .text_color(self.theme.fg_muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_click(cx.listener(|view, _: &gpui::ClickEvent, _window, cx| {
                            view.toggle_split(cx);
                        }))
                        .child(label),
                )
        });
        // The opt-in rail, built only when revealed for a multi-file diff.
        // Active file = the one owning the row at the top of the viewport
        // (same exact pixel-offset math the sticky header uses). Group tags
        // (section headers) come from the combined view; commit / range
        // diffs render a flat list.
        // Lazily build the rail's filter input the first time the rail is
        // shown (it needs a `Window`, unavailable in `new`). A change event
        // re-renders so the filtered row set updates as the user types.
        if multi_file && rail_open && self.rail_filter.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Filter files…"));
            self._rail_filter_sub = Some(cx.subscribe(
                &input,
                |_view, _input, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
            self.rail_filter = Some(input);
        }
        let rail = (multi_file && rail_open).then(|| {
            let active_file_idx = self
                .row_owner
                .get(self.first_visible_row(cx))
                .copied()
                .unwrap_or(0);
            // Borrow the group tags directly from state — no per-frame clone.
            let rail_groups: Option<&[FileGroup]> = match &self.state {
                DiffViewState::CombinedReady { groups, .. } => Some(groups.as_slice()),
                _ => None,
            };
            let filter = self
                .rail_filter
                .as_ref()
                .map(|i| i.read(cx).value().to_lowercase())
                .unwrap_or_default();
            let weak_rail = cx.entity().downgrade();
            let rows = file_rail(
                RailContext {
                    headers: &self.headers,
                    groups: rail_groups,
                    active_file_idx,
                    collapsed_dirs: &self.rail_collapsed_dirs,
                    filter: &filter,
                    theme: self.theme,
                    density: self.density,
                },
                &self.typography,
                weak_rail,
            );
            // Rail = fixed-width column: filter box pinned on top, file tree
            // scrolling beneath it.
            let mut col = div()
                .flex_shrink_0()
                .w(px(RAIL_WIDTH))
                .h_full()
                .flex()
                .flex_col()
                .bg(self.theme.bg_panel)
                .border_r_1()
                .border_color(self.theme.border_inactive);
            if let Some(input) = self.rail_filter.as_ref() {
                col = col.child(
                    div()
                        .flex_shrink_0()
                        .w_full()
                        .p(px(6.0))
                        .border_b_1()
                        .border_color(self.theme.border_inactive)
                        .child(Input::new(input).small()),
                );
            }
            col.child(
                div()
                    .id("diff-rail-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(rows),
            )
        });
        // The body lives in a `flex_1 + min_h(0)` wrapper so the toolbar can
        // sit above it while the `uniform_list` inside still gets a DEFINITE
        // height (`h_full` of this wrapper) — preserving scroll-to-end.
        // Clearing hover on body-leave: the body rows ignore hover-leave (so
        // the pointer can travel onto the floating staging card), which means
        // nothing disarms `hovered_region` when the pointer exits the diff
        // entirely. This wrapper-level leave catches that, hiding a stranded
        // card. Re-entering a changed line (or the card) re-arms it.
        let weak_leave = cx.entity().downgrade();
        let mut body_wrap = div()
            .id("diff-body-wrap")
            // `relative` makes this the positioning context for the overview
            // ruler stacked on its right edge (added below).
            .relative()
            // `flex_1` (grow 1, basis 0) fills the width the rail leaves in
            // the content row; height stretches via the row's cross axis. No
            // `w_full` here — on the horizontal main axis it would fight the
            // rail's fixed width.
            .flex_1()
            // `min_w(0)` is load-bearing: a flex item defaults to
            // `min-width: auto`, so the very wide diff body (long lines in the
            // combined view) would stretch this wrapper past the viewport and
            // push the right-edge scrollbar + overview ruler off-screen. Pinning
            // the min width to 0 keeps the wrapper at the available width and
            // lets the inner `overflow_x_scroll` clip + scroll instead.
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .on_hover(move |hovered, _window, cx: &mut App| {
                if !*hovered {
                    let _ = weak_leave.update(cx, |view, cx| view.set_hover(None, None, cx));
                }
            })
            .child(scroll_body);
        // In split mode, a horizontal-intent wheel (trackpad swipe or
        // Shift+wheel) scrolls BOTH columns' content in sync. We never stop
        // propagation, so the list still handles vertical scroll from the
        // same event; we only act on the horizontal component.
        if split {
            // Zoomed char width so the horizontal-scroll clamp + line-delta
            // conversion track the body's actual (zoomed) glyph advance.
            let char_w = body_typography.t_body_sm * 0.6;
            let max_off = (self.prepared_widest_chars as f32 * char_w - 80.0).max(0.0);
            body_wrap = body_wrap.on_scroll_wheel(cx.listener(
                move |view, ev: &gpui::ScrollWheelEvent, _window, cx| {
                    if !view.split {
                        return;
                    }
                    let (dx, dy) = match ev.delta {
                        gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                        gpui::ScrollDelta::Lines(l) => (l.x * char_w, l.y * char_w),
                    };
                    // Shift makes a vertical wheel scroll horizontally; else
                    // honor a horizontal-dominant trackpad delta.
                    let amount = if ev.modifiers.shift {
                        dy
                    } else if dx.abs() > dy.abs() {
                        dx
                    } else {
                        0.0
                    };
                    if amount != 0.0 {
                        let next = (view.split_h_offset - amount).clamp(0.0, max_off);
                        if next != view.split_h_offset {
                            view.split_h_offset = next;
                            cx.notify();
                        }
                    }
                },
            ));
        }
        // Sticky file header: a pinned twin of the first-visible file's
        // header so its name + stats stay on screen while the body scrolls
        // under it. Stacked above the rows but below the ruler. Gains a
        // shadow only when a real header has scrolled past the top.
        if has_body && !self.headers.is_empty() {
            let fv = self.first_visible_row(cx);
            let file_idx = self.row_owner.get(fv).copied().unwrap_or(0);
            // Only pin the overlay once a real header has scrolled ABOVE the
            // viewport (`stuck`). While the file's own header is still flush at
            // the top it's already on screen and clickable — stacking an
            // identical interactive twin over it makes a click fire
            // `toggle_file_fold` twice (overlay + real row), toggling fold
            // on then off, which read as "the first file won't expand".
            let stuck = !matches!(
                self.prepared.as_ref().and_then(|r| r.get(fv)),
                Some(PreparedRow::FileHeader { .. })
            );
            if stuck
                && let Some(header) = self.headers.iter().find(|h| h.file_idx == file_idx)
            {
                let weak_sticky = cx.entity().downgrade();
                body_wrap = body_wrap.child(sticky_header_overlay(
                    header,
                    true,
                    self.recently_copied_file == Some(file_idx),
                    self.theme,
                    body_density,
                    &body_typography,
                    weak_sticky,
                ));
            }
        }
        // The change-map (overview ruler) + draggable scrollbar are NOT
        // overlaid on the body — they live in a reserved right gutter built
        // below, so they never sit on top of the diff text or the header's
        // copy/open icons (the way VS Code / the reference editors reserve a
        // dedicated scrollbar lane).
        //
        // Floating Stage/Discard card for the hovered region — pinned to the
        // viewport's right edge at the anchor row's on-screen Y (so it's
        // always visible regardless of the changed line's length or the
        // horizontal scroll). Gated on the hovered region's per-file side:
        // single-file `Ready` and combined `Unstaged`/`Staged` files stage;
        // commit/range views and `Committed`/`Untracked`-resolved sides that
        // return `None` show no card.
        if has_body
            && let Some(region) = hovered_region
            && let Some(row) = self.hovered_row
            && let Some(side) = self.side_for_region(region.0)
        {
            // The hovered row's on-screen Y within the body viewport. `list()`
            // measures each item, so there's no `row * h_row` shortcut — read
            // the row's painted window bounds and subtract the viewport's top
            // (the body wrapper shares the list's top edge). `None` (row not
            // yet measured) clamps to the top defensively; the hovered row is
            // by definition on screen.
            let top = self
                .body_list
                .bounds_for_item(row)
                .map(|b| f32::from(b.origin.y - self.body_list.viewport_bounds().origin.y).max(0.0))
                .unwrap_or(0.0);
            let weak_card = cx.entity().downgrade();
            if let Some(card) = staging_card_overlay(
                top,
                region.0,
                region.1,
                row,
                side,
                self.theme,
                body_density,
                &body_typography,
                weak_card,
            ) {
                body_wrap = body_wrap.child(card);
            }
        }
        // Reserved right gutter: a fixed-width lane holding the change-map
        // (overview ruler) + the draggable scrollbar, so neither overlaps the
        // diff text or the header's copy/open icons. `body_wrap` (the scrolling
        // content) yields this width via the `flex_row` below — the same
        // dedicated-lane layout VS Code / the reference editors use.
        const BODY_GUTTER_W: f32 = 14.0;
        let gutter = if has_body {
            let mut g = div()
                .relative()
                .h_full()
                .flex_shrink_0()
                .w(px(BODY_GUTTER_W));
            if !self.overview.is_empty() {
                let total_rows = self.prepared.as_ref().map(|r| r.len()).unwrap_or(0);
                let weak_ruler = cx.entity().downgrade();
                g = g.child(overview_ruler(
                    &self.overview,
                    total_rows,
                    &self.theme,
                    weak_ruler,
                ));
            }
            // `list()` owns its scroll internally and paints no native
            // scrollbar, so bind gpui-component's overlay scrollbar to the same
            // `ListState`. `Always` keeps the thumb visible at rest.
            g = g.child(
                gpui_component::scroll::Scrollbar::vertical(&self.body_list)
                    .scrollbar_show(gpui_component::scroll::ScrollbarShow::Always),
            );
            Some(g)
        } else {
            None
        };
        // Compose the scrolling content + the reserved gutter side by side.
        // `body_wrap` keeps its own `flex_1`/`min_w(0)`, so it fills the width
        // left after the 14px gutter; the gutter never moves with h-scroll.
        let mut body_outer = div()
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(body_wrap);
        if let Some(g) = gutter {
            body_outer = body_outer.child(g);
        }
        // When the discard-hunk confirm modal is mounted, stack it as a
        // centered overlay over the diff body. Mirrors `confirm_dialog`
        // and `rename_tab_dialog` overlay shape in `workspace_root` but
        // scoped to THIS diff tab (other open diff tabs each carry their
        // own slot) so a destructive action in one tab doesn't backdrop
        // the rest of the workspace.
        let overlay = self.confirm_dialog.clone().map(|dialog| {
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.0))
                .child(dialog)
        });
        // Review-note compose/edit popover — same centered-overlay shape as
        // the confirm dialog, scoped to this diff tab.
        let note_overlay = self.note_popover.clone().map(|popover| {
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.0))
                .child(popover)
        });
        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_expand_diff))
            .on_action(cx.listener(Self::on_retry_diff))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_zoom_reset))
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_base)
            .border_l_1()
            .border_color(self.theme.border_inactive);
        if let Some(tb) = toolbar {
            root = root.child(tb);
        }
        // Below the toolbar: an opt-in left rail beside the body. The content
        // row owns the remaining height; the body keeps `flex_1` so it fills
        // whatever the rail leaves. Without a rail it's a thin pass-through.
        let mut content = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .w_full();
        if let Some(rail) = rail {
            content = content.child(rail);
        }
        content = content.child(body_outer);
        root = root.child(content);
        if let Some(o) = overlay {
            root = root.child(o);
        }
        if let Some(o) = note_overlay {
            root = root.child(o);
        }
        root.into_any_element()
    }
}

fn loading_state(path: &str, rctx: &RenderCtx<'_>) -> impl IntoElement {
    // Minimal centered text — no icon, no animation. Matches the muted
    // single-line placeholder the rest of the loading surfaces use.
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .w_full()
        .p(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(format!("Loading diff for {path}…"))
}

fn failed_state(
    path: &str,
    error: &str,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<DiffView>,
) -> impl IntoElement {
    let pad = px(rctx.density.pad_panel);
    let text_size = px(rctx.typography.t_body_sm);
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .h_full()
        .w_full()
        .p(pad)
        .text_size(text_size)
        .text_color(rctx.theme.fg_subtle)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .text_color(rctx.theme.status_error)
                .child(
                    Icon::default()
                        .path("icons/alert-triangle.svg")
                        .xsmall()
                        .text_color(rctx.theme.status_error),
                )
                .child(format!("Failed to load {path}")),
        )
        .child(div().child(error.to_string()))
        .child(
            // Retry button — clickable row that dispatches `RetryDiff`.
            // The action handler reads path/staged/untracked off the
            // current `Failed` state and re-runs the load with the same
            // routing the original call used.
            div()
                .id("diff-view-retry")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(12.0))
                .py(px(6.0))
                .text_color(rctx.theme.status_info)
                .border_1()
                .border_color(rctx.theme.border_inactive)
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(rctx.theme.hover_overlay))
                .on_click(cx.listener(|view, _: &gpui::ClickEvent, _, cx| {
                    view.retry(cx);
                    cx.notify();
                }))
                .child(
                    Icon::default()
                        .path("icons/refresh-cw.svg")
                        .xsmall()
                        .text_color(rctx.theme.status_info),
                )
                .child("Retry"),
        )
}

