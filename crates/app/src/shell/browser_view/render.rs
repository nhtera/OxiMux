//! `BrowserView` render: a compact toolbar (back / forward / reload +
//! address bar) above an anchor canvas. The canvas paint pins the native
//! webview's frame to the laid-out body bounds each frame — the webview
//! draws there natively, above the GPU canvas.

use gpui::{
    Bounds, Context, Focusable as _, InteractiveElement, IntoElement, ParentElement, Pixels,
    Render, SharedString, Styled, Window, canvas, div, px,
};
use gpui_component::{
    Icon, Sizable,
    button::{Button, ButtonVariants},
    input::{Enter as InputEnter, Input},
};

use super::BrowserView;

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        // Keep the address bar in step with the live URL while the user is
        // NOT editing it (so link-click navigations are reflected). When the
        // field has focus the user's text always wins.
        let editing = self.address.read(cx).focus_handle(cx).is_focused(window);
        if !editing && self.address.read(cx).value().as_ref() != self.url.as_str() {
            let url = self.url.clone();
            self.address
                .update(cx, |s, cx| s.set_value(SharedString::from(url), window, cx));
        }

        let nav_btn = |id: &'static str, icon: &'static str| {
            Button::new(id)
                .icon(Icon::default().path(icon))
                .ghost()
                .small()
        };

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .px(px(density.pad_panel))
            .py(px(density.pad_panel * 0.5))
            .border_b_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel)
            .child(
                nav_btn("browser-back", "icons/arrow-left.svg")
                    .on_click(cx.listener(|this, _, _window, _cx| this.go_back())),
            )
            .child(
                nav_btn("browser-forward", "icons/arrow-right.svg")
                    .on_click(cx.listener(|this, _, _window, _cx| this.go_forward())),
            )
            .child(
                nav_btn("browser-reload", "icons/refresh-cw.svg")
                    .on_click(cx.listener(|this, _, _window, _cx| this.reload())),
            )
            // Address bar — submit on Enter via `capture_action` (an ancestor
            // `on_key_down` never fires while the Input owns focus).
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .capture_action(cx.listener(|this, _: &InputEnter, _window, cx| {
                        this.submit_address(cx);
                    }))
                    .child(Input::new(&self.address).small()),
            );

        let body: gpui::AnyElement = match &self.native {
            Some(native) => {
                let native = native.clone();
                canvas(
                    |_bounds, _window, _cx| (),
                    move |bounds: Bounds<Pixels>, _: (), _window, _cx| {
                        native.set_bounds_px(bounds);
                    },
                )
                .size_full()
                .into_any_element()
            }
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .text_color(theme.fg_muted)
                .text_size(px(typography.t_body_sm))
                .child(SharedString::from(
                    "Could not create the web view on this platform.",
                ))
                .into_any_element(),
        };

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .child(toolbar)
            .child(div().flex_1().min_h(px(0.0)).w_full().child(body))
    }
}
