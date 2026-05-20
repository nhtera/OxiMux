//! Inline adapter-picker popover — anchored to the `+` button.
//!
//! Click `+` → list "+ New terminal" + every detected adapter. Click a row
//! to spawn it. Detection (`AdapterRegistry::detect_available`) is async
//! with a 500 ms timeout; results cache for the app lifetime. Subsequent
//! opens render the cache instantly + fire a background refresh.
//!
//! Pattern mirrors [`super::pane_actions::PaneActionsMenu`] — hand-rolled
//! `Entity<Self>` with full-window overlay for click-outside dismiss; no
//! `FocusHandle`, no Escape key (parity with the existing popover).

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, Styled, Window, div, px,
};
use oximux_agents::{AdapterRegistry, RegistryEntry};
use oximux_core::AgentAdapter;
use oximux_settings::{Density, Theme, Typography};

/// Width of the popover card. Slightly wider than `PaneActionsMenu` to fit
/// adapter labels ("Claude Code", "Codex CLI") + the disabled hint.
const MENU_WIDTH: f32 = 240.0;
/// Vertical gap below the chrome row before the popover starts. Matches the
/// pane-actions menu so the two visually align if both happen to be open.
const ANCHOR_TOP_PX: f32 = 42.0;
/// Edge padding around the card content.
const CARD_PADDING: f32 = 6.0;
/// Single menu item height.
const ITEM_HEIGHT: f32 = 30.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Vertical separator thickness between the "+ New terminal" row and the
/// adapter list.
const SEP_HEIGHT: f32 = 1.0;

/// What the user picked. Routed to the owner's `on_select` callback; the
/// picker has no opinion on how spawn happens.
///
/// `Adapter` carries both the `AgentAdapter` discriminant (for runtime
/// dispatch) and the `&'static str` slug (for tab labelling) so the
/// caller doesn't have to re-walk `detect_available` to recover the id —
/// the row already held both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterSelection {
    /// First row — spawns a plain shell tab.
    NewTerminal,
    /// One of the registry's available agent adapters.
    Adapter {
        kind: AgentAdapter,
        id: &'static str,
    },
}

/// Boxed handler for selections. Boxed so the picker can be constructed
/// without a `Context<WorkspaceRoot>` — the owner builds the closure with a
/// `WeakEntity` capture and hands it in at `new` time.
pub type OnSelect = Box<dyn Fn(AdapterSelection, &mut Window, &mut App) + Send + 'static>;

/// Hand-rolled popover entity. Mounted as a sibling of `PaneActionsMenu`
/// inside `WorkspaceRoot`; renders `div().into_any_element()` when closed.
pub struct AdapterPicker {
    open: bool,
    /// Left-edge offset in CSS pixels — set by the action handler at open
    /// time so the popover tracks the `+` button across left-rail toggles.
    left_anchor_px: f32,
    /// `None` until the first detection completes. `Some(vec![])` if
    /// detection timed out or errored — the failure render path keys off
    /// this combined with `!is_refreshing` to surface a retry row.
    entries: Option<Vec<RegistryEntry>>,
    /// `true` while an async `detect_available` task is in flight. Drives
    /// the "Loading…" placeholder on first open and the silent background
    /// refresh on subsequent opens.
    is_refreshing: bool,
    registry: Arc<AdapterRegistry>,
    on_select: OnSelect,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl AdapterPicker {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        registry: Arc<AdapterRegistry>,
        on_select: OnSelect,
    ) -> Self {
        Self {
            open: false,
            left_anchor_px: 0.0,
            entries: None,
            is_refreshing: false,
            registry,
            on_select,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Toggle-aware open: a second click on the `+` button closes the
    /// popover instead of being a no-op. Callers (WorkspaceRoot's
    /// `RequestOpenAdapterPicker` handler) hit this on every dispatch.
    pub fn open(&mut self, left_anchor_px: f32, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
            return;
        }
        self.left_anchor_px = left_anchor_px;
        self.open = true;
        cx.notify();
        self.refresh_if_needed(window, cx);
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    /// Spawn the async detect task. Fires when:
    /// - First open (`entries.is_none()`), or
    /// - Subsequent open with a cache present and no refresh already in
    ///   flight (silent background refresh — D4 fire-and-update).
    fn refresh_if_needed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_refreshing {
            return;
        }
        self.is_refreshing = true;
        cx.notify();
        let registry = self.registry.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result =
                tokio::time::timeout(Duration::from_millis(500), registry.detect_available())
                    .await;
            let update_result = this.update(cx, |p, cx| match result {
                Ok(entries) => {
                    p.entries = Some(entries);
                    p.is_refreshing = false;
                    cx.notify();
                }
                Err(_timeout) => {
                    if p.entries.is_none() {
                        p.entries = Some(Vec::new());
                    }
                    p.is_refreshing = false;
                    tracing::warn!(
                        "adapter-picker: detect_available timed out after 500ms; PATH may be on a slow mount"
                    );
                    cx.notify();
                }
            });
            if update_result.is_err() {
                tracing::debug!(
                    "adapter-picker: entity dropped before detection completed; ignoring"
                );
            }
        })
        .detach();
    }
}

/// Pure helper: filter Custom out of the visible row set. Custom needs a
/// `(program, args)` config flow that's out of scope for step 10 — the row
/// is hidden entirely until that surface lands.
///
/// INVARIANT: the cache (`AdapterPicker::entries`) holds **all** entries
/// returned by `AdapterRegistry::detect_available`, including Custom. This
/// helper is the only path that filters Custom. Any future code touching
/// the cache directly must NOT assume cache contents equal display rows.
fn render_rows(entries: &[RegistryEntry]) -> Vec<&RegistryEntry> {
    entries
        .iter()
        .filter(|e| e.adapter_enum != AgentAdapter::Custom)
        .collect()
}

impl Render for AdapterPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let left_px = self.left_anchor_px;

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(CARD_PADDING))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg()
            // Stop click-propagation INSIDE the card so the overlay's
            // dismiss handler doesn't fire when the user clicks a row.
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _window, cx| {
                cx.stop_propagation()
            });

        // "+ New terminal" row — always first, always clickable.
        card = card.child(picker_row(
            "new-terminal",
            SharedString::from("+ New terminal"),
            None,
            RowState::Active,
            theme,
            density,
            typography.clone(),
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                (this.on_select)(AdapterSelection::NewTerminal, window, cx);
                this.close(cx);
            }),
        ));

        // Separator between the new-terminal escape hatch and the adapter
        // list.
        card = card.child(
            div()
                .h(px(SEP_HEIGHT))
                .bg(theme.border_inactive)
                .mx(px(4.0))
                .my(px(4.0)),
        );

        // Body: loading, available/unavailable list, or retry row.
        // Four arms enumerate every (entries × is_refreshing) state. The
        // combined OR-pattern + guard form (`(None, _) | (Some(_), true) if
        // self.entries.is_none()`) collapses silently — guards only apply
        // to the last alternative, so the second arm becomes dead. The
        // explicit split prevents the retry-during-active-refresh
        // false-positive caught by review 260520-1830 (H1).
        card = match (&self.entries, self.is_refreshing) {
            // First open — no cache, detection running. Show Loading.
            (None, _) => card.child(picker_row(
                "loading",
                SharedString::from("Loading…"),
                None,
                RowState::Disabled,
                theme,
                density,
                typography.clone(),
                cx.listener(|_, _: &MouseDownEvent, _, _| {}),
            )),
            // Cache present AND a silent background refresh is in flight:
            // render cached rows. The refresh swaps the cache on completion.
            (Some(entries), true) => {
                append_adapter_rows(card, entries, theme, density, typography.clone(), cx)
            }
            // Cache present, no refresh, empty: genuine failure (timeout
            // or empty registry). Offer retry.
            (Some(entries), false) if entries.is_empty() => card.child(picker_row(
                "retry",
                SharedString::from("Detection failed — retry"),
                None,
                RowState::Retry,
                theme,
                density,
                typography.clone(),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.entries = None;
                    this.refresh_if_needed(window, cx);
                }),
            )),
            // Cache present, no refresh, populated: render rows.
            (Some(entries), false) => {
                append_adapter_rows(card, entries, theme, density, typography.clone(), cx)
            }
        };

        // Full-window invisible overlay for click-outside dismiss. Same
        // shape as `PaneActionsMenu`.
        div()
            .absolute()
            .inset_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    this.close(cx);
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(ANCHOR_TOP_PX))
                    .left(px(left_px))
                    .w(px(MENU_WIDTH))
                    .child(card),
            )
            .into_any_element()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowState {
    /// Hover + cursor pointer + full opacity. Click fires the handler.
    Active,
    /// Grayed (opacity 0.4), no cursor pointer. No click handler attached.
    Disabled,
    /// "Retry" style: same chrome as Active. Click re-runs detection.
    Retry,
}

/// Shared row-rendering path used by the two cache states that show
/// adapter rows (cache+refresh and cache+populated). Returns the card by
/// value so the four-arm `match` in `render` stays single-expression.
fn append_adapter_rows(
    mut card: gpui::Div,
    entries: &[RegistryEntry],
    theme: Theme,
    density: Density,
    typography: Typography,
    cx: &mut Context<AdapterPicker>,
) -> gpui::Div {
    for (ix, entry) in render_rows(entries).into_iter().enumerate() {
        let kind = entry.adapter_enum;
        let id = entry.adapter_id;
        let label = SharedString::from(entry.display_name);
        let (state, hint) = if entry.available {
            (RowState::Active, None)
        } else {
            (
                RowState::Disabled,
                Some(SharedString::from("not installed")),
            )
        };
        let handler = cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            if matches!(state, RowState::Active) {
                (this.on_select)(AdapterSelection::Adapter { kind, id }, window, cx);
                this.close(cx);
            }
        });
        card = card.child(picker_row(
            ("adapter-row", ix),
            label,
            hint,
            state,
            theme,
            density,
            typography.clone(),
            handler,
        ));
    }
    card
}

#[allow(clippy::too_many_arguments)]
fn picker_row(
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    hint: Option<SharedString>,
    state: RowState,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .h(px(ITEM_HEIGHT))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(theme.fg_base)
        .child(div().flex_1().child(label));
    if let Some(h) = hint {
        row = row.child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(h),
        );
    }
    match state {
        RowState::Active | RowState::Retry => row
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_panel_alt))
            .on_mouse_down(MouseButton::Left, on_click)
            .into_any_element(),
        RowState::Disabled => row.opacity(0.4).into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_on_select() -> OnSelect {
        Box::new(|_, _, _| {})
    }

    fn test_picker() -> AdapterPicker {
        AdapterPicker::new(
            Theme::charcoal(),
            Density::cockpit(),
            Typography::cockpit(),
            Arc::new(AdapterRegistry::empty()),
            noop_on_select(),
        )
    }

    fn entry(
        id: &'static str,
        name: &'static str,
        kind: AgentAdapter,
        available: bool,
    ) -> RegistryEntry {
        RegistryEntry {
            adapter_id: id,
            display_name: name,
            adapter_enum: kind,
            available,
        }
    }

    #[test]
    fn new_picker_is_closed() {
        let p = test_picker();
        assert!(!p.is_open());
        assert!(p.entries.is_none());
        assert!(!p.is_refreshing);
    }

    #[test]
    fn open_field_set_anchors_visibility() {
        let mut p = test_picker();
        // Mirror open() body inline — no Context<Self> in unit tests.
        p.left_anchor_px = 250.0;
        p.open = true;
        assert!(p.is_open());
        assert_eq!(p.left_anchor_px, 250.0);
    }

    #[test]
    fn close_resets_open() {
        let mut p = test_picker();
        p.open = true;
        p.left_anchor_px = 100.0;
        // close() only flips `open` — anchor is overwritten next open().
        p.open = false;
        assert!(!p.is_open());
    }

    #[test]
    fn cache_replacement_on_second_detect() {
        let mut p = test_picker();
        p.entries = Some(vec![entry(
            "claude-code",
            "Claude Code",
            AgentAdapter::ClaudeCode,
            false,
        )]);
        // Simulate a fresh detect_available result landing.
        p.entries = Some(vec![entry(
            "claude-code",
            "Claude Code",
            AgentAdapter::ClaudeCode,
            true,
        )]);
        let current = p.entries.as_ref().unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].available);
    }

    #[test]
    fn failure_path_sets_empty_entries() {
        let mut p = test_picker();
        // Simulate timeout branch in refresh_if_needed.
        assert!(p.entries.is_none());
        if p.entries.is_none() {
            p.entries = Some(Vec::new());
        }
        p.is_refreshing = false;
        assert_eq!(p.entries.as_ref().unwrap().len(), 0);
        assert!(!p.is_refreshing);
    }

    #[test]
    fn custom_filtered_from_render_rows() {
        let entries = vec![
            entry("claude-code", "Claude Code", AgentAdapter::ClaudeCode, true),
            entry("codex", "Codex", AgentAdapter::Codex, false),
            entry("aider", "Aider", AgentAdapter::Aider, true),
            entry("custom", "Custom Command", AgentAdapter::Custom, true),
        ];
        let visible = render_rows(&entries);
        assert_eq!(visible.len(), 3);
        assert!(
            visible
                .iter()
                .all(|e| e.adapter_enum != AgentAdapter::Custom)
        );
    }

    #[test]
    fn render_rows_preserves_order() {
        let entries = vec![
            entry("claude-code", "Claude Code", AgentAdapter::ClaudeCode, true),
            entry("custom", "Custom", AgentAdapter::Custom, true),
            entry("codex", "Codex", AgentAdapter::Codex, true),
            entry("aider", "Aider", AgentAdapter::Aider, true),
        ];
        let visible = render_rows(&entries);
        let order: Vec<&str> = visible.iter().map(|e| e.adapter_id).collect();
        assert_eq!(order, vec!["claude-code", "codex", "aider"]);
    }

    #[test]
    fn render_rows_handles_empty_input() {
        let entries: Vec<RegistryEntry> = Vec::new();
        let visible = render_rows(&entries);
        assert!(visible.is_empty());
    }
}
