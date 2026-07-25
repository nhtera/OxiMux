//! Display-options dropdown for the "Projects" header — group-by, sort, card
//! layout, and collapse-all, consolidated behind the header's options icon.
//!
//! Mirrors the `dashboard_status_menu` contract: a full-window occluding
//! backdrop (closes on outside click) with the menu card pinned at the trigger
//! anchor. Selections apply to the owning `LeftRail` through a `WeakEntity` so
//! the rail's state stays the single source of truth. Toggle selections keep
//! the menu open (the snapshot updates so checks track live state); collapse-all
//! is an action that closes the menu.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::LeftRail;
use crate::shell::left_rail::workspace_list_render::{WorkspaceGroupMode, WorkspaceSortMode};

/// Card width — wide enough for the longest sort label plus a trailing check.
const MENU_WIDTH: f32 = 220.0;
/// One selectable row's height — matches the other rail popovers.
const ROW_H: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Y offset below the trigger so the menu doesn't overlap it.
const ANCHOR_Y_OFFSET: f32 = 4.0;

/// Snapshot of the rail's display state, rendered as the menu's checked items.
#[derive(Debug, Clone, Copy)]
struct MenuState {
    sort_mode: WorkspaceSortMode,
    group_mode: WorkspaceGroupMode,
    compact: bool,
}

/// What a check-marked radio row does when clicked (sort order, card layout).
type RadioClick = Box<dyn Fn(&mut WorkspaceOptionsMenu, &mut Context<WorkspaceOptionsMenu>)>;

pub struct WorkspaceOptionsMenu {
    /// `None` when closed; `Some` carries the live snapshot plus the
    /// screen-pixel anchor for the popover.
    open_for: Option<(MenuState, f32, f32)>,
    rail: WeakEntity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl WorkspaceOptionsMenu {
    /// `true` while the menu is pinned open.
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        rail: WeakEntity<LeftRail>,
    ) -> Self {
        Self {
            open_for: None,
            rail,
            theme,
            density,
            typography,
        }
    }

    /// Open the menu anchored at (x, y), showing the current display state as
    /// the checked items.
    pub(crate) fn open(
        &mut self,
        sort_mode: WorkspaceSortMode,
        group_mode: WorkspaceGroupMode,
        compact: bool,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.open_for = Some((
            MenuState {
                sort_mode,
                group_mode,
                compact,
            },
            x,
            y + ANCHOR_Y_OFFSET,
        ));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        cx.notify();
    }

    fn set_sort(&mut self, mode: WorkspaceSortMode, cx: &mut Context<Self>) {
        // Only mirror into the local snapshot if the rail write actually
        // committed; otherwise the menu would show a checkmark the rail never
        // applied (e.g. the rail entity was dropped mid-session).
        if self.rail.update(cx, |r, cx| r.set_sort_mode(mode, cx)).is_ok()
            && let Some((state, _, _)) = &mut self.open_for
        {
            state.sort_mode = mode;
        }
        cx.notify();
    }

    fn set_group(&mut self, mode: WorkspaceGroupMode, cx: &mut Context<Self>) {
        if self.rail.update(cx, |r, cx| r.set_group_mode(mode, cx)).is_ok()
            && let Some((state, _, _)) = &mut self.open_for
        {
            state.group_mode = mode;
        }
        cx.notify();
    }

    fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        let committed = self
            .rail
            .update(cx, |r, cx| {
                if r.compact_cards() != compact {
                    r.toggle_compact_cards(cx);
                }
            })
            .is_ok();
        if committed && let Some((state, _, _)) = &mut self.open_for {
            state.compact = compact;
        }
        cx.notify();
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let _ = self.rail.update(cx, |r, cx| r.toggle_collapse_all(cx));
        self.close(cx);
    }
}

/// A small section title above a group of rows.
fn section_label(text: &'static str, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .px(px(ROW_PADDING_X))
        .pt(px(6.0))
        .pb(px(2.0))
        .text_size(px(typography.t_sub_label))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_subtle)
        .child(text)
}

impl Render for WorkspaceOptionsMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((state, x, y)) = self.open_for else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        // A check-marked radio row (used for sort + card layout).
        let radio_row = |id: &'static str,
                         label: &str,
                         is_active: bool,
                         on_click: RadioClick,
                         cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(density.gap_inline))
                .h(px(ROW_H))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.hover_overlay))
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_base)
                .child(div().flex_1().whitespace_nowrap().child(label.to_string()))
                .child(
                    div()
                        .w(px(12.0))
                        .flex_shrink_0()
                        .text_color(theme.fg_base)
                        .child(if is_active { "✓" } else { "" }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        on_click(this, cx);
                    }),
                )
        };

        let mut card = div()
            .flex()
            .flex_col()
            .py(px(density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        // ── Group by ── (segmented None | Project)
        card = card.child(section_label("Group by", theme, &typography));
        let group_seg = |id: &'static str,
                         label: &'static str,
                         mode: WorkspaceGroupMode,
                         active: bool,
                         cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .h(px(ROW_H - 6.0))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .text_size(px(typography.t_body_md))
                .when(active, |s| s.bg(theme.hover_overlay).text_color(theme.fg_base))
                .when(!active, |s| {
                    s.text_color(theme.fg_subtle).hover(|s| s.text_color(theme.fg_base))
                })
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.set_group(mode, cx);
                    }),
                )
        };
        card = card.child(
            div()
                .flex()
                .flex_row()
                .gap(px(density.gap_inline))
                .px(px(ROW_PADDING_X))
                .pb(px(4.0))
                .child(group_seg(
                    "group-none",
                    "None",
                    WorkspaceGroupMode::Flat,
                    state.group_mode == WorkspaceGroupMode::Flat,
                    cx,
                ))
                .child(group_seg(
                    "group-project",
                    "Project",
                    WorkspaceGroupMode::Project,
                    state.group_mode == WorkspaceGroupMode::Project,
                    cx,
                )),
        );

        // ── Sort by ── (5 radio rows)
        card = card.child(section_label("Sort by", theme, &typography));
        for mode in WorkspaceSortMode::ALL {
            let active = state.sort_mode == mode;
            card = card.child(radio_row(
                match mode {
                    WorkspaceSortMode::Name => "sort-name",
                    WorkspaceSortMode::Smart => "sort-smart",
                    WorkspaceSortMode::Recent => "sort-recent",
                    WorkspaceSortMode::Project => "sort-project",
                    WorkspaceSortMode::Manual => "sort-manual",
                },
                mode.label(),
                active,
                Box::new(move |this, cx| this.set_sort(mode, cx)),
                cx,
            ));
        }

        // ── Card layout ── (Detailed | Compact radio rows)
        card = card.child(section_label("Card layout", theme, &typography));
        card = card.child(radio_row(
            "layout-detailed",
            "Detailed",
            !state.compact,
            Box::new(|this, cx| this.set_compact(false, cx)),
            cx,
        ));
        card = card.child(radio_row(
            "layout-compact",
            "Compact",
            state.compact,
            Box::new(|this, cx| this.set_compact(true, cx)),
            cx,
        ));

        // ── Collapse all ── (action; disabled in flat mode where there are no
        // groups to collapse)
        let can_collapse = state.group_mode == WorkspaceGroupMode::Project;
        let mut collapse = div()
            .id("options-collapse-all")
            .flex()
            .flex_row()
            .items_center()
            .h(px(ROW_H))
            .px(px(ROW_PADDING_X))
            .mt(px(2.0))
            .rounded(px(density.r_xs))
            .text_size(px(typography.t_body_md));
        if can_collapse {
            collapse = collapse
                .cursor_pointer()
                .text_color(theme.fg_base)
                .hover(|s| s.bg(theme.hover_overlay))
                .child("Collapse / expand all")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| this.collapse_all(cx)),
                );
        } else {
            collapse = collapse
                .text_color(theme.fg_subtle)
                .child("Collapse / expand all");
        }
        card = card.child(collapse);

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(MENU_WIDTH))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
            .into_any_element()
    }
}
