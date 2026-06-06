//! Rendering for [`SettingsModal`]: the full-window overlay, the modal
//! card (left nav + active pane body), and pane-body dispatch. State +
//! persistence live in the parent module.

use gpui::{
    AnyElement, AppContext, Context, DragMoveEvent, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled,
    Window, div, px,
};
use oximux_settings::{Density, Typography};

use super::{
    CARD_HEIGHT, CARD_WIDTH, SettingsModal, SettingsPane, layout, nav, pane_about, pane_agents,
    pane_keybindings, pane_terminal,
};

/// Drag payload marker for the title-bar move gesture. Mirrors the floating
/// terminal's pattern: a zero-size ghost preview while the card itself moves
/// on each `on_drag_move`.
struct TitleDrag;

struct DragGhost;
impl Render for DragGhost {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

impl SettingsModal {
    fn render_body(
        &self,
        density: Density,
        typography: &Typography,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let query = self.search_text(cx);

        // Non-empty query → global results across every pane (like a native
        // settings finder), tagged with their source pane.
        if !query.is_empty() {
            let groups = vec![
                (
                    SettingsPane::Terminal.label(),
                    pane_terminal::entries(self, theme, density, typography, cx),
                ),
                (
                    SettingsPane::Agents.label(),
                    pane_agents::entries(self, theme, density, typography, cx),
                ),
                (
                    SettingsPane::Keybindings.label(),
                    pane_keybindings::entries(theme, typography),
                ),
                (
                    SettingsPane::Appearance.label(),
                    pane_about::appearance_entries(theme, typography),
                ),
                (
                    SettingsPane::About.label(),
                    pane_about::about_entries(theme, typography),
                ),
            ];
            return layout::search_results(&query, groups, theme, typography);
        }

        match self.selected {
            SettingsPane::Terminal => pane_terminal::render(self, theme, density, typography, cx),
            SettingsPane::Agents => pane_agents::render(self, theme, density, typography, cx),
            SettingsPane::Keybindings => {
                pane_keybindings::render(&query, theme, density, typography)
            }
            SettingsPane::Appearance => pane_about::render_appearance(&query, theme, typography),
            SettingsPane::About => pane_about::render_about(&query, theme, typography),
        }
    }
}

impl Render for SettingsModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let selected = self.selected;
        let searching = !self.search_text(cx).is_empty();
        let pos = self.resolved_pos(window);

        let header = div()
            .flex()
            .items_center()
            .h(px(44.0))
            .px(px(density.pad_panel))
            .text_size(px(typography.t_body_lg))
            .font_weight(typography.w_semibold)
            .text_color(theme.fg_base)
            .child(if searching {
                "Search results"
            } else {
                selected.label()
            });

        let body = div()
            .id("settings-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(density.pad_panel))
            .pb(px(density.pad_panel))
            .child(self.render_body(density, &typography, cx));

        let body_col = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(header)
            .child(div().w_full().h(px(1.0)).bg(theme.border_inactive))
            .child(body);

        // Window-level title bar: "Settings" caption on the left, a close (×)
        // affordance on the right. Doubles as the drag handle — grabbing it
        // moves the whole card (the close button stops propagation so it
        // never starts a drag).
        let title_bar = div()
            .id("settings-title")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.0))
            .px(px(density.pad_panel))
            .bg(theme.bg_panel)
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _window, _cx| {
                    this.drag_grab = Some(ev.position - pos);
                }),
            )
            .on_drag(TitleDrag, |_payload, _offset, _window, cx| {
                cx.new(|_| DragGhost)
            })
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child("Settings"),
            )
            .child(
                div()
                    .id("settings-close")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .rounded(px(density.r_xs))
                    .text_size(px(typography.t_body_md))
                    .text_color(theme.fg_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _window, cx| {
                            cx.stop_propagation();
                            this.close(cx);
                        }),
                    )
                    .child("×"),
            );

        // Nav column + body sit in a row beneath the title bar.
        let content_row = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(nav::render_nav(
                selected,
                self.search_input.as_ref(),
                theme,
                density,
                &typography,
                cx,
            ))
            .child(body_col);

        let card = div()
            .id("settings-card")
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .flex()
            .flex_col()
            .w(px(CARD_WIDTH))
            .h(px(CARD_HEIGHT))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            // Clip the edge-to-edge nav/header backgrounds to the rounded
            // corners (otherwise the nav column's square corners poke out).
            .overflow_hidden()
            // Lift the card off the workspace, matching the other dialogs.
            .shadow_lg()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() != "escape" {
                    return;
                }
                // Esc clears an active filter first, then closes on a second
                // press — so search never traps the modal open.
                if let Some(input) = this.search_input.clone()
                    && !input.read(cx).value().is_empty()
                {
                    input.update(cx, |s, cx| s.set_value("", window, cx));
                    cx.notify();
                    return;
                }
                this.close(cx);
            }))
            .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                // Stop the click from bubbling to the overlay's click-outside
                // dismiss. An empty handler does NOT stop propagation, so
                // without this every control click would close the modal.
                cx.stop_propagation();
            })
            // Move: fired while the title-bar drag is active. Keep the grab
            // point under the cursor by subtracting the captured offset.
            .on_drag_move::<TitleDrag>(cx.listener(
                |this, ev: &DragMoveEvent<TitleDrag>, window, cx| {
                    if let Some(grab) = this.drag_grab {
                        let p = ev.event.position;
                        this.set_pos(f32::from(p.x - grab.x), f32::from(p.y - grab.y), window, cx);
                    }
                },
            ))
            // Drop the grab offset on release so a stale value can't leak into
            // a later gesture.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, _cx| {
                    this.drag_grab = None;
                }),
            )
            .child(title_bar)
            .child(div().w_full().h(px(1.0)).bg(theme.border_inactive))
            .child(content_row);

        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.close(cx)),
            )
            .child(card)
            .into_any_element()
    }
}
