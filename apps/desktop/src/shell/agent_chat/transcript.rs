//! The scrolling transcript: how a `Vec<ThreadEntry>` becomes rows on screen,
//! and every scroll primitive the rest of the chat drives it with.
//!
//! Split out of `mod.rs` unchanged. The three consumers that jump around the
//! transcript — the tick rail ([`super::message_rail`]), the jump list
//! ([`super::jump_menu`]) and the find bar ([`super::find_bar`]) — all address
//! it through the primitives here rather than touching the scroll handle
//! themselves, so there is one place that knows how an entry index becomes a
//! scroll position.
//!
//! That translation is the awkward part and the reason this module exists as a
//! unit: entries do **not** map one-to-one onto scroll children. A collapsed
//! tool run renders nothing, an expander renders a child with no entry behind
//! it, so [`user_turn_child_indices`] and [`entry_child_indices`] rebuild the
//! map every frame from the flags the render loop just produced.

use super::*;

/// One direct child of the scrolling transcript: a reading-measure column,
/// `width` px wide. Callers pass [`AgentChatView::content_width`].
///
/// The **definite** width is load-bearing, and the obvious spelling
/// (`w_full().max_w(px(CONTENT_MAX_W))`) is what this must never go back to.
/// Under that spelling taffy sizes the column's height against the container's
/// *available* width and only clamps to the max-width afterwards — so a reply
/// measured across the full pane re-wraps into more lines once it is capped to
/// the reading measure, and paints those extra lines outside the height it
/// reported. The turn below then draws on top of the tail (a ~475px reply
/// reporting 400px was the observed case). Sizing the column to one already-
/// capped number keeps measure width == paint width, so a reply's box always
/// matches the text in it.
///
/// `flex().flex_col()` matters too: a bare block lets a wide bubble escape the
/// column.
fn transcript_column(width: f32) -> gpui::Div {
    div().flex().flex_col().flex_shrink_0().w(px(width))
}

/// Compute, for each USER turn in order, its child index within the flattened
/// transcript scroll box — the input to `ScrollHandle::scroll_to_item` for a
/// jump. Each slice is indexed by entry position in transcript order:
/// `produces[i]` = entry `i` rendered a direct child element; `is_user[i]` =
/// it's a user turn; `has_expander[i]` = a collapsed-tool-run expander child is
/// pushed right after it. Children are counted in the exact push order
/// `render_transcript` uses, so the returned indices line up with the tracked
/// `list_scroll` child bounds. Pure + unit-tested; render feeds it the live
/// per-entry flags.
fn user_turn_child_indices(produces: &[bool], is_user: &[bool], has_expander: &[bool]) -> Vec<usize> {
    let mut child_ord = 0usize;
    let mut out = Vec::new();
    for (i, &produced) in produces.iter().enumerate() {
        if produced {
            if is_user.get(i).copied().unwrap_or(false) {
                out.push(child_ord);
            }
            child_ord += 1;
        }
        if has_expander.get(i).copied().unwrap_or(false) {
            child_ord += 1;
        }
    }
    out
}

/// Map every RENDERED entry's transcript index → its scroll-child index (for the
/// in-chat find bar's jump-to-match), mirroring the push order in
/// `render_transcript`: one child per producing row, plus one per trailing
/// expander. Rows that produce no element are absent. Pure for unit testing.
fn entry_child_indices(
    entry_idx: &[usize],
    produces: &[bool],
    has_expander: &[bool],
) -> Vec<(usize, usize)> {
    let mut child_ord = 0usize;
    let mut out = Vec::new();
    for i in 0..produces.len() {
        if produces[i] {
            out.push((entry_idx[i], child_ord));
            child_ord += 1;
        }
        if has_expander.get(i).copied().unwrap_or(false) {
            child_ord += 1;
        }
    }
    out
}

impl AgentChatView {
    /// Whether the transcript is scrolled to (within one card of) the bottom.
    /// `offset().y` is `<= 0` and reaches `-max_offset().y` at the very bottom,
    /// so their sum is the remaining scroll distance. Fresh views (no paint yet)
    /// report `0`, i.e. "at bottom", so the first turn follows.
    pub(super) fn is_near_bottom(&self) -> bool {
        let sh = &self.list_scroll;
        sh.max_offset().y + sh.offset().y <= px(160.0)
    }

    /// Jump the transcript to the `n`-th user turn (0-based ordinal among user
    /// messages) and briefly highlight it. Releases auto-follow so the jump
    /// sticks, and re-issues the scroll once next frame in case the target's
    /// markdown height is still settling. No-op if `n` is out of range or that
    /// turn wasn't rendered this frame. Shared primitive for the jump menu and
    /// (later) the message rail.
    pub(super) fn scroll_to_user_ordinal(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let child_ix = match self.user_child_ix.borrow().get(n) {
            Some(&ix) => ix,
            None => return,
        };
        let Some(entry_idx) = self.thread.user_entry_index(n) else {
            return;
        };
        self.stick_to_bottom = false;
        self.list_scroll.scroll_to_item(child_ix);
        self.flash_entry = Some(entry_idx);
        self.flash_frames = FLASH_FRAMES;
        // The target's height can settle a frame late (async markdown), which
        // leaves a first scroll landing short when a long reply sits above it;
        // re-issue once on the next frame against the freshly-measured bounds.
        let this = cx.entity().downgrade();
        window.on_next_frame(move |_window, cx| {
            let _ = this.update(cx, |this, cx| {
                if let Some(&ix) = this.user_child_ix.borrow().get(n) {
                    this.list_scroll.scroll_to_item(ix);
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// Scroll so the entry at `entry_idx` is in view and briefly flash it — the
    /// find bar's jump-to-match. Window-free (a single `scroll_to_item`, no
    /// next-frame re-measure) because it's driven from the find input's change
    /// subscription, which has no `Window`. Reads the per-entry child map
    /// rebuilt each render (`entry_child_ix`).
    pub(super) fn scroll_to_entry(&mut self, entry_idx: usize, cx: &mut Context<Self>) {
        let Some(&child_ix) = self.entry_child_ix.borrow().get(&entry_idx) else {
            return;
        };
        self.stick_to_bottom = false;
        self.list_scroll.scroll_to_item(child_ix);
        self.flash_entry = Some(entry_idx);
        self.flash_frames = FLASH_FRAMES;
        cx.notify();
    }

    /// The user-turn ordinal currently at (or just above) the top of the
    /// viewport — the anchor for prev/next navigation. `user_child_ix` is sorted
    /// ascending (child index grows with ordinal), so a binary search over it
    /// against the top visible child maps back to an ordinal. Returns 0 when
    /// nothing is scrolled or there are no user turns.
    pub(super) fn current_user_ordinal(&self) -> usize {
        let top_child = self.list_scroll.top_item();
        match self.user_child_ix.borrow().binary_search(&top_child) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// The width every transcript child is built at: the reading measure
    /// ([`CONTENT_MAX_W`]) on a roomy pane, or the pane itself once it is
    /// narrower (a split pane, a dragged-in window edge).
    ///
    /// This is resolved here, in the view, rather than left to
    /// `max_w(px(CONTENT_MAX_W))` on the children, because the children's text
    /// must be *measured* at the width it will *paint* at — see
    /// [`transcript_column`]. The scroll box's own width is the only reading of
    /// "how much room is there", and it is last frame's: a fresh view has no
    /// bounds yet and a resize lands one frame late, so fall back to the cap and
    /// let the next frame settle it. No feedback loop — the scroll box is
    /// full-width regardless of what its children ask for.
    pub(super) fn content_width(&self) -> f32 {
        let painted = f32::from(self.list_scroll.bounds().size.width) - self.density.pad_panel * 2.0;
        if painted <= 0.0 { CONTENT_MAX_W } else { painted.min(CONTENT_MAX_W) }
    }

    /// The scrollable transcript column. Entries stack in a centered reading
    /// column ([`CONTENT_MAX_W`]) so wide windows don't stretch text edge-to-
    /// edge; the outer element only scrolls and centers.
    pub(super) fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let content_w = self.content_width();
        let scroll = div()
            .id("agent-chat-list")
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .flex_1()
            // `min_h(0)` is essential: a flex child defaults to `min-height:auto`
            // (= content height), so without this the transcript grows to its
            // content size instead of shrinking to the flex-allocated space —
            // its scroll box then extends past the composer and the true bottom
            // (the newest message / approval row) is never reachable, no matter
            // the scroll offset. Pinning min-height to 0 lets it shrink so
            // `overflow_y_scroll` actually bounds the box to the visible area.
            .min_h(px(0.0))
            // Same reasoning on the cross axis — see the note in `wrap_scroll`.
            .min_w_0()
            .px(px(density.pad_panel))
            .py(px(density.pad_panel))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            // Release auto-follow when the user scrolls UP to read history (so a
            // streaming turn doesn't yank them back down); re-arm once they
            // return to the bottom. gpui's scroll offset grows more negative as
            // you scroll down, so a positive wheel delta means "toward the top".
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _window, cx| {
                let dy = ev.delta.pixel_delta(px(20.0)).y;
                let was = this.stick_to_bottom;
                if dy > px(0.0) {
                    this.stick_to_bottom = false;
                } else if this.is_near_bottom() {
                    this.stick_to_bottom = true;
                }
                if this.stick_to_bottom != was {
                    cx.notify();
                }
            }));

        if self.thread.entries.is_empty() {
            // Even the empty state rides the scroll box + overlay so the layout
            // is identical once messages arrive; the scrollbar auto-hides when
            // content fits.
            //
            // A sign-in requirement is surfaced BEFORE any session opens, so the
            // transcript is still empty when it lands — render the auth card here
            // too, or it would never reach the tail-card chain below (which only
            // runs once there are entries) and the empty greeting would shadow it.
            let body = if self.auth.is_some() {
                let card = self.render_auth_card(cx);
                transcript_column(content_w)
                    .child(card)
                    .into_any_element()
            } else {
                self.render_empty_hint(&theme, &typo)
            };
            return self.wrap_scroll(scroll.child(body)).into_any_element();
        }

        // Flatten the transcript: each turn is a DIRECT child of the tracked
        // scroll box (wrapped in a centered max-width column so the reading
        // measure is unchanged) rather than sharing one inner `content` column.
        // gpui records child bounds for direct children only, so this is what
        // lets `ScrollHandle::scroll_to_item` reveal an exact user turn for jump
        // navigation. The inter-turn gap moves from the old column onto the
        // scroll box; turns breathe a little more than inline content.
        let mut scroll = scroll.gap(px(density.pad_panel * 2.0));

        // Group long runs of tool cards: a run of >8 collapses to first-3 +
        // "N more" + last-2, with pending/failed cards always kept visible.
        let is_tool: Vec<bool> = self
            .thread
            .entries
            .iter()
            .map(|e| matches!(e, ThreadEntry::ToolCall(_)))
            .collect();
        let force_show: Vec<bool> = self
            .thread
            .entries
            .iter()
            .map(|e| matches!(e, ThreadEntry::ToolCall(tc) if must_stay_visible(tc)))
            .collect();
        let group_plan = plan_tool_grouping(&is_tool, &force_show, &self.expanded_tool_runs);

        // Build each visible entry's element first, capturing per-entry flags so
        // "which scroll child is which user turn" is a pure, unit-tested function
        // (`user_turn_child_indices`) rather than logic tangled into the push loop.
        struct Row {
            entry_idx: usize,
            el: Option<AnyElement>,
            dimmed: bool,
            is_user: bool,
            expander: Option<AnyElement>,
        }
        let mut rows: Vec<Row> = Vec::with_capacity(self.thread.entries.len());
        for (idx, entry) in self.thread.entries.iter().enumerate() {
            if matches!(group_plan[idx], EntryDisplay::Hide) {
                continue;
            }
            let is_user = matches!(entry, ThreadEntry::User { .. });
            let el: Option<AnyElement> = match entry {
                ThreadEntry::User { text, images, .. } => {
                    // No "You" caption — the right-aligned bubble is the signal.
                    Some(self.render_user_entry(idx, text, images, cx))
                }
                ThreadEntry::Assistant(msg) => {
                    if msg.is_empty() {
                        None
                    } else {
                        let group = SharedString::from(format!("chat-asst-{idx}"));
                        let mut block = div()
                            .group(group.clone())
                            .flex()
                            .flex_col()
                            // Let the column shrink to the max-width wrapper so a
                            // long markdown line wraps instead of overflowing the
                            // edge (see `bubble::assistant_body`).
                            .min_w_0()
                            .gap(px(4.0))
                            .w_full()
                            .child(assistant_header(
                                idx,
                                self.recently_copied == Some(idx),
                                // Regenerate is a constrained rewind: offer it only
                                // on a settled, resumable, connected thread AND only
                                // on a reply in the LAST turn (no user prompt after
                                // it). Regenerating an earlier reply would silently
                                // fork + drop every later turn in one click, with no
                                // confirmation — so it's restricted to the tail turn,
                                // where the only thing dropped is the reply itself.
                                !self.thread.turn_active
                                    && !self.disconnected
                                    && !self.rewinding
                                    && self.thread.session_id.is_some()
                                    && self.backend_supports_rewind()
                                    && !self.thread.entries[idx + 1..]
                                        .iter()
                                        .any(|e| matches!(e, ThreadEntry::User { .. })),
                                group,
                                &msg.text,
                                self.provider_label(),
                                theme,
                                &typo,
                                cx,
                            ));
                        // Thinking display honors the chat-wide level (see
                        // `thinking_expanded`): Hidden drops the block; Expanded
                        // forces it open; Auto peeks the streaming thought and
                        // otherwise respects the user's per-entry toggle.
                        if !msg.thinking.is_empty() && self.thinking_level != ThinkingLevel::Hidden {
                            let is_last = idx + 1 == self.thread.entries.len();
                            let expanded = self.thinking_expanded(idx, is_last, msg);
                            block = block.child(thinking_block(
                                idx, expanded, &msg.thinking, theme, density, &typo, cx,
                            ));
                        }
                        if !msg.text.is_empty() {
                            block = block.child(bubble::assistant_body(idx, &msg.text, &typo));
                        }
                        Some(block.into_any_element())
                    }
                }
                ThreadEntry::ToolCall(tc) => {
                    // An AskUserQuestion awaiting answers renders as the dedicated
                    // interactive question card (reconciled into `question_cards`
                    // before this loop); a TodoWrite as a read-only plan checklist;
                    // every other tool call uses the generic (expandable) card.
                    if matches!(tc.status, ToolCallStatus::AwaitingAnswer(_)) {
                        self.question_cards.get(&tc.id).map(|c| c.clone().into_any_element())
                    } else if question_card::is_question(tc) {
                        // Answered/skipped question → a compact one-line summary.
                        Some(question_card::render_settled(tc, theme, density, &typo).into_any_element())
                    } else if plan_panel::is_plan(tc) {
                        Some(plan_panel::render_plan_card(tc, theme, density, &typo).into_any_element())
                    } else {
                        let expanded = self.expanded_tool_calls.contains(&tc.id);
                        let card = tool_card::render_tool_card(
                            tc,
                            expanded,
                            self.provider_label(),
                            self.screen_context(tc),
                            theme,
                            density,
                            &typo,
                            cx,
                        );
                        // Append inline result-image thumbnails (a Read of an
                        // image, a screenshot) and/or an ACP embedded terminal
                        // below the card. Both are optional; a plain tool renders
                        // the bare card.
                        let thumbs = self.render_tool_result_images(idx, &tc.images, cx);
                        let terminal = self.render_embedded_terminal(&tc.id);
                        if thumbs.is_some() || terminal.is_some() {
                            let mut col = div().flex().flex_col().w_full().child(card);
                            if let Some(thumbs) = thumbs {
                                col = col.child(thumbs);
                            }
                            if let Some(terminal) = terminal {
                                col = col.child(terminal);
                            }
                            Some(col.into_any_element())
                        } else {
                            Some(card.into_any_element())
                        }
                    }
                }
                ThreadEntry::ContextCompaction { summary } => {
                    Some(compaction_divider(summary, theme, &typo).into_any_element())
                }
                // What the turn changed on disk, closing the turn. Review opens the
                // turn's own diff; it is offered only when the backend reported
                // one, since a derived summary has no hunks to show.
                ThreadEntry::TurnDiff { files, diff } => {
                    let on_review = diff.clone().map(|d| {
                        // Key the tab by the DIFF ITSELF, not by anything
                        // positional. An entry index is not an identity: it is
                        // scoped to one transcript, so two chats' first editing
                        // turn would both key "2" and one would silently
                        // reactivate the other's tab; and rewind/edit-resend
                        // truncate and repopulate from the same index, so a
                        // post-rewind turn would reactivate the pre-rewind tab.
                        // Both show the WRONG diff under the right label.
                        //
                        // Content-addressing makes a collision mean the content is
                        // identical, in which case reusing the tab is correct.
                        let key = diff_tab_key(&d);
                        Box::new(cx.listener(move |_this, _e: &ClickEvent, _w, cx| {
                            cx.emit(AgentChatEvent::ReviewTurnDiffRequested {
                                key: key.clone(),
                                diff: d.clone(),
                            });
                        })) as Box<_>
                    });
                    Some(turn_summary_card::render(files, theme, density, &typo, on_review))
                }
            };
            // A collapsed tool-run expander follows its anchor entry as its own child.
            let expander = match group_plan[idx] {
                EntryDisplay::ShowThenExpander { run_start, hidden } => {
                    // Summarize the cards the collapse HIDES — what's behind the
                    // fold is exactly what the user can't see for themselves. The
                    // run is the consecutive tool block from `run_start`, the same
                    // extent `plan_tool_grouping` collapsed.
                    let collapsed: Vec<GroupedTool> = (run_start..is_tool.len())
                        .take_while(|&i| is_tool[i])
                        .filter(|&i| matches!(group_plan[i], EntryDisplay::Hide))
                        .filter_map(|i| match self.thread.entries.get(i) {
                            Some(ThreadEntry::ToolCall(tc)) => Some(GroupedTool {
                                kind: ToolDetail::classify(&tc.name, tc.kind.as_deref(), &tc.input),
                                failed: matches!(tc.status, ToolCallStatus::Failed(_)),
                                target: bubble::tool_target(tc),
                                screen: screen_card::is_screen_call(&tc.name),
                            }),
                            _ => None,
                        })
                        .collect();
                    let summary = summarize_tool_run(&collapsed);
                    Some(self.render_tool_run_expander(run_start, hidden, summary, cx))
                }
                _ => None,
            };
            let dimmed = el.is_some() && self.is_pending_edit_dimmed(idx);
            rows.push(Row { entry_idx: idx, el, dimmed, is_user, expander });
        }

        // Pure child-index map (user ordinal → scroll child index), rebuilt every
        // render and read by `scroll_to_user_ordinal` for jump nav / the rail.
        let produces: Vec<bool> = rows.iter().map(|r| r.el.is_some()).collect();
        let user_flags: Vec<bool> = rows.iter().map(|r| r.is_user).collect();
        let expander_flags: Vec<bool> = rows.iter().map(|r| r.expander.is_some()).collect();
        *self.user_child_ix.borrow_mut() =
            user_turn_child_indices(&produces, &user_flags, &expander_flags);
        // Same child accounting, but keyed by entry index across all rendered
        // entries — the find bar jumps to any matching entry, not just user turns.
        let rows_entry_idx: Vec<usize> = rows.iter().map(|r| r.entry_idx).collect();
        *self.entry_child_ix.borrow_mut() =
            entry_child_indices(&rows_entry_idx, &produces, &expander_flags).into_iter().collect();

        // Push each entry (then any trailing tool-run expander) as a DIRECT child
        // of the scroll box, in the exact order the index map counted, each in a
        // centered max-width wrapper matching the old single column. The wrapper
        // MUST be `flex().flex_col()` (not a bare block) so the max-width actually
        // caps the child — a plain block lets a wide bubble overflow past the edge.
        for row in rows {
            if let Some(el) = row.el {
                let mut wrap =
                    transcript_column(content_w).child(el);
                if row.dimmed {
                    // A staged edit dims the messages it will remove on send.
                    wrap = wrap.opacity(0.4);
                }
                // A jumped-to turn briefly tints its wrapper (whole-row highlight),
                // fading with the frame counter so it settles rather than snaps.
                if self.flash_entry == Some(row.entry_idx) {
                    let a = (self.flash_frames as f32 / FLASH_FRAMES as f32).clamp(0.0, 1.0);
                    wrap = wrap
                        .rounded(px(density.r_card))
                        .bg(theme.focus_ring.opacity(0.16 * a));
                }
                scroll = scroll.child(wrap);
            }
            if let Some(expander) = row.expander {
                scroll = scroll.child(
                    transcript_column(content_w).child(expander),
                );
            }
        }
        // The agent's execution plan (ACP `Plan`) as a pinned checklist at the tail
        // of the transcript — one card, full-replaced on each `PlanUpdated`, kept
        // across turns until cleared. Reuses the `TodoWrite` checklist renderer.
        if let Some(entries) = self.thread.plan.as_ref().filter(|e| !e.is_empty()) {
            scroll = scroll.child(
                transcript_column(content_w)
                    .child(plan_panel::render_plan_entries(entries, theme, density, &typo)),
            );
        }
        // Live turn / disconnect state lives at the tail of the transcript (like
        // a native chat), NOT above the composer — so it never resizes the input.
        // These trail every user turn, so they never shift the child-index map.
        if self.disconnected {
            // A crash is terminal for this child, but the session is usually
            // resumable — offer Retry, which respawns via `--resume` then
            // re-sends the last prompt.
            let msg = self
                .thread
                .last_error
                .clone()
                .unwrap_or_else(|| "Agent process exited.".to_string());
            let retry = self.retry_button(cx);
            scroll = scroll.child(
                transcript_column(content_w)
                    .child(error_card::error_card(&msg, theme, &typo, retry)),
            );
        } else if self.auth.is_some() {
            // The agent needs login before a session can open — the auth card is
            // the only actionable state, so it takes precedence over the working
            // indicator and the plain error/signed-out cards BELOW it. (The
            // `disconnected` error card above wins over it, but a failed
            // EnvVar-auth respawn clears `self.auth`, so the two never coexist.)
            let card = self.render_auth_card(cx);
            scroll = scroll.child(
                transcript_column(content_w).child(card),
            );
        } else if self.thread.turn_active {
            // While a question card is pending, the agent isn't working — it's
            // blocked on the user's answer — so don't show the "working…" spinner
            // (it would also add height that pushes the card's controls down).
            if self.thread.pending_question().is_none() {
                // While compacting, show the specific "Compacting context…"
                // spinner instead of the generic "…is working…" so a long
                // compaction reads as progress, not a hang.
                let indicator = if self.thread.compacting {
                    compacting_indicator(theme, &typo)
                } else {
                    working_indicator(self.provider_label(), theme, &typo)
                };
                scroll = scroll.child(
                    transcript_column(content_w).child(indicator),
                );
            }
        } else if self.is_signed_out() && self.login_adapter_id().is_some() {
            // The turn settled (or errored) on an auth failure whose fix is a
            // terminal sign-in. Turn the dead-end reply into an action: a banner
            // that opens a terminal running the agent CLI, where `/login` works.
            // Takes precedence over the plain error card below since it's the
            // actionable version of the same state.
            let action = self.open_login_terminal_button(cx);
            scroll = scroll.child(
                transcript_column(content_w)
                    .child(login_card::login_card(self.provider_label(), theme, &typo, action)),
            );
        } else if let Some(err) = self.thread.last_error.clone() {
            // An idle turn that ended in error: surface it inline at the tail
            // with a Retry. This is the ONLY place a failure after the first
            // message becomes visible — the empty-state hint that also renders
            // `last_error` only paints when the transcript is empty.
            let retry = self.retry_button(cx);
            scroll = scroll.child(
                transcript_column(content_w)
                    .child(error_card::error_card(&err, theme, &typo, retry)),
            );
        } else {
            // A settled turn: surface its one-line summary and token/cost usage
            // (both decoded by the backend; shown only when present).
            if let Some(summary) = self
                .thread
                .last_summary
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                scroll = scroll.child(
                    transcript_column(content_w)
                        .child(summary_line(summary, theme, &typo)),
                );
            }
            if let Some(usage) = self.thread.usage.as_ref() {
                scroll = scroll.child(
                    transcript_column(content_w)
                        .child(usage_footer(usage, theme, &typo)),
                );
            }
        }
        // Trailing clearance INSIDE the scrollable content, above the composer:
        // a plain breathing margin below the last line. It used to carry a second,
        // much larger term estimating a reply's "under-counted tail", back when a
        // multi-paragraph reply painted past the height it reported and
        // `scroll_to_bottom` — which pins to gpui's `scroll_max`, derived from the
        // measured content height — could not reach the end of it. Transcript
        // children are now measured at the width they paint at (see
        // [`transcript_column`]), so the measured content height is the real one
        // and no extra reveal room is needed. A pending question keeps a roomier
        // margin so its Allow/Reject controls clear the composer.
        let tail_gap = if self.thread.pending_question().is_some() {
            px(160.0)
        } else {
            px(density.pad_panel * 4.0)
        };
        scroll = scroll.child(div().flex_none().w_full().h(tail_gap));
        // Compose the timeline row: the left tick-rail, the scrolling transcript,
        // and the top-left jump dropdown + hover preview as absolute overlays over
        // it. The `relative` row is the positioning context all three overlays
        // (and the rail's per-tick fractions) resolve against.
        div()
            .relative()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .children(self.render_message_rail(cx))
            .child(self.wrap_scroll(scroll))
            .children(self.render_jump_list(cx))
            .children(self.render_session_detail(cx))
            .children(self.render_find_bar(cx))
            .into_any_element()
    }

    /// Wrap the scrolling transcript box in a positioned container and overlay a
    /// fading scrollbar bound to the SAME [`ScrollHandle`]. The bar paints on the
    /// container's right edge, auto-hides when the content fits, and — being a
    /// `Normal` hitbox gated to its own 16px strip — never blocks clicks on the
    /// messages, tool cards, or Allow/Reject rows beneath it.
    pub(super) fn wrap_scroll(&self, scroll_box: impl IntoElement) -> gpui::Div {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            // The horizontal twin of `min_h(0)`, and just as load-bearing. A flex
            // item defaults to `min-width: auto`, i.e. "never shrink below my
            // content's min-content width" — and the transcript's children are
            // sized to a definite width taken from THIS box's measured width
            // ([`AgentChatView::content_width`]). Leave the default on and the two
            // feed each other: the children pin the box open at the reading
            // measure, the box reports that width back, and a pane narrower than
            // the measure never shrinks — it just clips the text. Zeroing the
            // min-width breaks the cycle, so the box always reports the room it
            // actually has.
            .min_w_0()
            .child(scroll_box)
            .child(Scrollbar::vertical(&self.list_scroll))
    }

    pub(super) fn render_empty_hint(&self, theme: &Theme, typo: &Typography) -> AnyElement {
        // Disconnected → surface the error plainly. Otherwise a calm, centered
        // greeting (title + hint) rather than a lone sentence.
        let (title, subtitle, title_color) = if self.disconnected {
            (
                "Agent unavailable",
                self.thread.last_error.as_deref().unwrap_or("The agent process exited.").to_string(),
                theme.status_error,
            )
        } else {
            (
                "Start a conversation",
                format!("Ask {} to explain code, make edits, or run commands.", self.provider_label()),
                theme.fg_muted,
            )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .w_full()
            .child(
                div()
                    .text_size(px(typo.t_body_lg))
                    .text_color(title_color)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(subtitle)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_child_indices_map_ordinals_to_scroll_children() {
        // Alternating user/assistant, every entry renders a child: user turns sit
        // at even child indices.
        assert_eq!(
            user_turn_child_indices(
                &[true, true, true, true],
                &[true, false, true, false],
                &[false, false, false, false],
            ),
            vec![0, 2],
        );

        // An empty assistant renders no child, so it doesn't consume a child
        // index — the following user turn shifts up by one.
        assert_eq!(
            user_turn_child_indices(
                &[true, false, true],
                &[true, false, true],
                &[false, false, false],
            ),
            vec![0, 1],
        );

        // A collapsed tool-run expander is its own extra child pushed after its
        // anchor, so it advances the child counter without being a user turn.
        assert_eq!(
            user_turn_child_indices(
                &[true, true, true, true],
                &[true, false, false, true],
                &[false, true, false, false],
            ),
            vec![0, 4],
        );

        // No entries → no user turns.
        assert_eq!(user_turn_child_indices(&[], &[], &[]), Vec::<usize>::new());
    }

    #[test]
    fn entry_child_indices_map_every_rendered_entry() {
        // entry_idx per row, produces, has_expander. Row 1 (an empty assistant)
        // produces no child; row 2 carries a trailing expander.
        let entry_idx = [0usize, 1, 2, 3];
        let produces = [true, false, true, true];
        let has_expander = [false, false, true, false];
        // Child indices: entry0→0, entry1 skipped, entry2→1 (+expander at 2),
        // entry3→3. Keyed by entry index, not ordinal.
        let mut got = entry_child_indices(&entry_idx, &produces, &has_expander);
        got.sort();
        assert_eq!(got, vec![(0, 0), (2, 1), (3, 3)]);
        // Empty transcript → empty map.
        assert!(entry_child_indices(&[], &[], &[]).is_empty());
    }
}
