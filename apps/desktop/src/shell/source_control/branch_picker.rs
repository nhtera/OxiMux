//! Branch picker popover — a single primitive that serves two surfaces:
//!
//! - **Switch mode**: anchored to the toolbar branch chip; lists local
//!   branches (current marked), offers "Create branch `<typed>` from HEAD"
//!   when the typed text doesn't match any existing branch.
//! - **BaseRef mode**: anchored to the toolbar settings button; lists
//!   remote refs preceded by a "(repo default)" row that clears the
//!   per-worktree override.
//!
//! Layout pattern mirrors [`super::super::adapter_picker::AdapterPicker`]
//! (full-window overlay for click-outside dismiss, absolute-positioned
//! card anchored to a button) and the keyboard model mirrors
//! [`super::super::project_picker::ProjectPickerModal`] (arrow nav over a
//! selectable-rows list, Enter to commit, Escape to dismiss).
//!
//! Wiring to the toolbar happens in the next slice; this module is
//! intentionally standalone so the filter helper and row-builder can be
//! exercised without GPUI.

use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, px,
};
use gpui_component::input::{
    Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState, MoveDown, MoveUp,
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;

/// Width of the popover card. Wide enough for `origin/release-2026-Q2`
/// without truncation but narrow enough to fit beside the toolbar.
const CARD_WIDTH: f32 = 280.0;
/// Max card height before the rows scroll. Eight rows + filter + header
/// at the default density.
const CARD_MAX_HEIGHT: f32 = 320.0;
/// Single row height. Intentionally 2px shorter than the global
/// `density.h_overlay_item = 30` because branch lists tend to be longer
/// than action menus and benefit from packing more rows into the same
/// visible popover area. Treated as an approved exception to the
/// floating-surface chrome recipe; do not refactor toward
/// `density.h_overlay_item` without a paired UX decision.
const ROW_HEIGHT: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Vertical separator thickness between row sections.
const SEPARATOR_HEIGHT: f32 = 1.0;
/// Margin around separators so they don't visually touch row backgrounds.
const SEPARATOR_MARGIN: f32 = 4.0;

/// Which surface the picker is currently servicing. Drives row layout
/// (BaseRef prepends a "(repo default)" row; Switch appends a "Create
/// branch from HEAD" row when the query has no exact match).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerMode {
    Switch,
    BaseRef,
}

/// What the user committed to — handed back through the picker's
/// `on_pick` callback. The owner translates this into the appropriate
/// `Repository` call (Switch: `switch_branch` or
/// `create_branch+switch_branch`; BaseRef: `worktree_settings.upsert`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerOutcome {
    /// User picked an existing branch from the list.
    Branch(String),
    /// Switch mode only: query didn't match any existing branch and the
    /// user confirmed the "create from HEAD" row. The string is the
    /// trimmed query verbatim — the owner validates it as a branch
    /// name before invoking `create_branch`.
    CreateFromHead(String),
    /// BaseRef mode only: clear any per-worktree override, falling back
    /// to the repo's default base ref.
    UseRepoDefault,
}

/// Boxed handler the owner registers. Boxed (not `Rc`) because the
/// picker entity is owned by one parent and the callback fires inside
/// that parent's `Context`. `Send + 'static` matches the parent
/// `PaneActionsMenu`-style pattern.
///
/// `PickerMode` is passed alongside the outcome so the owner doesn't
/// need to re-read the picker entity to discover which surface
/// (Switch vs BaseRef) fired — the callback is invoked from
/// `BranchPicker::confirm`, which runs inside the picker's own
/// `update` and would panic on `picker.read(cx)` from a re-entrant
/// access (gpui entity_map enforces one writer at a time).
pub type OnPick =
    Box<dyn Fn(PickerOutcome, PickerMode, &mut Window, &mut App) + Send + 'static>;

/// Most branch rows shown at once. The card has a fixed max height with no
/// scroll — rows past the cap were silently CLIPPED before; the cap plus an
/// explicit overflow hint keeps every visible row reachable and tells the
/// user to narrow the filter instead of pretending the list ends.
pub const MAX_BRANCH_ROWS: usize = 12;

/// One row in the rendered display. Separators and the overflow hint are
/// non-selectable; arrow nav skips them. `Branch.is_current` drives a
/// check-mark indicator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayRow {
    Branch { name: String, is_current: bool },
    Separator,
    CreateFromHead(String),
    RepoDefault,
    /// "+N more — keep typing" hint when the filter still matches more
    /// branches than the cap shows.
    MoreHint(usize),
}

impl DisplayRow {
    /// True when the row participates in keyboard navigation and accepts
    /// Enter/click. Separators and the overflow hint are never selectable;
    /// everything else is.
    pub fn is_selectable(&self) -> bool {
        !matches!(self, DisplayRow::Separator | DisplayRow::MoreHint(_))
    }

    /// Translate a selectable row into the outcome that fires when the
    /// user commits it. Returns `None` for the non-selectable rows (which
    /// should never reach here anyway — defensive guard).
    pub fn outcome(&self) -> Option<PickerOutcome> {
        match self {
            DisplayRow::Branch { name, .. } => Some(PickerOutcome::Branch(name.clone())),
            DisplayRow::CreateFromHead(name) => Some(PickerOutcome::CreateFromHead(name.clone())),
            DisplayRow::RepoDefault => Some(PickerOutcome::UseRepoDefault),
            DisplayRow::Separator | DisplayRow::MoreHint(_) => None,
        }
    }
}

/// Case-insensitive substring filter. Returns indices into `candidates`
/// so the caller keeps a single source of truth and avoids lifetime
/// juggling with `Vec<&str>`. Empty query returns every index in order.
pub fn filter_branches(query: &str, candidates: &[String]) -> Vec<usize> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return (0..candidates.len()).collect();
    }
    let needle = trimmed.to_lowercase();
    candidates
        .iter()
        .enumerate()
        .filter(|(_, name)| name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// Compose the display rows for the picker given inputs and the current
/// query. Pure function so we can unit-test row layout for every mode ×
/// (empty / matching / non-matching) combination without spinning up
/// GPUI.
///
/// Layout invariants:
/// - BaseRef always starts with `RepoDefault`; a `Separator` follows
///   only when at least one filtered branch row would render after it.
/// - Switch appends `CreateFromHead(query)` when the trimmed query is
///   non-empty AND no candidate equals it case-sensitively. A
///   `Separator` precedes the Create row only when at least one branch
///   row was rendered above it.
pub fn build_rows(
    mode: PickerMode,
    candidates: &[String],
    current_branch: Option<&str>,
    query: &str,
) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    let filtered = filter_branches(query, candidates);

    if matches!(mode, PickerMode::BaseRef) {
        rows.push(DisplayRow::RepoDefault);
        if !filtered.is_empty() {
            rows.push(DisplayRow::Separator);
        }
    }

    for idx in filtered.iter().take(MAX_BRANCH_ROWS) {
        let name = candidates[*idx].clone();
        let is_current = current_branch.is_some_and(|c| c == name);
        rows.push(DisplayRow::Branch { name, is_current });
    }
    if filtered.len() > MAX_BRANCH_ROWS {
        rows.push(DisplayRow::MoreHint(filtered.len() - MAX_BRANCH_ROWS));
    }

    if matches!(mode, PickerMode::Switch) {
        let trimmed = query.trim();
        // Compare case-insensitively to match `filter_branches` semantics.
        // Without the fold, typing "MAIN" against an existing "main"
        // surfaces both the matched branch row AND a spurious Create row,
        // and the subsequent `create_branch("MAIN")` would collide on
        // case-insensitive file systems (default on macOS).
        let folded = trimmed.to_lowercase();
        let exact_match =
            !trimmed.is_empty() && candidates.iter().any(|c| c.to_lowercase() == folded);
        if !trimmed.is_empty() && !exact_match {
            // The overflow hint already separates the branch list from
            // what follows — a second hairline would orphan it.
            if !filtered.is_empty() && filtered.len() <= MAX_BRANCH_ROWS {
                rows.push(DisplayRow::Separator);
            }
            rows.push(DisplayRow::CreateFromHead(trimmed.to_string()));
        }
    }

    rows
}

/// Count selectable rows (everything except separators). Used by the
/// selection-cursor math to know how far Up/Down can travel.
pub fn selectable_count(rows: &[DisplayRow]) -> usize {
    rows.iter().filter(|r| r.is_selectable()).count()
}

/// Map a selectable-index (0..selectable_count) to the absolute row
/// index in `rows`. Returns `None` when `selectable_idx` is out of
/// range — defensive against stale selections after a filter change.
pub fn selectable_to_absolute(rows: &[DisplayRow], selectable_idx: usize) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.is_selectable())
        .nth(selectable_idx)
        .map(|(i, _)| i)
}

/// Wrap a selectable-index by `delta` modulo the selectable count.
/// `count == 0` returns `0` defensively — callers should bail before
/// calling with no selectable rows.
pub fn wrap_selectable(current: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let modulus = count as isize;
    let signed = (current as isize) + delta;
    let wrapped = signed.rem_euclid(modulus);
    wrapped as usize
}

/// GPUI entity. Owns its filter input state and renders a positioned
/// popover when `open == true`; renders nothing otherwise. Built once
/// and reused across opens — the owner calls `open(...)` with fresh
/// candidates each time.
pub struct BranchPicker {
    open: bool,
    mode: PickerMode,
    candidates: Vec<String>,
    current_branch: Option<String>,
    query_input: Entity<InputState>,
    query: String,
    /// Index into the SELECTABLE rows (separators do not count). Reset
    /// to 0 on open and on filter change.
    selected_idx: usize,
    focus_handle: FocusHandle,
    on_pick: OnPick,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Anchor coordinates in window pixels — set by `open()`; the
    /// owner computes them from the source button's bounds.
    left_anchor_px: f32,
    top_anchor_px: f32,
    _query_sub: Subscription,
}

impl BranchPicker {
    pub fn new(
        mode: PickerMode,
        on_pick: OnPick,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match mode {
            PickerMode::Switch => "Switch branch or create…",
            PickerMode::BaseRef => "Filter remote branches…",
        };
        let query_input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let query_sub = cx.subscribe_in(
            &query_input,
            window,
            move |me, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    me.query = input.read(cx).value().to_string();
                    me.selected_idx = 0;
                    cx.notify();
                }
            },
        );
        Self {
            open: false,
            mode,
            candidates: Vec::new(),
            current_branch: None,
            query_input,
            query: String::new(),
            selected_idx: 0,
            focus_handle: cx.focus_handle(),
            on_pick,
            theme,
            density,
            typography,
            left_anchor_px: 0.0,
            top_anchor_px: 0.0,
            _query_sub: query_sub,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn mode(&self) -> PickerMode {
        self.mode
    }

    /// Open the popover with a fresh candidate list, anchored at the
    /// given window-space coordinates. Idempotent against state from a
    /// previous open — query/selection reset, focus claimed.
    pub fn open(
        &mut self,
        candidates: Vec<String>,
        current_branch: Option<String>,
        left_anchor_px: f32,
        top_anchor_px: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.candidates = candidates;
        self.current_branch = current_branch;
        self.left_anchor_px = left_anchor_px;
        self.top_anchor_px = top_anchor_px;
        self.selected_idx = 0;
        self.open = true;
        self.query_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.query.clear();
        // Focus the filter INPUT, not the card: typing must land in the
        // input, and the card's `on_key_down` still sees arrows/Enter/
        // Escape on the bubble path (same contract as `confirm_dialog`).
        let input_focus = self.query_input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    /// Swap the picker mode at open time. Callers that share one picker
    /// entity between the branch-chip and the base-ref button flip
    /// modes via `set_mode` before `open(...)`. No-op when the mode is
    /// unchanged.
    pub fn set_mode(&mut self, mode: PickerMode) {
        self.mode = mode;
    }

    fn current_rows(&self) -> Vec<DisplayRow> {
        build_rows(
            self.mode,
            &self.candidates,
            self.current_branch.as_deref(),
            &self.query,
        )
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.current_rows();
        let count = selectable_count(&rows);
        if count == 0 {
            return;
        }
        self.selected_idx = wrap_selectable(self.selected_idx, delta, count);
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.current_rows();
        let Some(abs) = selectable_to_absolute(&rows, self.selected_idx) else {
            return;
        };
        let Some(outcome) = rows[abs].outcome() else {
            return;
        };
        // Close first so the callback's UI work isn't visually layered
        // over a still-visible popover — matches the
        // `ProjectPickerModal::finalize_recent` sequencing (C2).
        self.close(cx);
        // Pass `self.mode` so the callback never has to call
        // `picker.read(cx).mode()` — that would re-enter this entity
        // (we're still inside its `update`) and panic.
        (self.on_pick)(outcome, self.mode, window, cx);
    }
}

impl Focusable for BranchPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranchPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let left_px = self.left_anchor_px;
        let top_px = self.top_anchor_px;
        let selected_idx = self.selected_idx;
        let rows = self.current_rows();

        let mut card = div()
            .flex()
            .flex_col()
            .w(px(CARD_WIDTH))
            .max_h(px(CARD_MAX_HEIGHT))
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg()
            .track_focus(&self.focus_handle)
            // Keyboard plumbing. The focused `Input` converts nav keys into
            // its own ACTIONS before raw key listeners on ancestors run, so
            // `on_key_down` alone never hears them while the input is
            // focused:
            //   - Escape / Enter: the single-line input propagates these
            //     actions → handle them at the bubble phase.
            //   - Up / Down: the input swallows them (no propagate) →
            //     intercept at the CAPTURE phase and stop propagation.
            // The raw `on_key_down` stays for the no-input focus path
            // (e.g. focus on the card handle after a row click).
            // Known trade-off: capturing Escape preempts the input's own
            // IME-unmark path, so Escape mid-IME-composition closes the
            // picker instead of cancelling the composition. Accepted: the
            // bubble-phase alternative never receives the action in
            // practice (verified live), and a closed picker beats dead
            // keys for the common case.
            .capture_action(cx.listener(|this, _: &InputEscape, _window, cx| {
                cx.stop_propagation();
                this.close(cx);
            }))
            .capture_action(cx.listener(|this, _: &InputEnter, window, cx| {
                cx.stop_propagation();
                this.confirm(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, _window, cx| {
                cx.stop_propagation();
                this.move_selection(-1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _window, cx| {
                cx.stop_propagation();
                this.move_selection(1, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "down" => this.move_selection(1, cx),
                    "up" => this.move_selection(-1, cx),
                    "enter" => this.confirm(window, cx),
                    "escape" => this.close(cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _window, cx| {
                // Stop click-propagation inside the card so the overlay's
                // dismiss handler doesn't fire when the user clicks a row.
                cx.stop_propagation()
            })
            .child(
                div()
                    .pb(px(density.pad_overlay))
                    .child(Input::new(&self.query_input)),
            );

        if rows.is_empty() {
            card = card.child(empty_row(theme, &typography));
        } else {
            let mut selectable_cursor = 0usize;
            for row in &rows {
                let elem = match row {
                    DisplayRow::Separator => separator_row(theme),
                    DisplayRow::MoreHint(n) => more_hint_row(*n, theme, &typography),
                    other => {
                        let selected = selectable_cursor == selected_idx;
                        let idx = selectable_cursor;
                        selectable_cursor += 1;
                        rendered_row(other, selected, idx, theme, density, typography.clone(), cx)
                    }
                };
                card = card.child(elem);
            }
        }

        div()
            .absolute()
            .inset_0()
            .size_full()
            // Occlusion is load-bearing: without it GPUI hover hit-testing
            // passes through the dismiss layer and rows underneath keep
            // reacting (hover tint + revealed actions) while the picker is open.
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    this.close(cx);
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(top_px))
                    .left(px(left_px))
                    .child(card),
            )
            .into_any_element()
    }
}

fn separator_row(theme: Theme) -> AnyElement {
    div()
        .h(px(SEPARATOR_HEIGHT))
        .bg(theme.border_inactive)
        .mx(px(SEPARATOR_MARGIN))
        .my(px(SEPARATOR_MARGIN))
        .into_any_element()
}

/// Non-selectable overflow hint under the capped branch list.
fn more_hint_row(more: usize, theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING_X))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .italic()
        .child(SharedString::from(format!("+{more} more — keep typing")))
        .into_any_element()
}

fn empty_row(theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING_X))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .italic()
        .child(SharedString::from("No matches"))
        .into_any_element()
}

fn rendered_row(
    row: &DisplayRow,
    selected: bool,
    selectable_idx: usize,
    theme: Theme,
    density: Density,
    typography: Typography,
    cx: &mut Context<BranchPicker>,
) -> AnyElement {
    let (label, lead, italic) = match row {
        DisplayRow::Branch { name, is_current } => (
            SharedString::from(name.clone()),
            if *is_current {
                Some(SharedString::from("✓"))
            } else {
                None
            },
            false,
        ),
        DisplayRow::CreateFromHead(name) => (
            SharedString::from(format!("Create branch \"{name}\" from HEAD")),
            Some(SharedString::from("+")),
            true,
        ),
        DisplayRow::RepoDefault => (
            SharedString::from("(repo default)"),
            None,
            true,
        ),
        DisplayRow::Separator | DisplayRow::MoreHint(_) => {
            unreachable!("non-selectable rows filtered before rendered_row")
        }
    };
    let bg = if selected {
        theme.bg_panel_alt
    } else {
        theme.bg_overlay
    };
    let mut row_div = div()
        .id(("branch-picker-row", selectable_idx))
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .bg(bg)
        .text_size(px(typography.t_body_md))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                this.selected_idx = selectable_idx;
                this.confirm(window, cx);
            }),
        );
    if let Some(lead_str) = lead {
        row_div = row_div.child(
            div()
                .w(px(16.0))
                .text_color(theme.fg_muted)
                .child(lead_str),
        );
    } else {
        row_div = row_div.child(div().w(px(16.0)));
    }
    let label_div = if italic {
        div().italic().flex_1().child(label)
    } else {
        div().flex_1().child(label)
    };
    row_div.child(label_div).into_any_element()
}

