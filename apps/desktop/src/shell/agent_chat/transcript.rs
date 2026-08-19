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

use std::cell::Cell;
use std::rc::Rc;
use std::sync::OnceLock;

use gpui::{list, FollowMode, ListAlignment, ListSizingBehavior, ListState};

/// How far beyond the viewport the list keeps rows rendered, so a flick does not
/// paint blank. Chat rows are tall and expensive, so this is deliberately about
/// one screenful rather than the several a cheap uniform list could afford.
const OVERDRAW: f32 = 600.0;

/// The ceiling on how many frames a jump keeps correcting itself. It normally
/// stops well before this, at the fixed point — the cap only exists so a target
/// that never settles cannot repaint forever.
const REVEAL_ATTEMPTS: u8 = 4;

/// Whether the transcript renders through [`gpui::list`] — only visible rows
/// materialized — or the original `overflow_y_scroll` box that builds every
/// entry every frame.
///
/// An escape hatch, not a feature. Virtualizing rewrites how every jump, the
/// rail, the find bar and auto-follow address the transcript, and
/// `OXIMUX_LEGACY_TRANSCRIPT=1` is the way back without waiting for a release.
/// Read once per process: the two paths keep their scroll position in different
/// places, so flipping mid-session would land the user somewhere arbitrary.
pub(super) fn virtualized() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OXIMUX_LEGACY_TRANSCRIPT").is_none())
}

/// A jump that has been issued and is still converging.
#[derive(Clone, Copy)]
pub(super) struct PendingReveal {
    /// The row to bring on screen.
    row: usize,
    /// Frames left before giving up.
    attempts: u8,
    /// Consecutive frames on which the row has come out fully inside the
    /// viewport. One is not enough — see [`AgentChatView::settle_pending_reveal`].
    stable: u8,
}

/// Everything about where the transcript is scrolled.
///
/// Both halves exist at once because [`virtualized`] is a runtime switch, but
/// only one is ever driven — they hold the position in incompatible terms (a
/// pixel offset over child bounds versus a measured item index), so there is
/// nothing to keep in step and no attempt to.
pub(super) struct ScrollState {
    /// The non-virtualized box's offset. Live when NOT [`virtualized`].
    pub legacy: ScrollHandle,
    /// Scroll position + per-row height cache for [`gpui::list`]. Rows differ
    /// wildly in height — a one-line prompt, a screenshot, a folded diff — so
    /// the list measures each rather than assuming a uniform row.
    pub list: ListState,
    /// The thread revision [`Self::list`]'s height cache was last told about.
    /// Content changes without the row COUNT changing all the time — a reply
    /// streaming text, a tool result arriving — and `list()` only re-measures
    /// rows it lays out, so an off-screen row's height would otherwise go
    /// stale. See [`AgentChatView::sync_list_state`].
    pub measured_revision: Cell<u64>,
    /// A jump that is still landing. See
    /// [`AgentChatView::settle_pending_reveal`].
    pub pending_reveal: Cell<Option<PendingReveal>>,
}

impl ScrollState {
    pub fn new() -> Self {
        Self {
            legacy: ScrollHandle::new(),
            // `Top` alignment, not `Bottom`: a conversation shorter than the
            // viewport sits at the top, the way the scroll box always rendered
            // it. Following the tail is `FollowMode::Tail`'s job and works
            // under either alignment.
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            measured_revision: Cell::new(0),
            pending_reveal: Cell::new(None),
        }
    }
}

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
    /// The "N more" expander for a collapsed tool run, trailing the run's last
    /// visible card. `anchor_idx` is that card's entry index — NOT the run's
    /// first entry, which is what the expander is keyed by; both that key and
    /// the hidden count live on the anchor's
    /// [`EntryDisplay::ShowThenExpander`]. No [`ThreadEntry`] corresponds to
    /// the expander itself.
    Expander { anchor_idx: usize },
    /// Everything trailing the last entry — the pinned plan checklist, the one
    /// live-state card, and the breathing room above the composer — as a single
    /// child. Always present, even on a transcript with nothing to say, so the
    /// row list has a tail of fixed length: those cards appear and disappear on
    /// every turn boundary, and a tail that changed length would splice the list
    /// each time.
    Tail,
}

impl TranscriptRow {
    /// The entry behind this row, if any. `None` for an expander — which is the
    /// distinction every reverse lookup here turns on.
    fn entry(&self) -> Option<usize> {
        match *self {
            TranscriptRow::Entry { entry_idx } => Some(entry_idx),
            TranscriptRow::Expander { .. } | TranscriptRow::Tail => None,
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
        if matches!(plan[idx], EntryDisplay::ShowThenExpander { .. }) {
            rows.push(TranscriptRow::Expander { anchor_idx: idx });
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

    /// Reveal the row at `ix`.
    ///
    /// The two paths address scrolling differently — `ScrollHandle` by child
    /// ordinal, `ListState` by measured item — but both take a row index, which
    /// is what the row model bought.
    fn scroll_to_row(&self, ix: usize) {
        self.reveal_row_now(ix);
        // One issue is not enough, and this is not a nicety — see
        // [`Self::settle_pending_reveal`].
        self.scroll.pending_reveal.set(Some(PendingReveal {
            row: ix,
            attempts: REVEAL_ATTEMPTS,
            stable: 0,
        }));
    }

    fn reveal_row_now(&self, ix: usize) {
        if virtualized() {
            self.scroll.list.scroll_to_reveal_item(ix);
        } else {
            self.scroll.legacy.scroll_to_item(ix);
        }
    }

    /// The legacy path's auto-follow: re-pin to the bottom every frame while
    /// following, and keep re-arming while the content is still growing.
    ///
    /// All of this is what [`FollowMode::Tail`] does inside `list()`'s own
    /// layout — which is why the virtualized path has no counterpart and this
    /// runs only when it is off.
    pub(super) fn settle_legacy_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if virtualized() || !self.stick_to_bottom {
            return;
        }
        self.scroll.legacy.scroll_to_bottom();
        // The async markdown layout that follows a content change does not
        // re-run this render, so one pin lands on a too-short `content_size`.
        // Keep re-arming the follow while the scrollable extent is still
        // growing (the layout settling), so a slow/large reply is followed to
        // its true bottom; once it holds steady the counter drains and the
        // frame loop stops. Each armed frame forces a re-render that re-pins
        // to the freshly-settled height.
        let max_y = f32::from(self.scroll.legacy.max_offset().y);
        if (max_y - self.last_max_offset).abs() > 0.5 {
            self.follow_frames = FOLLOW_FRAMES;
        }
        self.last_max_offset = max_y;
        if self.follow_frames > 0 {
            self.follow_frames -= 1;
            let this = cx.entity().downgrade();
            window.on_next_frame(move |_window, cx| {
                let _ = this.update(cx, |_this, cx| cx.notify());
            });
        }
    }

    /// Keep re-issuing a jump until the scroll position it produces stops
    /// moving.
    ///
    /// A downward reveal resolves its offset by summing row heights through the
    /// target (list.rs:620), and those heights are only real for rows the list
    /// has laid out. Rows it has not are carried at whatever they last measured
    /// — including rows that changed height off-screen, which `remeasure_items`
    /// keeps as a *hint* rather than dropping to unknown (unknown would blank
    /// the scrollbar extent). So the first issue lands approximately: near
    /// enough that the rows around the target get measured, not near enough to
    /// be right. Re-issuing against those fresh measurements is what makes it
    /// exact, and each pass measures more, so it walks in.
    ///
    /// It stops at the fixed point — two passes agreeing on the position —
    /// rather than the first time the target looks visible. A target landed in
    /// view during development and then drifted 294px back out on the next
    /// layout, because the rows above it were still being measured for the first
    /// time; on screen this frame is no evidence about the next one, and the
    /// position holding still is.
    ///
    /// This replaces a single `on_next_frame` re-issue that could not tell
    /// whether it had worked and that the find bar, having no `Window`, never
    /// got at all. Costs nothing when idle.
    pub(super) fn settle_pending_reveal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.scroll.pending_reveal.get() else {
            return;
        };
        if pending.attempts == 0 || pending.row >= self.rows.borrow().len() {
            self.scroll.pending_reveal.set(None);
            return;
        }
        // The correction has to be computed AFTER this frame lays out, not here
        // — `render` runs before layout, so the heights available now are the
        // ones that produced the miss. Correcting from inside `render` was
        // measured landing ~150px short and staying there however many times it
        // repeated, because every pass read the same pre-layout tree.
        let this = cx.entity().downgrade();
        window.on_next_frame(move |_window, cx| {
            let _ = this.update(cx, |this, cx| {
                let Some(mut pending) = this.scroll.pending_reveal.get() else {
                    return;
                };
                let viewport = this.scroll.list.viewport_bounds();
                let inside = this
                    .scroll
                    .list
                    .bounds_for_item(pending.row)
                    .is_some_and(|b| b.top() >= viewport.top() && b.bottom() <= viewport.bottom());
                // Two consecutive good frames, not one. A target landed in view
                // and drifted back out on the very next layout: each reveal
                // moves the viewport, which measures rows that were only
                // estimated, which moves everything again. One good frame is a
                // coincidence; two in a row means the heights stopped moving.
                pending.stable = if inside { pending.stable + 1 } else { 0 };
                if pending.stable >= 2 || pending.attempts == 0 {
                    this.scroll.pending_reveal.set(None);
                    return;
                }
                this.reveal_row_now(pending.row);
                pending.attempts -= 1;
                this.scroll.pending_reveal.set(Some(pending));
                cx.notify();
            });
        });
    }

    /// The row at the top of the viewport.
    fn top_row(&self) -> usize {
        if virtualized() {
            self.scroll.list.logical_scroll_top().item_ix
        } else {
            self.scroll.legacy.top_item()
        }
    }

    /// Return to the live tail and re-arm auto-follow.
    pub(super) fn follow_bottom(&mut self) {
        self.stick_to_bottom = true;
        if virtualized() {
            // `Tail` snaps to the end AND re-pins on every later layout, including
            // while the last row is still growing — which is the whole of what the
            // legacy path re-arms by hand with `follow_frames`.
            self.scroll.list.set_follow_mode(FollowMode::Tail);
        } else {
            self.scroll.legacy.scroll_to_bottom();
        }
    }

    /// Whether newly-arrived content should pull the view down with it.
    ///
    /// Virtualized, this is the list's own follow state rather than a flag we
    /// maintain: `list()` drops it when the user scrolls up, picks it back up
    /// when they return to the bottom, and every `scroll_to` — so every jump —
    /// drops it too. Duplicating that in a `bool` would only create something to
    /// get out of step.
    pub(super) fn following(&self) -> bool {
        if virtualized() {
            self.scroll.list.is_following_tail()
        } else {
            self.stick_to_bottom
        }
    }

    /// Whether the transcript is scrolled to (within one card of) the bottom.
    /// `offset().y` is `<= 0` and reaches `-max_offset().y` at the very bottom,
    /// so their sum is the remaining scroll distance. Fresh views (no paint yet)
    /// report `0`, i.e. "at bottom", so the first turn follows.
    pub(super) fn is_near_bottom(&self) -> bool {
        if virtualized() {
            // Same sum, same tolerance, different source. NOT
            // `is_scrolled_to_end`, which is exact — this question is "close
            // enough that the newest turn is on screen", and an exact answer
            // would banner "jump down" at five pixels off the bottom.
            let max = self.scroll.list.max_offset_for_scrollbar().y;
            let off = self.scroll.list.scroll_px_offset_for_scrollbar().y;
            return max + off <= px(160.0);
        }
        let sh = &self.scroll.legacy;
        sh.max_offset().y + sh.offset().y <= px(160.0)
    }

    /// Jump the transcript to the `n`-th user turn (0-based ordinal among user
    /// messages) and briefly highlight it. Releases auto-follow so the jump
    /// sticks. No-op if `n` is out of range or that turn has no row. Shared
    /// primitive for the jump menu and the message rail.
    pub(super) fn scroll_to_user_ordinal(&mut self, n: usize, cx: &mut Context<Self>) {
        let Some(entry_idx) = self.thread.user_entry_index(n) else {
            return;
        };
        // A user turn is never collapsed and always renders, so the n-th user
        // entry is the n-th user child — no separate ordinal map to keep in step.
        let Some(child_ix) = self.row_of_entry(entry_idx) else {
            return;
        };
        self.stick_to_bottom = false;
        self.scroll_to_row(child_ix);
        self.flash_entry = Some(entry_idx);
        self.flash_frames = FLASH_FRAMES;
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
        self.scroll_to_row(child_ix);
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
        let top_child = self.top_row();
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
        let viewport = if virtualized() {
            self.scroll.list.viewport_bounds().size.width
        } else {
            self.scroll.legacy.bounds().size.width
        };
        let painted = f32::from(viewport) - self.density.pad_panel * 2.0;
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
            .track_scroll(&self.scroll.legacy)
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
            // An empty transcript has nothing to virtualize, so it always takes
            // the plain box — and the bar must be bound to THAT box, not to an
            // idle list state.
            return self
                .wrap_scroll(scroll.child(body), Scrollbar::vertical(&self.scroll.legacy))
                .into_any_element();
        }

        let mut scroll = scroll;

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

        // The child sequence every jump / rail / find target resolves through,
        // and the order they are pushed in — one list, so the two cannot drift.
        let mut rows = build_rows(&self.thread.entries, &group_plan);
        // Unconditional, and stated here rather than inside `build_rows` so that
        // function stays about entries and its tests keep meaning what they say.
        rows.push(TranscriptRow::Tail);
        if virtualized() {
            self.sync_list_state(&rows);
        }
        *self.rows.borrow_mut() = rows.clone();

        let body: AnyElement = if virtualized() {
            self.render_row_list(rows, group_plan, is_tool, content_w, cx)
        } else {
            // Every row is a DIRECT child of the scroll box: gpui records child
            // bounds for direct children only, which is what lets
            // `scroll_to_item` reveal an exact turn.
            for (ix, &row) in rows.iter().enumerate() {
                scroll =
                    scroll.child(self.render_row(row, ix, &group_plan, &is_tool, content_w, cx));
            }
            scroll.into_any_element()
        };
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
            .child(self.wrap_scroll(body, self.scrollbar()))
            .children(self.render_jump_list(cx))
            .children(self.render_session_detail(cx))
            .children(self.render_find_bar(cx))
            .into_any_element()
    }
    /// The element for one entry, or `None` when it renders nothing.
    ///
    /// `None` is only ever an assistant message with no text streamed yet, and
    /// [`produces_element`] says so independently — a row exists precisely when
    /// this returns `Some`, and the two must not be able to disagree.
    fn entry_element(&self, idx: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entry = self.thread.entries.get(idx)?;
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
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
        el
    }

    /// The "N more" expander that trails a collapsed tool run, if the entry at
    /// `idx` anchors one. Needs the grouping plan and the tool mask because the
    /// summary describes the cards the fold HIDES, which only the plan knows.
    fn expander_element(
        &self,
        idx: usize,
        plan: &[EntryDisplay],
        is_tool: &[bool],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match plan[idx] {
            EntryDisplay::ShowThenExpander { run_start, hidden } => {
                // Summarize the cards the collapse HIDES — what's behind the
                // fold is exactly what the user can't see for themselves. The
                // run is the consecutive tool block from `run_start`, the same
                // extent `plan_tool_grouping` collapsed.
                let collapsed: Vec<GroupedTool> = (run_start..is_tool.len())
                    .take_while(|&i| is_tool[i])
                    .filter(|&i| matches!(plan[i], EntryDisplay::Hide))
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
        }
    }

    /// Everything that trails the last entry: the pinned plan checklist, the one
    /// live-state card (disconnected / auth / working / signed-out / errored /
    /// settled), and the clearance above the composer.
    ///
    /// One child rather than the four it used to push, because the row list is
    /// now the list's item count — see [`TranscriptRow::Tail`].
    fn tail_element(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        // The same gap the scroll box put between top-level children, so folding
        // these into one child does not change the spacing between them.
        let mut col = div().flex().flex_col().gap(px(density.pad_panel * 2.0));
        // The agent's execution plan (ACP `Plan`) as a pinned checklist at the tail
        // of the transcript — one card, full-replaced on each `PlanUpdated`, kept
        // across turns until cleared. Reuses the `TodoWrite` checklist renderer.
        if let Some(entries) = self.thread.plan.as_ref().filter(|e| !e.is_empty()) {
            col = col.child(plan_panel::render_plan_entries(entries, theme, density, &typo));
        }
        // Live turn / disconnect state lives at the tail of the transcript (like
        // a native chat), NOT above the composer — so it never resizes the input.
        // Exactly one of these branches renders, which is what keeps the tail a
        // single row no matter which state the turn is in.
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
            col = col.child(error_card::error_card(&msg, theme, &typo, retry));
        } else if self.auth.is_some() {
            // The agent needs login before a session can open — the auth card is
            // the only actionable state, so it takes precedence over the working
            // indicator and the plain error/signed-out cards BELOW it. (The
            // `disconnected` error card above wins over it, but a failed
            // EnvVar-auth respawn clears `self.auth`, so the two never coexist.)
            let card = self.render_auth_card(cx);
            col = col.child(card);
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
                col = col.child(indicator);
            }
        } else if self.is_signed_out() && self.login_adapter_id().is_some() {
            // The turn settled (or errored) on an auth failure whose fix is a
            // terminal sign-in. Turn the dead-end reply into an action: a banner
            // that opens a terminal running the agent CLI, where `/login` works.
            // Takes precedence over the plain error card below since it's the
            // actionable version of the same state.
            let action = self.open_login_terminal_button(cx);
            col = col.child(login_card::login_card(self.provider_label(), theme, &typo, action));
        } else if let Some(err) = self.thread.last_error.clone() {
            // An idle turn that ended in error: surface it inline at the tail
            // with a Retry. This is the ONLY place a failure after the first
            // message becomes visible — the empty-state hint that also renders
            // `last_error` only paints when the transcript is empty.
            let retry = self.retry_button(cx);
            col = col.child(error_card::error_card(&err, theme, &typo, retry));
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
                col = col.child(summary_line(summary, theme, &typo));
            }
            if let Some(usage) = self.thread.usage.as_ref() {
                col = col.child(usage_footer(usage, theme, &typo));
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
        col.child(div().flex_none().w_full().h(tail_gap)).into_any_element()
    }

    /// One transcript row as a finished child: the element, in the centered
    /// reading column, with the staged-edit dim and jump flash applied.
    ///
    /// Deliberately the whole child and not just its contents — the wrapper is
    /// where the reading measure lives, and a caller that built its own wrapper
    /// would be free to get that wrong.
    fn render_row(
        &self,
        row: TranscriptRow,
        ix: usize,
        plan: &[EntryDisplay],
        is_tool: &[bool],
        content_w: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let density = self.density;
        let el = match row {
            TranscriptRow::Entry { entry_idx } => self.entry_element(entry_idx, cx),
            TranscriptRow::Expander { anchor_idx } => {
                self.expander_element(anchor_idx, plan, is_tool, cx)
            }
            TranscriptRow::Tail => Some(self.tail_element(cx)),
        };
        // The reading column is centered by a full-width row rather than by the
        // container: a list gives its items the full width and has no
        // `items_center` to offer, and doing it per row keeps both paths
        // identical instead of nearly so. The leading margin reproduces the
        // scroll box's `gap`, which is spacing BETWEEN children — hence not on
        // the first.
        let mut outer = div().flex().flex_row().justify_center().w_full();
        if ix > 0 {
            // Padding, not margin. A margin on a list item's ROOT element sits
            // outside the box `layout_as_root` measures, so the list would cache
            // a height short by the gap on every row and every jump would land
            // proportionally wrong. Padding is inside the box and is measured.
            outer = outer.pt(px(density.pad_panel * 2.0));
        }
        // A row with no element cannot happen — `build_rows` only emits rows for
        // things that render — but an empty child beats a panic if it ever does.
        let Some(el) = el else {
            return outer.into_any_element();
        };
        let mut wrap = transcript_column(content_w).child(el);
        if let TranscriptRow::Entry { entry_idx } = row {
            if self.is_pending_edit_dimmed(entry_idx) {
                // A staged edit dims the messages it will remove on send.
                wrap = wrap.opacity(0.4);
            }
            // A jumped-to turn briefly tints its wrapper (whole-row highlight),
            // fading with the frame counter so it settles rather than snaps.
            if self.flash_entry == Some(entry_idx) {
                let a = (self.flash_frames as f32 / FLASH_FRAMES as f32).clamp(0.0, 1.0);
                wrap = wrap
                    .rounded(px(density.r_card))
                    .bg(self.theme.focus_ring.opacity(0.16 * a));
            }
        }
        outer.child(wrap).into_any_element()
    }


    /// The scrollbar for whichever scroll state is driving the transcript.
    fn scrollbar(&self) -> Scrollbar {
        if virtualized() {
            Scrollbar::vertical(&self.scroll.list)
        } else {
            Scrollbar::vertical(&self.scroll.legacy)
        }
    }

    /// Tell the height cache what changed since the last frame.
    ///
    /// Two separate staleness problems, and missing either one puts every jump
    /// target past the stale row at the wrong offset — `scroll_to_reveal_item`
    /// resolves a position by summing measured heights through the target.
    ///
    /// **Rows moved.** Diffing against the previous list and splicing the
    /// difference — rather than `reset`ting — is what keeps the scroll position:
    /// `reset` drops it entirely, and rows move on every turn. The common cases
    /// (a turn appended, a tool run folded, a rewind shrinking the list) all
    /// fall out of the same common-prefix diff.
    ///
    /// **A row changed height without moving.** A reply streaming text, a tool
    /// result landing. `list()` re-renders and re-measures every VISIBLE row on
    /// each layout, so this is invisible until the row is off-screen — scroll up
    /// to read history while a reply streams below and its cached height stops
    /// tracking. Marking the range for remeasure keeps the old height as a hint,
    /// so nothing flickers and the scrollbar extent stays sane; the row is
    /// re-measured when it next lays out.
    ///
    /// The remeasure range is the whole list rather than the rows we think
    /// changed, because the thread reports THAT it mutated and not where. It is
    /// cheap for the same reason the staleness was invisible: an unmeasured row
    /// outside the overdraw band is never laid out, so this only re-measures
    /// what was going to be rendered anyway plus the overdraw.
    fn sync_list_state(&self, rows: &[TranscriptRow]) {
        let old = self.rows.borrow();
        let common = old.iter().zip(rows).take_while(|(a, b)| a == b).count();
        if common != old.len() || common != rows.len() {
            self.scroll.list.splice(common..old.len(), rows.len() - common);
        }

        let revision = self.thread.revision();
        if self.scroll.measured_revision.replace(revision) != revision {
            self.scroll.list.remeasure_items(0..rows.len());
        }
    }

    /// The virtualized transcript body: only the rows on screen (plus an
    /// overdraw band) are ever built.
    ///
    /// The closure outlives this frame, so it captures the row list and the
    /// grouping it was built against rather than reaching back for them — a row
    /// rendered against a newer grouping than it was indexed under would draw
    /// the wrong entry.
    fn render_row_list(
        &self,
        rows: Vec<TranscriptRow>,
        plan: Vec<EntryDisplay>,
        is_tool: Vec<bool>,
        content_w: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let density = self.density;
        let weak = cx.entity().downgrade();
        let rows = Rc::new(rows);
        let plan = Rc::new(plan);
        let is_tool = Rc::new(is_tool);
        list(self.scroll.list.clone(), move |ix, _window, cx| {
            let (Some(view), Some(&row)) = (weak.upgrade(), rows.get(ix)) else {
                return div().into_any_element();
            };
            let (plan, is_tool) = (plan.clone(), is_tool.clone());
            view.update(cx, |this, cx| {
                this.render_row(row, ix, &plan, &is_tool, content_w, cx)
            })
        })
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .size_full()
        // Both min-* zeroings carry the same weight here as on the scroll box —
        // see `wrap_scroll`.
        .min_h(px(0.0))
        .min_w_0()
        .px(px(density.pad_panel))
        .py(px(density.pad_panel))
        .into_any_element()
    }

    /// Wrap the scrolling transcript box in a positioned container and overlay a
    /// fading scrollbar, which the caller binds to whichever scroll state is
    /// actually driving the box — they are different types on the two paths, and
    /// a bar bound to the idle one is a bar that never moves. It paints on the
    /// container's right edge, auto-hides when the content fits, and — being a
    /// `Normal` hitbox gated to its own 16px strip — never blocks clicks on the
    /// messages, tool cards, or Allow/Reject rows beneath it.
    pub(super) fn wrap_scroll(&self, scroll_box: impl IntoElement, bar: Scrollbar) -> gpui::Div {
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
            .child(bar)
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
    use gpui::{size, TestAppContext, VisualTestContext};
    use oximux_agents::thread::ThreadEvent;
    use oximux_agents::thread::StubConnection;

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
                TranscriptRow::Expander { anchor_idx: 1 },
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
                TranscriptRow::Expander { anchor_idx: 0 },
                TranscriptRow::Entry { entry_idx: 1 },
            ],
        );
    }

    #[test]
    fn an_empty_transcript_has_no_rows() {
        assert!(build_rows(&[], &[]).is_empty());
    }

    /// A conversation deep enough that most of it is off-screen: `n` user turns,
    /// each answered, with the reply lengths varied so no two rows share a
    /// height. Uniform rows would let an off-by-one land on the right pixel by
    /// luck, which is exactly the bug these assertions exist to catch.
    fn seeded_view(
        n: usize,
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<AgentChatView>, VisualTestContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        window
            .update(cx, |view, _window, cx| {
                for i in 0..n {
                    view.thread.push_user_message_with_images(
                        format!("question {i}"),
                        Vec::new(),
                    );
                    view.on_event(
                        ThreadEvent::AssistantText(
                            format!("reply {i}\n").repeat(1 + i % 7),
                        ),
                        cx,
                    );
                    view.on_event(
                        ThreadEvent::TurnEnded {
                            result: None,
                            usage: None,
                            is_error: false,
                            turn_diff: None,
                        },
                        cx,
                    );
                }
            })
            .unwrap();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        // Nothing has bounds until the window lays out, and every assertion here
        // is about measured heights.
        vcx.simulate_resize(size(px(1000.0), px(600.0)));
        vcx.run_until_parked();
        (window, vcx)
    }

    /// The list is told about exactly the rows that exist. If this drifts, every
    /// index-based assertion below is meaningless and the real UI renders blanks
    /// past the end.
    #[gpui::test]
    async fn the_list_item_count_tracks_the_row_list(cx: &mut TestAppContext) {
        if !virtualized() {
            return;
        }
        let (window, mut vcx) = seeded_view(12, cx);
        window
            .update(&mut vcx.cx, |view, _window, _cx| {
                assert_eq!(view.scroll.list.item_count(), view.rows.borrow().len());
                // 12 turns -> 24 entry rows + the tail.
                assert_eq!(view.rows.borrow().len(), 25);
            })
            .unwrap();
    }

    /// The point of the phase: jumping to a user turn lands on that turn's row,
    /// not near it. `scroll_to_reveal_item` resolves a pixel offset by summing
    /// measured heights through the target, so this fails the moment the height
    /// cache is out of step with what is actually rendered.
    #[gpui::test]
    async fn a_jump_lands_on_the_target_row(cx: &mut TestAppContext) {
        if !virtualized() {
            return;
        }
        let (window, mut vcx) = seeded_view(12, cx);
        for ordinal in [11usize, 6, 0, 9] {
            window
                .update(&mut vcx.cx, |view, _window, cx| {
                    view.scroll_to_user_ordinal(ordinal, cx);
                })
                .unwrap();
            vcx.run_until_parked();
            window
                .update(&mut vcx.cx, |view, _window, _cx| {
                    let want = view
                        .thread
                        .user_entry_index(ordinal)
                        .and_then(|e| view.row_of_entry(e))
                        .expect("the turn has a row");
                    let top = view.scroll.list.logical_scroll_top().item_ix;
                    // Reveal, not scroll-to-top: a target already on screen does
                    // not move, so the assertion is "at or above the target",
                    // with the target within a screenful below.
                    assert!(
                        top <= want,
                        "jump to user turn {ordinal} put row {want} above the viewport top {top}",
                    );
                })
                .unwrap();
        }
    }

    /// How far a jump can miss when a row above the target changed height while
    /// off-screen — the case the plan flagged as not deferrable.
    ///
    /// `scroll_to_reveal_item` resolves a downward jump by summing row heights
    /// through the target (list.rs:620), and rows the list has never laid out
    /// are carried at an ESTIMATE: `remeasure_items` deliberately keeps the last
    /// measurement as a hint rather than dropping to unknown, since an unknown
    /// height blanks the scrollbar extent. So the plan's premise — wire
    /// `remeasure_items` and jumps land correctly — does not hold. It makes the
    /// row re-measure when next laid out; it does not fix the sum for a row that
    /// is never laid out.
    ///
    /// What does fix it is re-issuing the reveal after a layout, which
    /// `settle_pending_reveal` does. That correction runs on `on_next_frame`,
    /// and **this harness does not drive frames** — `run_until_parked` leaves
    /// those callbacks unrun — so what this test measures is the UNCORRECTED
    /// landing. It therefore asserts the bound rather than exactness: the miss
    /// must stay within one viewport, which is what proves the sums are merely
    /// estimated rather than broken. A row model that lost track of an entry
    /// misses by the whole transcript and fails here.
    ///
    /// Verifying the correction itself needs a frame-driving harness or a
    /// running app; it is on the phase's manual checklist.
    #[gpui::test]
    async fn a_row_that_grew_off_screen_misses_a_later_jump_by_at_most_a_viewport(cx: &mut TestAppContext) {
        if !virtualized() {
            return;
        }
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        // Turn 0 opens a tool call and leaves it pending; the turns after it
        // push it far above the viewport.
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message_with_images("run it", Vec::<ChatImage>::new());
                view.on_event(
                    ThreadEvent::ToolCallStarted {
                        id: "t0".into(),
                        name: "Bash".into(),
                        input: serde_json::json!({ "command": "sleep 1" }),
                    },
                    cx,
                );
                for i in 0..12 {
                    view.thread
                        .push_user_message_with_images(format!("question {i}"), Vec::<ChatImage>::new());
                    view.on_event(
                        ThreadEvent::AssistantText(format!("reply {i}\n").repeat(1 + i % 5)),
                        cx,
                    );
                }
            })
            .unwrap();
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_resize(size(px(1000.0), px(600.0)));
        vcx.run_until_parked();

        // Read history at the very top. The pending tool card is up here; the
        // jump target is far below.
        window
            .update(&mut vcx.cx, |view, _window, cx| {
                view.scroll_to_user_ordinal(0, cx);
            })
            .unwrap();
        vcx.run_until_parked();

        // Its result lands — a long one — while the row is off-screen.
        window
            .update(&mut vcx.cx, |view, _window, cx| {
                view.on_event(
                    ThreadEvent::ToolResult {
                        tool_use_id: "t0".into(),
                        content: "output line\n".repeat(200),
                        is_error: false,
                        structured: None,
                    },
                    cx,
                );
            })
            .unwrap();
        vcx.run_until_parked();

        // Jump down past it. The target must actually be on screen afterwards —
        // "the scroll top is at or above it" would also be true of a jump that
        // landed a thousand pixels short.
        window
            .update(&mut vcx.cx, |view, _window, cx| {
                view.scroll_to_user_ordinal(12, cx);
            })
            .unwrap();
        vcx.run_until_parked();
        window
            .update(&mut vcx.cx, |view, _window, _cx| {
                let want = view
                    .thread
                    .user_entry_index(12)
                    .and_then(|e| view.row_of_entry(e))
                    .expect("the last turn has a row");
                let viewport = view.scroll.list.viewport_bounds();
                let bounds = view.scroll.list.bounds_for_item(want).unwrap_or_else(|| {
                    panic!("row {want} was not rendered at all after jumping to it")
                });
                let miss = bounds.top() - viewport.bottom();
                assert!(
                    bounds.top() >= viewport.top() && miss < viewport.size.height,
                    "row {want} landed {miss:?} past the fold of {viewport:?} — \
                     more than a viewport off means the heights are not merely \
                     estimated, they are wrong",
                );
            })
            .unwrap();
    }

    /// A rewind shrinks the row list mid-conversation. `splice` must keep the
    /// list's item count honest — a stale count renders past the end.
    #[gpui::test]
    async fn shrinking_the_transcript_keeps_the_list_honest(cx: &mut TestAppContext) {
        if !virtualized() {
            return;
        }
        let (window, mut vcx) = seeded_view(12, cx);
        window
            .update(&mut vcx.cx, |view, _window, _cx| {
                view.thread.entries.truncate(7);
            })
            .unwrap();
        vcx.simulate_resize(size(px(1000.0), px(601.0)));
        vcx.run_until_parked();
        window
            .update(&mut vcx.cx, |view, _window, _cx| {
                assert_eq!(view.scroll.list.item_count(), view.rows.borrow().len());
                assert_eq!(view.rows.borrow().len(), 8, "7 entries + the tail");
            })
            .unwrap();
    }
}
