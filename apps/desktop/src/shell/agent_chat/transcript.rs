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

/// One child of the transcript, in the order it is laid out.
///
/// Entries do **not** map one-to-one onto children, which is the entire reason
/// this type exists. A tool call collapsed inside a long run renders nothing; a
/// run expander renders a child with no [`ThreadEntry`] behind it at all; an
/// assistant message that has not streamed a byte yet renders nothing either.
/// Naming each child turns "which child is entry N" into a lookup instead of an
/// accounting exercise over parallel flag arrays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TranscriptRow {
    /// The entry at this transcript index, rendered normally.
    Entry { entry_idx: usize },
    /// The "N more" expander for the collapsed tool run starting at `run_start`.
    /// No [`ThreadEntry`] corresponds to it.
    Expander { run_start: usize },
}

impl TranscriptRow {
    /// The entry behind this row, if any. `None` for an expander — which is the
    /// distinction every reverse lookup here turns on.
    fn entry(&self) -> Option<usize> {
        match *self {
            TranscriptRow::Entry { entry_idx } => Some(entry_idx),
            TranscriptRow::Expander { .. } => None,
        }
    }
}

/// Whether an entry renders a child at all.
///
/// Exactly one entry kind can render nothing: an assistant message with no text
/// streamed yet. It is spelled once and shared, because [`build_rows`] and the
/// render loop disagreeing about it is precisely the bug the row model exists to
/// prevent — one would count a child the other never pushed, and every jump
/// target past that point would land a row off.
fn produces_element(entry: &ThreadEntry) -> bool {
    !matches!(entry, ThreadEntry::Assistant(msg) if msg.is_empty())
}

/// Flatten the transcript into the exact child sequence the render loop pushes.
///
/// Pure, so the ordering every jump / rail / find target resolves through is
/// unit-testable without a window.
pub(super) fn build_rows(entries: &[ThreadEntry], plan: &[EntryDisplay]) -> Vec<TranscriptRow> {
    let mut rows = Vec::with_capacity(entries.len());
    for (idx, entry) in entries.iter().enumerate() {
        if matches!(plan[idx], EntryDisplay::Hide) {
            continue;
        }
        if produces_element(entry) {
            rows.push(TranscriptRow::Entry { entry_idx: idx });
        }
        // The expander trails its anchor as its own child — including when the
        // anchor itself rendered nothing, which is what the flag-array
        // accounting did and what keeps the two provably identical.
        if let EntryDisplay::ShowThenExpander { run_start, .. } = plan[idx] {
            rows.push(TranscriptRow::Expander { run_start });
        }
    }
    rows
}

impl AgentChatView {
    /// The child index of `entry_idx`, or `None` when that entry has no child —
    /// it is hidden inside a collapsed tool run, or it is an assistant message
    /// that has not streamed yet. Callers treat `None` as "nothing to jump to".
    fn row_of_entry(&self, entry_idx: usize) -> Option<usize> {
        self.rows.borrow().iter().position(|r| r.entry() == Some(entry_idx))
    }

    /// Every entry index that currently has a child, for callers that need to
    /// test many entries at once (the find bar filtering its match list). One
    /// pass over the rows instead of one scan per candidate.
    pub(super) fn rendered_entries(&self) -> HashSet<usize> {
        self.rows.borrow().iter().filter_map(|r| r.entry()).collect()
    }

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
        let Some(entry_idx) = self.thread.user_entry_index(n) else {
            return;
        };
        // A user turn is never collapsed and always renders, so the n-th user
        // entry is the n-th user child — no separate ordinal map to keep in step.
        let Some(child_ix) = self.row_of_entry(entry_idx) else {
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
                if let Some(ix) = this
                    .thread
                    .user_entry_index(n)
                    .and_then(|e| this.row_of_entry(e))
                {
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
    /// subscription, which has no `Window`. Reads the row list rebuilt each
    /// render.
    pub(super) fn scroll_to_entry(&mut self, entry_idx: usize, cx: &mut Context<Self>) {
        let Some(child_ix) = self.row_of_entry(entry_idx) else {
            return;
        };
        self.stick_to_bottom = false;
        self.list_scroll.scroll_to_item(child_ix);
        self.flash_entry = Some(entry_idx);
        self.flash_frames = FLASH_FRAMES;
        cx.notify();
    }

    /// The user-turn ordinal currently at (or just above) the top of the
    /// viewport — the anchor for prev/next navigation. Counting the user turns
    /// at or above the top child gives the ordinal of the last one to have
    /// scrolled past, which is the same answer the old sorted-array binary
    /// search produced in both its hit and miss cases. Returns 0 when nothing is
    /// scrolled or there are no user turns.
    pub(super) fn current_user_ordinal(&self) -> usize {
        let top_child = self.list_scroll.top_item();
        let rows = self.rows.borrow();
        rows.iter()
            .take(top_child.saturating_add(1))
            .filter(|r| {
                r.entry().is_some_and(|e| {
                    matches!(self.thread.entries.get(e), Some(ThreadEntry::User { .. }))
                })
            })
            .count()
            .saturating_sub(1)
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
            expander: Option<AnyElement>,
        }
        let mut rows: Vec<Row> = Vec::with_capacity(self.thread.entries.len());
        for (idx, entry) in self.thread.entries.iter().enumerate() {
            if matches!(group_plan[idx], EntryDisplay::Hide) {
                continue;
            }
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
            rows.push(Row { entry_idx: idx, el, dimmed, expander });
        }

        // The child sequence every jump / rail / find target resolves through.
        // Derived from the same `group_plan` the element loop above walked, so
        // the two cannot drift; `debug_assert` below pins that to the actual
        // elements rather than to the intent.
        *self.rows.borrow_mut() = build_rows(&self.thread.entries, &group_plan);
        debug_assert_eq!(
            self.rows.borrow().len(),
            rows.iter().map(|r| usize::from(r.el.is_some()) + usize::from(r.expander.is_some())).sum::<usize>(),
            "build_rows must produce exactly the children render pushes",
        );

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

    fn user(text: &str) -> ThreadEntry {
        ThreadEntry::User { text: text.into(), images: Vec::new(), checkpoint: None }
    }

    fn assistant(text: &str) -> ThreadEntry {
        ThreadEntry::Assistant(AssistantMessage {
            text: text.into(),
            thinking: String::new(),
        })
    }

    /// `EntryDisplay` is not `Clone`, so a run of `Show` is built rather than
    /// repeated.
    fn all_shown(n: usize) -> Vec<EntryDisplay> {
        (0..n).map(|_| EntryDisplay::Show).collect()
    }

    /// The straightforward case: every entry renders, so row index == entry
    /// index and a user turn's ordinal is just its position among user rows.
    #[test]
    fn every_entry_that_renders_gets_its_own_row() {
        let entries = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let plan = all_shown(4);
        assert_eq!(
            build_rows(&entries, &plan),
            vec![
                TranscriptRow::Entry { entry_idx: 0 },
                TranscriptRow::Entry { entry_idx: 1 },
                TranscriptRow::Entry { entry_idx: 2 },
                TranscriptRow::Entry { entry_idx: 3 },
            ],
        );
    }

    /// An assistant message with nothing streamed yet renders no child, so it
    /// takes no row and everything after it shifts up. This is the case that
    /// makes entry index and child index diverge in ordinary use.
    #[test]
    fn an_empty_assistant_takes_no_row() {
        let entries = vec![user("a"), assistant(""), user("c")];
        let plan = all_shown(3);
        assert_eq!(
            build_rows(&entries, &plan),
            vec![
                TranscriptRow::Entry { entry_idx: 0 },
                TranscriptRow::Entry { entry_idx: 2 },
            ],
        );
    }

    /// A collapsed run contributes no rows for its hidden entries and one extra
    /// row for the expander, which has no entry behind it at all.
    #[test]
    fn a_collapsed_run_hides_rows_and_adds_an_expander() {
        let entries = vec![user("a"), assistant("t1"), assistant("t2"), user("b")];
        let plan = vec![
            EntryDisplay::Show,
            EntryDisplay::ShowThenExpander { run_start: 1, hidden: 1 },
            EntryDisplay::Hide,
            EntryDisplay::Show,
        ];
        assert_eq!(
            build_rows(&entries, &plan),
            vec![
                TranscriptRow::Entry { entry_idx: 0 },
                TranscriptRow::Entry { entry_idx: 1 },
                TranscriptRow::Expander { run_start: 1 },
                TranscriptRow::Entry { entry_idx: 3 },
            ],
        );
    }

    /// An expander whose anchor rendered nothing still gets its row — the old
    /// flag-array accounting advanced the child counter for the expander
    /// independently of whether the anchor produced an element, and every index
    /// past it depends on that.
    #[test]
    fn an_expander_survives_an_anchor_that_renders_nothing() {
        let entries = vec![assistant(""), user("b")];
        let plan = vec![
            EntryDisplay::ShowThenExpander { run_start: 0, hidden: 2 },
            EntryDisplay::Show,
        ];
        assert_eq!(
            build_rows(&entries, &plan),
            vec![
                TranscriptRow::Expander { run_start: 0 },
                TranscriptRow::Entry { entry_idx: 1 },
            ],
        );
    }

    #[test]
    fn an_empty_transcript_has_no_rows() {
        assert!(build_rows(&[], &[]).is_empty());
    }
}
