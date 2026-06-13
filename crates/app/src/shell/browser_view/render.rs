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

use super::{BrowserView, CopyKind, PageAppearance};
use crate::actions::SendTextToActiveAgent;

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
        // On the click that focuses the address bar, hand keyboard
        // first-responder back from the webview to the GPUI surface — GPUI's
        // focus state and the native first-responder diverge once the page is
        // clicked, so without this the user's typing leaks into the page.
        if editing && !self.address_focused && let Some(native) = &self.native {
            native.focus_parent();
        }
        self.address_focused = editing;

        // Deliver a picked element to the active agent terminal. Stashed in the
        // IPC callback (which has no `Window`); dispatched here where one
        // exists, via the same action the terminal/diff views use.
        if let Some(text) = self.pending_agent_text.take() {
            window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
        }

        // Tooltip labels for the cycle-on-click profile + appearance controls.
        let appearance_label = match self.appearance {
            PageAppearance::System => "System",
            PageAppearance::Light => "Light",
            PageAppearance::Dark => "Dark",
        };
        let profile_name = self.profile_name(cx);
        let devtools_open = self.devtools_open;

        let nav_btn = |id: &'static str, icon: &'static str| {
            Button::new(id)
                .icon(Icon::default().path(icon))
                .ghost()
                .small()
        };

        // A probe button that swaps to a green check while its result is the
        // active confirmation, so a copy is never silent.
        let confirmed = self.confirmed;
        let probe_btn = move |id: &'static str, icon: &'static str, kind: CopyKind| {
            let lit = confirmed.is_some_and(|c| c.lights_button(kind));
            let path = if lit { "icons/check.svg" } else { icon };
            let mut icon_el = Icon::default().path(path);
            if lit {
                icon_el = icon_el.text_color(theme.status_ok);
            }
            Button::new(id).icon(icon_el).ghost().small()
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
            )
            // Agent-context probes — each hands pasteable page context to an AI
            // agent. The firing button briefly turns into a green check (see
            // `probe_btn`) and a "copied" pill appears so the action isn't
            // silent. Picker reads keys in the page, so it focuses the webview.
            .child(
                probe_btn("browser-pick", "icons/crosshair.svg", CopyKind::Pick)
                    .tooltip("Pick element (C copy · A → agent)")
                    .on_click(cx.listener(|this, _, _window, _cx| this.start_element_picker())),
            )
            .child(
                probe_btn("browser-shot", "icons/camera.svg", CopyKind::Screenshot)
                    .tooltip("Screenshot page (image → clipboard)")
                    .on_click(cx.listener(|this, _, _window, _cx| this.capture_screenshot(None))),
            )
            .child(
                probe_btn("browser-dom", "icons/file-code.svg", CopyKind::Dom)
                    .tooltip("Copy DOM outline (text → clipboard)")
                    .on_click(cx.listener(|this, _, _window, _cx| this.copy_dom_snapshot())),
            )
            .child(
                probe_btn("browser-console", "icons/list-tree.svg", CopyKind::Console)
                    .tooltip("Copy console log (text → clipboard)")
                    .on_click(cx.listener(|this, _, _window, _cx| this.copy_console())),
            )
            // Page controls: inspector, color-scheme override, and the
            // cookie-isolated profile (cycle-on-click + a new-profile button).
            .child({
                let mut icon = Icon::default().path("icons/wrench.svg");
                if devtools_open {
                    icon = icon.text_color(theme.status_ok);
                }
                Button::new("browser-devtools")
                    .icon(icon)
                    .ghost()
                    .small()
                    .tooltip("Toggle DevTools")
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_devtools(cx)))
            })
            .child(
                nav_btn("browser-appearance", "icons/contrast.svg")
                    .tooltip(SharedString::from(format!(
                        "Page theme: {appearance_label} (click to cycle)"
                    )))
                    .on_click(cx.listener(|this, _, _window, cx| this.cycle_appearance(cx))),
            )
            .child(
                nav_btn("browser-profile", "icons/user.svg")
                    .tooltip(SharedString::from(format!(
                        "Profile: {profile_name} (click to switch)"
                    )))
                    .on_click(cx.listener(|this, _, window, cx| this.cycle_profile(window, cx))),
            )
            .child(
                nav_btn("browser-profile-new", "icons/plus.svg")
                    .tooltip("New isolated profile")
                    .on_click(cx.listener(|this, _, window, cx| this.new_profile(window, cx))),
            )
            .children(self.confirmed.map(|kind| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline * 0.5))
                    .px(px(density.pad_panel * 0.6))
                    .py(px(density.pad_panel * 0.25))
                    .rounded_full()
                    .bg(theme.bg_panel_alt)
                    .text_color(theme.fg_muted)
                    .text_size(px(typography.t_body_sm))
                    .child(
                        Icon::default()
                            .path("icons/check.svg")
                            .small()
                            .text_color(theme.status_ok),
                    )
                    .child(SharedString::from(kind.pill_label()))
            }));

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
