//! The element the user ⌥-clicked on the screen (see `screen::inspector`):
//! a card with what it is, the command that reaches it, and Send to agent /
//! Copy selector. The crop is taken when the element is picked; Send waits
//! for it (a moment), then dispatches at once, to the chat active at the
//! click.

use std::time::Duration;

use gpui::{AnyElement, ClipboardItem, Context, IntoElement, ParentElement as _, Styled as _, Window, div, px};
use gpui_component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_agent_core::scrub_text::scrub_text;

use super::{PanelEvent, SimulatorPanel, os_label};
use crate::shell::simulator::annotate::export;
use crate::shell::simulator::hub::NoticeKind;
use crate::shell::simulator::screen::Picked;

/// Room around the element in its crop, in points.
const CROP_PAD_PT: f64 = 16.0;

impl SimulatorPanel {
    /// An ⌥-click picked `picked`: show it, and take its crop now.
    pub(super) fn picked(&mut self, picked: Picked, cx: &mut Context<Self>) {
        self.picked_png = None;
        // Each pick its own: a crop landing late (an earlier pick of the same
        // element, or another device's) never attaches to this one.
        self.pick_seq += 1;
        let seq = self.pick_seq;
        let session = self.hub.as_ref().and_then(|hub| hub.read(cx).session(&picked.device));
        self.cropping = session.is_some();
        if let Some(session) = session {
            let rect = picked.rect;
            // Points to pixels: the element is `rect.w` of the screen's
            // width and `points.w` points wide.
            let points_w = picked.points.w.max(1.0);
            let pad_px = move |png_w: f64| (CROP_PAD_PT * png_w * rect.w / points_w).round() as u32;
            cx.spawn(async move |this, cx| {
                let crop = cx
                    .background_executor()
                    .spawn(async move {
                        let png = session.screenshot_png(Duration::from_secs(10)).ok()?;
                        let width = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?.width();
                        let cut = export::crop(&png, (rect.x, rect.y, rect.w, rect.h), pad_px(f64::from(width))).ok()?;
                        export::fit_long_edge(&cut, export::AGENT_LONG_EDGE).ok()
                    })
                    .await;
                let _ = this.update(cx, |panel, cx| {
                    // Still the pick it was taken for; failed, Send goes
                    // without it.
                    if panel.pick_seq == seq && panel.picked.is_some() {
                        panel.cropping = false;
                        panel.picked_png = crop;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        self.picked = Some(picked);
        cx.notify();
    }

    /// The card, while an element is picked on the shown device.
    pub(super) fn render_picked(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picked = self.picked.as_ref()?;
        if self.device(cx).as_ref() != Some(&picked.device) {
            return None;
        }
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let mono = |s: String| div().font_family(ty.family_mono.clone()).text_size(px(ty.t_body_sm)).text_color(theme.fg_muted).child(s);
        let buttons = div()
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(density.gap_inline))
            .child(Button::new("sim-pick-dismiss").ghost().small().label("Dismiss").on_click(cx.listener(|this, _, _, cx| {
                this.picked = None;
                this.picked_png = None;
                cx.notify();
            })))
            .child(Button::new("sim-pick-copy").small().label("Copy selector").on_click(cx.listener(|this, _, _, cx| this.copy_selector(cx))))
            .child(
                Button::new("sim-pick-send")
                    .primary()
                    .small()
                    .label("Send to agent")
                    // The crop is on its way.
                    .loading(self.cropping)
                    .on_click(cx.listener(|this, _, window, cx| this.send_picked(window, cx))),
            );
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(density.gap_inline))
                .w_full()
                .max_w(px(360.))
                .flex_none()
                .p(px(density.pad_panel * 1.5))
                .rounded(px(density.r_card))
                .border_1()
                .border_color(theme.border_active)
                .bg(theme.bg_panel_alt)
                .child(div().text_size(px(ty.t_body_md)).font_weight(ty.w_semibold).text_color(theme.fg_base).child(picked.title()))
                .child(mono(picked.tap_command()))
                .child(buttons)
                .into_any_element(),
        )
    }

    fn copy_selector(&mut self, cx: &mut Context<Self>) {
        let Some(picked) = self.picked.as_ref() else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(picked.tap_command()));
        cx.emit(PanelEvent::Notice(NoticeKind::Success, "Selector copied".into()));
    }

    fn send_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picked) = self.picked.take() else { return };
        let png = self.picked_png.take().unwrap_or_default();
        let device = self.devices(cx).into_iter().find(|d| d.udid == picked.device);
        let device = device.map(|d| format!("{}, {}", d.name, os_label(&d))).unwrap_or_else(|| self.device_name(cx));
        self.send_to_agent(element_markdown(&device, &picked), png, window, cx);
        cx.notify();
    }
}

/// What the agent reads about a picked element (scrubbed: a field's value
/// can be anything).
pub(super) fn element_markdown(device: &str, picked: &Picked) -> String {
    let node = &picked.node;
    let mut facts = vec![format!("role: {}", node.role)];
    if let Some(label) = node.label.as_deref().filter(|l| !l.is_empty()) {
        facts.push(format!("label: \"{label}\""));
    }
    if let Some(id) = node.identifier.as_deref().filter(|i| !i.is_empty()) {
        facts.push(format!("id: {id}"));
    }
    if let Some(value) = node.value.as_deref().filter(|v| !v.is_empty()) {
        facts.push(format!("value: \"{value}\""));
    }
    facts.push(if node.enabled { "enabled" } else { "disabled" }.into());
    let p = picked.points;
    scrub_text(&format!(
        "Mobile Emulator element ({device})\n- {}\n- frame (pt): x {:.0}, y {:.0}, w {:.0}, h {:.0}\n- selector: `{}`",
        facts.join(" · "),
        p.x,
        p.y,
        p.w,
        p.h,
        picked.tap_command()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_simulator::ax::{AxNode, Selector};
    use oximux_simulator::geometry::Rect;

    #[test]
    fn the_agent_reads_what_it_is_where_and_how_to_reach_it() {
        let picked = Picked {
            device: oximux_simulator::DeviceId("U".into()),
            node: AxNode {
                label: Some("Sign in".into()),
                identifier: Some("login_btn".into()),
                value: None,
                role: "Button".into(),
                role_description: String::new(),
                enabled: true,
                secure: false,
                frame: Rect::new(0.0, 0.0, 1.0, 1.0),
                children: vec![],
            },
            rect: Rect::new(0.3, 0.68, 0.4, 0.05),
            points: Rect::new(120.0, 600.0, 162.0, 44.0),
            selector: Some(Selector::Id("login_btn".into())),
        };
        assert_eq!(
            element_markdown("iPhone 17, iOS 26.3", &picked),
            "Mobile Emulator element (iPhone 17, iOS 26.3)\n- role: Button · label: \"Sign in\" · id: login_btn · enabled\n- frame (pt): x 120, y 600, w 162, h 44\n- selector: `oximux sim tap --id login_btn`"
        );
    }
}
