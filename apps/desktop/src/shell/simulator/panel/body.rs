//! The panel body for each [`PanelState`]. Everything but the setup checklist
//! sits inside a phone outline (the "bezel"), matching the reference design:
//! the empty state is the bezel with no stream, and P6's live screen fills
//! the same frame.

use gpui::{
    AnyElement, App, Context, Div, IntoElement, ParentElement as _, SharedString, Styled as _, div, px,
};
use gpui_component::menu::DropdownMenu as _;
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::availability::{Availability, HelperStatus, Xcode};
use oximux_simulator::DeviceState;

use super::SimulatorPanel;
use super::header::device_menu;
use crate::shell::simulator::state::PanelState;

/// Widest the bezel gets; the panel letterboxes around it.
const BEZEL_MAX_W: f32 = 320.0;
/// The empty state's device glyph.
const EMPTY_ICON: f32 = 48.0;
/// Corner radius of the phone outline.
const BEZEL_RADIUS: f32 = 36.0;
/// Frame thickness of the phone outline.
const BEZEL_FRAME: f32 = 6.0;

impl SimulatorPanel {
    pub(super) fn render_body(&self, state: &PanelState, cx: &mut Context<Self>) -> AnyElement {
        let density = self.density;
        let content: AnyElement = match state {
            PanelState::Checking => self.status_line("Checking for Xcode and the simulator runtime…"),
            PanelState::Setup(availability) => return self.render_setup(availability),
            PanelState::Empty { error } => self.render_empty(error.as_deref(), cx),
            PanelState::Attaching => self.status_line("Finding a simulator…"),
            PanelState::Booting => self.status_line(&format!("Booting {}…", self.device_name(cx))),
            PanelState::Connecting => self.status_line("Starting stream…"),
            // P6 replaces this with the live screen.
            PanelState::Streaming => self.status_line(&format!("{} is connected.", self.device_name(cx))),
            PanelState::Disconnected { reason } => self.render_stopped(reason, "Reconnect", cx),
            PanelState::Error { message, xcode_hint } => {
                let body = self.render_stopped(message, "Retry", cx);
                if *xcode_hint {
                    div().flex().flex_col().gap(px(density.gap_inline)).child(body).child(self.xcode_hint()).into_any_element()
                } else {
                    body
                }
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .items_center()
            .p(px(density.pad_panel))
            .child(self.bezel().child(content))
            .into_any_element()
    }

    fn bezel(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(self.density.pad_panel))
            .w_full()
            .max_w(px(BEZEL_MAX_W))
            .flex_1()
            .min_h(px(0.))
            .p(px(self.density.pad_panel * 2.0))
            .rounded(px(BEZEL_RADIUS))
            .border(px(BEZEL_FRAME))
            .border_color(theme.border_active)
            .bg(theme.bg_base)
    }

    fn device_name(&self, cx: &App) -> String {
        let Some(udid) = self.device(cx) else { return "the simulator".into() };
        self.devices(cx).into_iter().find(|d| d.udid == udid).map(|d| d.name).unwrap_or_else(|| "the simulator".into())
    }

    fn text(&self, s: impl Into<SharedString>, size: f32, color: gpui::Hsla) -> Div {
        div().w_full().text_center().text_size(px(size)).text_color(color).child(s.into())
    }

    fn status_line(&self, s: &str) -> AnyElement {
        self.text(s.to_owned(), self.typography.t_body_sm, self.theme.fg_muted).into_any_element()
    }

    fn render_empty(&self, error: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let (theme, ty) = (self.theme, self.typography.clone());
        let devices = self.devices(cx);
        // Only once a listing has landed does "none booted" mean it.
        let listed = self.hub.as_ref().is_some_and(|h| h.read(cx).devices_listed());
        let none_booted = listed && !devices.iter().any(|d| d.state == DeviceState::Booted);
        let mut col = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(self.density.gap_inline))
            .w_full()
            .child(Icon::default().path("icons/smartphone.svg").size(px(EMPTY_ICON)).text_color(theme.fg_subtle))
            .child(
                self.text("Attach a simulator so agents can see your app", ty.t_body_lg, theme.fg_base)
                    .font_weight(ty.w_semibold),
            )
            .child(self.text(
                "Agents will control this simulator and take screenshots of its entire screen.",
                ty.t_body_sm,
                theme.fg_muted,
            ))
            .child(self.text("Shut-down devices boot automatically.", ty.t_body_sm, theme.fg_muted));
        if none_booted {
            col = col.child(self.text(
                "No booted simulator found. Boot one with `xcrun simctl boot <device>`.",
                ty.t_sub_label,
                theme.fg_subtle,
            ));
        }
        col = col.child(self.check_row(true, "Xcode and Simulator installed", None));
        if let Some(error) = error {
            col = col.child(self.text(error.to_owned(), ty.t_body_sm, theme.status_error));
        }
        col.child(self.text("Xcode is a trademark of Apple Inc.", ty.t_sub_label, theme.fg_subtle))
            .child(self.attach_split_button(devices, cx))
            .into_any_element()
    }

    /// Primary "Attach simulator" (the automatic pick) + a chevron opening
    /// the device menu.
    fn attach_split_button(&self, devices: Vec<oximux_simulator::DeviceInfo>, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        div()
            .flex()
            .flex_row()
            .gap(px(1.0))
            .child(
                Button::new("sim-attach")
                    .primary()
                    .small()
                    .label("Attach simulator")
                    .on_click(cx.listener(|this, _, _window, cx| this.attach(None, cx))),
            )
            .child(
                Button::new("sim-attach-pick")
                    .primary()
                    .small()
                    .icon(Icon::default().path("icons/chevron-down.svg"))
                    .tooltip("Choose a simulator")
                    .dropdown_menu(move |menu, _window, _cx| device_menu(menu, &devices, None, weak.clone())),
            )
            .into_any_element()
    }

    fn render_stopped(&self, reason: &str, action: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let (theme, ty) = (self.theme, self.typography.clone());
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(self.density.gap_inline))
            .child(self.text(reason.to_owned(), ty.t_body_sm, theme.fg_muted))
            .child(
                Button::new("sim-reconnect")
                    .outline()
                    .small()
                    .label(action)
                    .on_click(cx.listener(|this, _, _window, cx| this.reconnect(cx))),
            )
            .into_any_element()
    }

    fn xcode_hint(&self) -> AnyElement {
        let (theme, ty) = (self.theme, self.typography.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(self.density.gap_inline))
            .child(self.text("This version of Xcode is supported on a best-effort basis.", ty.t_body_sm, theme.status_warn))
            .child(self.text("If the simulator doesn't stream, select Xcode 26:", ty.t_body_sm, theme.fg_muted))
            .child(
                div()
                    .font_family(ty.family_mono.clone())
                    .text_size(px(ty.t_sub_label))
                    .text_color(theme.fg_base)
                    .child("sudo xcode-select -s /Applications/Xcode-26.app"),
            )
            .into_any_element()
    }

    /// The setup checklist: one row per requirement, live-updated while
    /// visible (the panel re-checks every few seconds).
    fn render_setup(&self, a: &Availability) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let xcode_ok = matches!(a.xcode, Xcode::Found { .. });
        let version = match &a.xcode {
            Xcode::Found { version: Some(v), .. } => Some(format!("Version {v}")),
            _ => None,
        };
        let card = div()
            .flex()
            .flex_col()
            .w_full()
            .rounded(px(density.r_card))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .child(self.check_row(xcode_ok, "Xcode installed", version))
            .child(self.check_row(a.macos_ok && a.arch_ok, "Apple silicon Mac on macOS 14 or later", None))
            .child(self.check_row(!a.ios_runtimes.is_empty(), "iOS Simulator runtime installed", None))
            .child(self.check_row(matches!(a.helper, HelperStatus::Found(_)), "Simulator helper bundled", None));
        let reason = a.blocking_reason().unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .gap(px(density.pad_panel))
            .p(px(density.pad_panel))
            .child(self.text("Set up the iOS simulator", ty.t_body_md, theme.fg_base).font_weight(ty.w_semibold))
            .child(self.text("Progress updates automatically as each step finishes.", ty.t_body_sm, theme.fg_muted))
            .child(card)
            .child(self.text(reason, ty.t_body_sm, theme.fg_muted))
            .child(self.text("Xcode is a trademark of Apple Inc.", ty.t_sub_label, theme.fg_subtle))
            .child(
                div().flex().justify_center().child(
                    Button::new("sim-open-xcode").primary().small().label("Open Xcode").on_click(|_, _window, _cx| open_xcode()),
                ),
            )
            .into_any_element()
    }

    fn check_row(&self, ok: bool, label: &'static str, detail: Option<String>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let (icon, color) = if ok { ("icons/check.svg", theme.status_ok) } else { ("icons/circle.svg", theme.fg_subtle) };
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .px(px(density.pad_panel))
            .py(px(density.pad_row))
            .child(Icon::default().path(icon).size(px(14.0)).text_color(color))
            .child(
                div()
                    .flex_1()
                    .text_size(px(ty.t_body_sm))
                    .text_color(if ok { theme.fg_base } else { theme.fg_muted })
                    .child(label),
            );
        if let Some(detail) = detail {
            row = row.child(div().text_size(px(ty.t_sub_label)).text_color(theme.fg_subtle).child(detail));
        }
        row.into_any_element()
    }
}

/// Open Xcode (Window › Devices is not scriptable). The `open` child is
/// reaped on a short-lived thread so it never lingers as a zombie.
pub(super) fn open_xcode() {
    if let Ok(mut child) = std::process::Command::new("/usr/bin/open").args(["-a", "Xcode"]).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
