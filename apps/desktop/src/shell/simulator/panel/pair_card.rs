//! "Pair over Wi-Fi": a card above the phone for Android's wireless
//! debugging, with the phone's six-digit code or a QR code it scans. The work
//! runs in the hub (`hub::wifi`); this card holds the fields and shows where
//! the pairing stands. Esc closes the card and nothing else.
//!
//! The pairing code and the QR's password are secrets: they reach adb on
//! stdin, and the QR image is only ever drawn (never saved or logged).

use std::sync::Arc;

use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable as _, Image, ImageFormat, ImageSource, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Styled as _, Window, div, img, px,
};
use gpui_component::input::{Input, InputState};
use gpui_component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use super::SimulatorPanel;
use crate::shell::simulator::hub::{PairStage, can_submit};

/// Side of the drawn QR code, in points.
const QR_SIZE: f32 = 176.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PairTab {
    Code,
    Qr,
}

/// The card's own state: its fields, and the QR image drawn for the current
/// payload (cached so a repaint never re-encodes it).
pub(super) struct PairCard {
    tab: PairTab,
    addr: Entity<InputState>,
    code: Entity<InputState>,
    port: Entity<InputState>,
    qr: Option<(String, Arc<Image>)>,
    /// The card itself holds focus whenever no field does (a disabled button
    /// drops it), so Esc still closes it.
    focus: FocusHandle,
    /// The connect-port field was focused when it appeared.
    port_focused: bool,
}

impl SimulatorPanel {
    /// Open the card on the code tab, its first field focused.
    pub(super) fn open_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let card = PairCard {
            tab: PairTab::Code,
            addr: field("192.168.1.5:37123", window, cx),
            code: field("6-digit code", window, cx),
            port: field("Port", window, cx),
            qr: None,
            focus: cx.focus_handle(),
            port_focused: false,
        };
        // The input, not the card: a gpui-component Input only takes text
        // (and Esc only reaches the card) once its own handle has focus.
        window.focus(&card.addr.read(cx).focus_handle(cx), cx);
        self.pairing = Some(card);
        // One pairing at a time, app-wide: a card opened here starts clean
        // (another window's pairing, or a stale status, is dropped).
        if let Some(hub) = self.hub.clone() {
            hub.update(cx, |hub, cx| hub.cancel_pairing(cx));
        }
        cx.notify();
    }

    pub(super) fn close_pairing(&mut self, cx: &mut Context<Self>) {
        self.pairing = None;
        if let Some(hub) = self.hub.clone() {
            hub.update(cx, |hub, cx| hub.cancel_pairing(cx));
        }
        cx.notify();
    }

    pub(super) fn pairing_tab(&mut self, tab: PairTab, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.pairing.as_mut() else { return };
        if card.tab == tab {
            return;
        }
        card.tab = tab;
        let focus = card.addr.read(cx).focus_handle(cx);
        if let Some(hub) = self.hub.clone() {
            match tab {
                PairTab::Qr => hub.update(cx, |hub, cx| hub.pair_with_qr(cx)),
                PairTab::Code => {
                    hub.update(cx, |hub, cx| hub.cancel_pairing(cx));
                    window.focus(&focus, cx);
                }
            }
        }
        cx.notify();
    }

    fn submit_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(card), Some(hub)) = (self.pairing.as_ref(), self.hub.clone()) else { return };
        let stage = hub.read(cx).pair_stage().clone();
        // Once: an Enter reaches both the card and a focused button.
        if !can_submit(&stage) {
            return;
        }
        // The button is about to disable and drop focus: keep Esc working.
        window.focus(&card.focus, cx);
        if let PairStage::NeedConnectPort { host } = stage {
            let port = card.port.read(cx).value().trim().to_owned();
            if !port.is_empty() {
                hub.update(cx, |hub, cx| hub.connect_wifi(host, port, cx));
            }
            return;
        }
        let addr = card.addr.read(cx).value().trim().to_owned();
        let code = card.code.read(cx).value().trim().to_owned();
        if addr.is_empty() || code.is_empty() {
            return;
        }
        hub.update(cx, |hub, cx| hub.pair_with_code(addr, code, cx));
    }

    /// The card, above the phone, while it is open.
    pub(super) fn render_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hub = self.hub.clone()?;
        let (stage, payload, mdns) = {
            let hub = hub.read(cx);
            (hub.pair_stage().clone(), hub.pair_qr().map(str::to_owned), hub.pair_mdns())
        };
        let card = self.pairing.as_mut()?;
        // The connect-port field takes focus as it appears (the field that had
        // it is no longer drawn).
        if matches!(stage, PairStage::NeedConnectPort { .. }) && !card.port_focused {
            card.port_focused = true;
            window.focus(&card.port.read(cx).focus_handle(cx), cx);
        }
        // Draw the QR once per payload.
        card.qr = match (payload, card.qr.take()) {
            (Some(p), Some((cached, image))) if p == cached => Some((cached, image)),
            (Some(p), _) => crate::shell::qr::qr_png(&p, 6).map(|png| (p, Arc::new(Image::from_bytes(ImageFormat::Png, png)))),
            (None, _) => None,
        };
        let (tab, addr, code, port, qr, focus) =
            (card.tab, card.addr.clone(), card.code.clone(), card.port.clone(), card.qr.clone(), card.focus.clone());
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let small = |s: String| div().text_size(px(ty.t_body_sm)).text_color(theme.fg_muted).child(s);
        let busy = matches!(stage, PairStage::Pairing | PairStage::Connecting | PairStage::WaitingForScan);

        let tabs = div()
            .flex()
            .flex_row()
            .gap(px(density.gap_inline))
            .child(tab_button("sim-pair-tab-code", "Pairing code", tab == PairTab::Code, PairTab::Code, cx))
            .child(tab_button("sim-pair-tab-qr", "QR code", tab == PairTab::Qr, PairTab::Qr, cx));

        let mut body = div().flex().flex_col().gap(px(density.gap_inline)).w_full();
        match (&stage, tab) {
            (PairStage::NeedConnectPort { host }, _) => {
                body = body
                    .child(small(format!(
                        "Paired. Enter the port shown under “IP address & Port” on the phone's Wireless debugging screen ({host}:…)."
                    )))
                    .child(Input::new(&port).small());
            }
            (_, PairTab::Code) => {
                body = body
                    .child(small(
                        "On the phone: Developer options › Wireless debugging › Pair device with pairing code.".into(),
                    ))
                    .child(Input::new(&addr).small())
                    .child(Input::new(&code).small());
            }
            (_, PairTab::Qr) => {
                body = body.child(small("On the phone: Developer options › Wireless debugging › Pair device with QR code.".into()));
                if mdns == Some(false) {
                    body = body.child(small("QR pairing needs adb's mDNS (platform-tools 30+). Use the pairing code instead.".into()));
                } else if let Some((_, image)) = qr {
                    body = body.child(
                        div()
                            .flex()
                            .justify_center()
                            .w_full()
                            // Black on white whatever the theme: an inverted code
                            // is unreadable to many scanners.
                            .child(div().p(px(8.0)).bg(gpui::white()).child(img(ImageSource::Image(image)).size(px(QR_SIZE)))),
                    );
                }
            }
        }
        let status = match &stage {
            PairStage::Idle => None,
            PairStage::WaitingForScan => Some(("Waiting for the phone to scan the code…".to_owned(), theme.fg_muted)),
            PairStage::Pairing => Some(("Pairing…".to_owned(), theme.fg_muted)),
            PairStage::Connecting => Some(("Connecting…".to_owned(), theme.fg_muted)),
            PairStage::NeedConnectPort { .. } => None,
            PairStage::Done { name } => Some((format!("Paired {name}. Pick it under Physical devices."), theme.status_ok)),
            PairStage::Failed(why) => Some((why.clone(), theme.status_error)),
        };
        let action = match (&stage, tab) {
            (PairStage::Done { .. }, _) => None,
            (PairStage::NeedConnectPort { .. }, _) => Some("Connect"),
            (_, PairTab::Code) => Some("Pair"),
            (_, PairTab::Qr) => None,
        };
        let mut buttons = div().flex().flex_row().justify_end().gap(px(density.gap_inline)).child(
            Button::new("sim-pair-close")
                .ghost()
                .small()
                .label(if matches!(stage, PairStage::Done { .. }) { "Close" } else { "Cancel" })
                .on_click(cx.listener(|this, _, _window, cx| this.close_pairing(cx))),
        );
        if let Some(label) = action {
            buttons = buttons.child(
                Button::new("sim-pair-submit")
                    .primary()
                    .small()
                    .label(label)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.submit_pairing(window, cx))),
            );
        }
        Some(
            div()
                .id("sim-pair-card")
                .key_context("SimPairCard")
                .track_focus(&focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| match event.keystroke.key.as_str() {
                    // Esc closes the card and nothing else.
                    "escape" => {
                        cx.stop_propagation();
                        this.close_pairing(cx);
                    }
                    "enter" => {
                        cx.stop_propagation();
                        this.submit_pairing(window, cx);
                    }
                    _ => {}
                }))
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
                .child(
                    div()
                        .text_size(px(ty.t_body_md))
                        .font_weight(ty.w_semibold)
                        .text_color(theme.fg_base)
                        .child("Pair a phone over Wi-Fi"),
                )
                .child(small("The phone and this Mac must be on the same network.".into()))
                .child(tabs)
                .child(body)
                .children(status.map(|(text, color)| div().text_size(px(ty.t_body_sm)).text_color(color).child(text)))
                .child(buttons)
                .into_any_element(),
        )
    }
}

fn tab_button(id: &'static str, label: &'static str, active: bool, tab: PairTab, cx: &mut Context<SimulatorPanel>) -> impl IntoElement {
    let button = Button::new(id).small().label(label);
    let button = if active { button.primary() } else { button.ghost() };
    button.on_click(cx.listener(move |this, _, window, cx| this.pairing_tab(tab, window, cx)))
}
