use super::*;

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Adopt the grid size the canvas measured from its real bounds
        // last paint, then apply it. `maybe_resize` resizes the PTY +
        // refetches the snapshot only when the size actually changed.
        input_trace("frame");
        self.pull_canvas_grid();
        self.maybe_resize();

        // Post-input frame persistence: while a recent keystroke's window is
        // open, keep requesting frames so the per-frame drain below catches a
        // straggler echo within one frame, even if the run loop would otherwise
        // sleep until the next wake. Counts down to zero so an idle terminal
        // stops repainting.
        if self.drain_frames > 0 {
            self.drain_frames -= 1;
            cx.notify();
        }

        // Drive the PTY drain from the frame loop, not only the background poll
        // timer. The keystroke that scheduled this frame echoed back by now, and
        // frames render promptly (vsync) where the background-executor timer is
        // throttled/coalesced when the run loop is idle between keystrokes —
        // which left echoes sitting undrained for 100ms–1s. Draining here pulls
        // the echo into this very frame (~one frame of latency). `tick` re-arms
        // the next frame itself (`cx.notify` while output flows), so a live TUI
        // keeps painting and the chain settles to idle once output stops. The
        // background poll stays as the drain path for hidden tabs (not rendered).
        self.tick(cx);

        // Cursor visibility rules:
        //   - Focused + blink on  → solid inverse block (active cursor)
        //   - Focused + blink off → hidden (mid-blink phase)
        //   - Unfocused           → ghost cursor: inverted but bg dimmed to
        //                           `UNFOCUSED_CURSOR_ALPHA` so the user can
        //                           still see where the shell's caret sits
        //                           without it competing with the focused pane.
        // Out-of-grid sentinels keep `build_row`'s cursor-col check fall-
        // through silently in the hidden case — no `if` gating in the hot
        // path. Pane-focus also drives the inactive FG dim
        // (`terminal_row::UNFOCUSED_FG_ALPHA`).
        let pane_focused = self.focus_handle.is_focused(window);
        let cursor_visible = !pane_focused || self.cursor_visible;
        let cursor = if cursor_visible {
            (
                self.snapshot.cursor.0 as usize,
                self.snapshot.cursor.1 as usize,
            )
        } else {
            (usize::MAX, usize::MAX)
        };
        let theme = self.theme;
        let pad = self.density.pad_panel;
        let focus_handle = self.focus_handle.clone();

        // Match buckets per visible row. History_len was captured at scan
        // time in `SearchState::rerun` — if PTY output has scrolled history
        // since the scan, highlights may drift one paint but self-correct
        // on the next keystroke. `display_offset` shifts the visible window
        // up into history while scrolled, so highlights track the rows the
        // user is actually looking at rather than the live tail.
        let visible_rows = self.snapshot.cells.len();
        let buckets = self
            .search
            .render_buckets(visible_rows, self.snapshot.display_offset);

        // Live render-time knobs from settings (alpha multipliers).
        let s = terminal_settings(cx);
        let alphas = Alphas {
            dim: s.dim_alpha,
            unfocused: s.unfocused_alpha,
            unfocused_cursor: s.unfocused_cursor_alpha,
        };

        // Build owned paint params (`FnOnce + 'static` requires no
        // borrows). Clone is cheap: snapshot is a Vec<Vec<Cell>> already
        // sized to the visible grid, buckets are tiny per-row vecs of
        // MatchHit (Copy), theme/typography/cursor are POD-sized.
        let paint_params = PaintParams {
            snapshot: self.snapshot.clone(),
            theme,
            typography: self.typography.clone(),
            cursor,
            cursor_shape: self.snapshot.cursor_shape,
            buckets,
            pane_focused,
            pad,
            hovered_link: self.underlinable_hover(cx),
            selection: self.selection,
            command_badges: self.visible_command_badges(),
            alphas,
        };

        let overlay = if self.search.active {
            let badge = self.search.count_badge();
            let query = self.search.query.clone();
            let typography = self.typography.clone();
            let options = self.search.options;
            // Caret blinks in lock-step with the terminal cursor — the
            // existing 530ms blink_task already drives `cursor_visible`,
            // so the overlay caret needs no second timer.
            let caret_on = self.cursor_visible;
            // Toggle handlers flip the bit and rerun the scan. Rerun
            // touches the backend so it has to live inside the listener
            // (we have `&mut Self` + `&mut Context` here).
            let on_toggle_case = Box::new(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.search.toggle_case_sensitive();
                this.rerun_search(cx);
                cx.notify();
            })) as terminal_search_overlay::ToggleHandler;
            let on_toggle_word = Box::new(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.search.toggle_whole_word();
                this.rerun_search(cx);
                cx.notify();
            })) as terminal_search_overlay::ToggleHandler;
            let on_toggle_regex = Box::new(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.search.toggle_regex();
                this.rerun_search(cx);
                cx.notify();
            })) as terminal_search_overlay::ToggleHandler;
            let on_prev = Box::new(cx.listener(|this, _, _, cx| {
                this.search.prev_match();
                this.follow_current_match();
                cx.notify();
            })) as terminal_search_overlay::ClickHandler;
            let on_next = Box::new(cx.listener(|this, _, _, cx| {
                this.search.next_match();
                this.follow_current_match();
                cx.notify();
            })) as terminal_search_overlay::ClickHandler;
            let on_close = Box::new(cx.listener(|this, _, _, cx| {
                this.search.close();
                cx.notify();
            })) as terminal_search_overlay::ClickHandler;
            Some(terminal_search_overlay::build(
                terminal_search_overlay::Params {
                    query: &query,
                    badge,
                    caret_on,
                    options,
                    theme: &theme,
                    typography: &typography,
                    on_toggle_case,
                    on_toggle_word,
                    on_toggle_regex,
                    on_prev,
                    on_next,
                    on_close,
                },
            ))
        } else {
            None
        };

        // F3.4 slice 3: surface dormancy. A restored sub-pane keeps its
        // prefilled scrollback but holds no shell child yet — the user
        // could otherwise see static text and wonder why nothing reacts.
        // The badge auto-clears once `respawn_if_dormant` flips the
        // backend live + `cx.notify()` re-renders.
        let dormant_badge = self.is_dormant().then(|| build_dormant_badge(&theme));

        // The grid is painted into a canvas child filling the pane body.
        // `canvas(prepaint, paint)` defers everything to a single paint
        // closure: we measure cell metrics, group runs, and emit
        // `paint_quad` + `shape_line` calls directly — no flex layout
        // round-trip per cell. The outer div keeps its existing role as
        // the focus owner, action target, and click-to-focus surface.
        //
        // The paint closure ALSO drives the PTY resize: it derives
        // (cols, rows) from the canvas's real `bounds` and records them in
        // the shared `canvas_grid` cell, then asks for a repaint via
        // `window.refresh()` when the size changed. The next `render`
        // reads `canvas_grid` back into `target_grid` and resizes the PTY.
        //
        // Recording into an `Rc<Cell<_>>` (instead of calling
        // `entity.update` here) keeps the paint phase free of any re-borrow
        // of this entity — `window.refresh()` is documented as safe to
        // call while drawing (it no-ops if already mid-draw and otherwise
        // marks the window dirty for the next frame).
        let dims_typography = self.typography.clone();
        let canvas_grid = Rc::clone(&self.canvas_grid);
        let canvas_bounds = Rc::clone(&self.canvas_bounds);
        // Captured for the per-paint IME input-handler registration (see
        // `TerminalInputHandler`): the focused view receives composed/marked
        // text from the platform input method through it.
        let view_entity = cx.entity();
        let input_focus = self.focus_handle.clone();
        let ime_marked = self.ime_marked.clone();
        let grid_canvas = canvas(
            // Prepaint: no per-paint state to capture; return unit.
            |_bounds, _window, _cx| (),
            move |bounds, _: (), window, cx| {
                // Record the painted bounds so mouse handlers can map a
                // pixel position back to a cell on the next event.
                canvas_bounds.set(bounds);
                let metrics = CellMetrics::measure(&dims_typography, window);
                let dims = grid_dims_for(bounds, &metrics, paint_params.pad);
                if canvas_grid.get() != dims {
                    canvas_grid.set(dims);
                    // Schedule a frame so `render` applies the new size +
                    // refetches the reflowed snapshot. Needed because an
                    // idle terminal emits no PTY output to trigger a
                    // repaint on its own.
                    window.refresh();
                }
                // Cursor cell bounds in window coords, for IME placement and
                // the preedit overlay. `(MAX, MAX)` means the cursor is
                // suppressed (off-blink) — no anchor then.
                let (crow, ccol) = paint_params.cursor;
                let cursor_bounds = if crow == usize::MAX || ccol == usize::MAX {
                    None
                } else {
                    let cw = metrics.cell_width;
                    let lh = metrics.line_height;
                    let x = f32::from(bounds.origin.x) + paint_params.pad + ccol as f32 * cw;
                    let y = f32::from(bounds.origin.y) + paint_params.pad + crow as f32 * lh;
                    Some(Bounds {
                        origin: point(px(x), px(y)),
                        size: size(px(cw), px(lh)),
                    })
                };
                // Register the platform IME bridge. `handle_input` is a no-op
                // unless this view holds focus, so only the focused terminal
                // claims text input. This both enables multi-keystroke
                // composition and disables the press-and-hold accent popup.
                window.handle_input(
                    &input_focus,
                    TerminalInputHandler {
                        view: view_entity.clone(),
                        cursor_bounds,
                    },
                    cx,
                );
                paint_grid(bounds, &paint_params, window, cx);
                // Draw the in-progress composition on top of the grid.
                if let (Some(marked), Some(cb)) = (ime_marked.as_deref(), cursor_bounds) {
                    crate::shell::terminal_canvas::paint_ime_preedit(
                        marked,
                        cb.origin,
                        metrics.line_height_px(),
                        &paint_params.typography,
                        &paint_params.theme,
                        window,
                        cx,
                    );
                }
            },
        )
        .size_full();

        let mut root = div()
            .id("oximux-terminal-view")
            .track_focus(&focus_handle)
            // Carries the terminal key context so Tab / Shift+Tab resolve to
            // the no-op bindings in `register_terminal_key_bindings` (shadowing
            // the host's focus-navigation) and fall through to `on_key_down`,
            // which forwards them to the shell for completion.
            .key_context(TERMINAL_KEY_CONTEXT)
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            // Anchor for absolute-positioned overlays (dormant badge,
            // search overlay) — the canvas child stays in flex flow.
            .relative()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            // `.font(...)` over `.font_family(...)` so the configured
            // per-platform fallback chain takes effect for glyphs the
            // primary face lacks; `font_family` takes a single literal name
            // and never cascades. Note the chain does NOT rescue a primary
            // that fails to load at all — see `Typography::platform_fonts`.
            // The canvas
            // paint reads typography directly, but keeping the font on
            // the root is the right default for any non-canvas children
            // (overlays, error banners) that may inherit it later.
            .font(self.typography.mono_font())
            .text_size(px(self.typography.t_body_lg))
            .on_action(cx.listener(Self::on_search))
            .on_action(cx.listener(Self::on_find_next))
            .on_action(cx.listener(Self::on_find_prev))
            .on_action(cx.listener(Self::on_send_selection_to_agent))
            .on_action(cx.listener(Self::on_send_last_command_output_to_agent))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    // Cmd-click on a link opens it instead of selecting/reporting.
                    if this.try_open_link(ev, window, cx) {
                        cx.notify();
                        return;
                    }
                    // A mouse-reporting app (no Shift) gets the click forwarded
                    // instead of starting a local selection.
                    if !this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Press,
                        window,
                        cx,
                    ) {
                        this.on_select_down(ev, window);
                    }
                    // Notify so `MainPane`'s observer can re-sync the focused
                    // PaneId and repaint the active-pane ring on the next
                    // frame. Without this, click-to-focus is invisible until
                    // the next Cmd-* action.
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                // A scrollbar-thumb drag owns the gesture: map travel to offset
                // and skip hover/selection until the button is released.
                if this.scrollbar_drag.is_some() {
                    this.drag_scrollbar(ev.position.y, window, cx);
                    return;
                }
                // Cmd-hover link underline updates regardless of button state.
                this.update_hover(ev, window, cx);
                let Some(button) = ev.pressed_button else {
                    return;
                };
                // Forward drags to a mouse-reporting app; otherwise extend the
                // local selection.
                if this.report_mouse(
                    button,
                    ev.position,
                    &ev.modifiers,
                    MouseAction::Drag,
                    window,
                    cx,
                ) {
                    return;
                }
                if this.selecting.is_some() && button == MouseButton::Left {
                    let cell = this.cell_at(ev.position, window);
                    this.apply_drag(cell);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                    // End a scrollbar drag without forwarding the release to the
                    // grid (no mouse-report, no selection finalize).
                    if this.scrollbar_drag.take().is_some() {
                        cx.notify();
                        return;
                    }
                    if this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Release,
                        window,
                        cx,
                    ) {
                        return;
                    }
                    if this.finish_select(cx) {
                        cx.notify();
                    }
                }),
            )
            // Right button: forward to a mouse-reporting app (vim/tmux own
            // their own right-click menus); otherwise open the local terminal
            // context menu at the cursor. Middle-click forwards when reporting
            // and otherwise has no local fallback.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    // Focus the pane being acted on (matches left-click).
                    this.focus_handle.focus(window, cx);
                    // A mouse-reporting app consumes the press — never shadow
                    // its own right-click handling with a local menu.
                    if this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Press,
                        window,
                        cx,
                    ) {
                        return;
                    }
                    let (row, col) = this.cell_at(ev.position, window);
                    // Auto-select the word under the cursor when nothing is
                    // selected so Copy / Send-to-Agent are meaningful.
                    if this.selection.is_none() {
                        this.select_word_at(row, col);
                    }
                    let link = this.link_string_at(row, col);
                    let has_selection = this.selection.is_some();
                    window.dispatch_action(
                        Box::new(OpenTerminalContextMenuAt {
                            x: f32::from(ev.position.x),
                            y: f32::from(ev.position.y),
                            session_id: this.session_id.0,
                            has_selection,
                            link,
                        }),
                        cx,
                    );
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                    this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Release,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Press,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                    // A mouse-reporting app consumes the release; otherwise
                    // middle-click pastes the clipboard (macOS has no separate
                    // X11 primary selection, so the system clipboard stands in
                    // — paired with `copy_on_select` this mirrors the classic
                    // select-then-middle-click-paste workflow).
                    if this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Release,
                        window,
                        cx,
                    ) {
                        return;
                    }
                    this.paste_from_clipboard(cx);
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, window, cx| {
                this.on_wheel(ev, window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .child(grid_canvas);
        if let Some(o) = overlay {
            root = root.child(o);
        }
        if let Some(badge) = dormant_badge {
            root = root.child(badge);
        }
        // A pane whose PTY child has exited gets a centered "process exited"
        // strip so a dead leader reads as finished, not hung — otherwise the
        // frozen final frame is indistinguishable from a stuck terminal.
        // Mutually exclusive with the dormant badge (dormant = never spawned;
        // exited = spawned and died).
        if let Some(code) = self.exited {
            root = root.child(build_exit_banner(&theme, code));
        }
        // Scrolled-up indicator: a faint chip while the viewport is off the
        // live tail, so the user knows new output is landing below the fold
        // and that any keystroke will snap back down.
        if self.snapshot.display_offset > 0 {
            root = root.child(build_scroll_indicator(&theme, self.snapshot.display_offset).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                    // Click the chip to jump to the live tail. Stop propagation
                    // so the same click doesn't also start a selection on the
                    // grid underneath (root owns that mouse-down listener).
                    this.scroll_to_tail(cx);
                    cx.stop_propagation();
                }),
            ));
        }
        // Overlay scrollbar on the right edge (only when scrollback exists).
        if let Some(bar) = self.render_scrollbar(&theme, cx) {
            root = root.child(bar);
        }
        // Attention ring: a blue inset stroke when an unfocused pane has
        // signalled (terminal BEL today; agent-waiting / `oximux notify`
        // later). Absolute inset_0 so it overlays without shifting the grid
        // layout, and gated on `!pane_focused` so it vanishes the instant the
        // user looks at the pane (belt-and-braces with the on_focus clear).
        if self.attention && !pane_focused {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .border_2()
                    .border_color(theme.status_info),
            );
        }
        root
    }
}
