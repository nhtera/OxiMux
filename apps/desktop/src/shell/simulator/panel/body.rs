//! The panel body: the phone (see [`super::bezel`]) with each
//! [`PanelState`]'s content on its screen, and the toolbar pill under it —
//! the reference design's layout: heading and disclosure at the top of the
//! screen, the checklist in rounded cards, the trademark line and a
//! full-width split Attach button at the bottom.

use gpui::{
    AnyElement, App, Context, Div, IntoElement, prelude::FluentBuilder as _, ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use gpui_component::menu::DropdownMenu as _;
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::availability::{Availability, HelperStatus, Xcode};
use oximux_simulator::geometry::{Size, display_size};
use oximux_simulator::{DeviceKind, DeviceState};

use super::SimulatorPanel;
use super::bezel::{Device, fit, phone};
use crate::shell::simulator::screen::Binding;
use super::header::{device_menu, watch_phones_while_open};
use crate::shell::simulator::state::PanelState;

/// The full trademark notice, as the platform owner asks it to be written.
const TRADEMARK: &str = "Xcode is a trademark of Apple Inc., registered in the U.S. and other countries.";

impl SimulatorPanel {
    pub(super) fn render_body(&mut self, state: &PanelState, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let density = self.density;
        let device = self.outline_device(cx);
        if !matches!(state, PanelState::Streaming) {
            // The stream went away under an annotation or a question.
            self.annotate = None;
            self.confirm_shutdown &= self.device(cx).is_some();
        }
        if !matches!(state, PanelState::Streaming)
            && let Some(screen) = self.screen.clone()
        {
            // Off screen: free the last frame and lift any finger.
            let binding = Binding { device: None, visible: false, radius: 0.0 };
            let (theme, typography) = (self.theme, self.typography.clone());
            screen.update(cx, |screen, cx| screen.bind(binding, theme, &typography, window, cx));
        }
        let screen: AnyElement = match state {
            PanelState::Checking => self.centered_line("Checking for Xcode and the simulator runtime…"),
            PanelState::Setup(availability) => self.render_setup(availability),
            PanelState::Empty { error } => self.render_empty(error.as_deref(), cx),
            PanelState::Attaching => self.centered_line("Finding a simulator…"),
            PanelState::Booting => self.centered_line(&format!("Booting {}…", self.device_name(cx))),
            // A real iPhone's first stream waits on macOS's Camera prompt.
            PanelState::Connecting if self.is_iphone(cx) => {
                self.centered_line("Starting stream… If macOS asks, allow Camera access for OxiMux Device Capture.")
            }
            PanelState::Connecting => self.centered_line("Starting stream…"),
            PanelState::Streaming if self.annotate.is_some() => {
                self.annotate.clone().map(IntoElement::into_any_element).unwrap_or_else(|| self.centered_line(""))
            }
            // A sleeping phone streams nothing: say so, and let the user wake it.
            PanelState::Streaming if self.screen_off(cx) => self.render_screen_off(cx),
            PanelState::Streaming => {
                let radius = self.area.get().and_then(|a| fit(a, &device)).map_or(0.0, |l| l.screen_radius);
                let binding = Binding { device: self.device(cx), visible: self.visible && self.window_visible, radius };
                match self.live_screen(binding, window, cx) {
                    Some(screen) => screen.into_any_element(),
                    None => self.centered_line("Starting stream…"),
                }
            }
            PanelState::Disconnected { reason, xcode_hint } => self.render_stopped(reason, "Reconnect", *xcode_hint, cx),
            PanelState::Error { message, xcode_hint } => self.render_stopped(message, "Retry", *xcode_hint, cx),
        };
        // The pairing card, while open, takes the banner's place.
        let banner = match self.render_pairing(window, cx) {
            Some(card) => Some(card),
            None => self.render_consent_banner(cx),
        };
        let badge = self.render_agent_badge(cx);
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .items_center()
            .gap(px(density.pad_panel))
            .px(px(density.pad_panel))
            .pt(px(density.pad_panel * 2.0))
            .pb(px(density.pad_panel))
            .children(banner)
            .child(phone(self.theme, &device, &self.area, cx.weak_entity(), screen))
            .child(if self.annotate.is_some() { self.render_annotate_controls(cx) } else { self.render_toolbar(state, cx) })
            .children((matches!(state, PanelState::Streaming) && self.is_iphone(cx)).then(|| self.render_iphone_control(cx)))
            .children(badge)
            .into_any_element()
    }

    /// What the outline depicts: the streamed device at its current size and
    /// orientation, else the placeholder phone.
    pub(super) fn outline_device(&self, cx: &App) -> Device {
        let Some(udid) = self.device(cx) else { return Device::PLACEHOLDER };
        let Some(hub) = self.hub.as_ref() else { return Device::PLACEHOLDER };
        let hub = hub.read(cx);
        let kind = hub.devices().iter().find(|d| d.udid == udid).map_or(DeviceKind::Phone, |d| d.kind);
        let Some(session) = hub.session(&udid) else { return Device { kind, ..Device::PLACEHOLDER } };
        let Some((w, h)) = session.framebuffer_size() else { return Device { kind, ..Device::PLACEHOLDER } };
        let orientation = session.orientation();
        let shown = display_size(orientation, Size::new(f64::from(w), f64::from(h)));
        Device { display: (shown.w as f32, shown.h as f32), kind, orientation }
    }

    pub(super) fn device_name(&self, cx: &App) -> String {
        let Some(udid) = self.device(cx) else { return "the simulator".into() };
        self.devices(cx).into_iter().find(|d| d.udid == udid).map(|d| d.name).unwrap_or_else(|| "the simulator".into())
    }

    fn text(&self, s: impl Into<SharedString>, size: f32, color: gpui::Hsla) -> Div {
        div().w_full().text_center().text_size(px(size)).text_color(color).child(s.into())
    }

    /// Screen padding and the top/bottom split every screen shares.
    fn screen(&self) -> Div {
        let pad = self.density.pad_panel * 2.0;
        div().flex().flex_col().size_full().px(px(pad)).pt(px(pad * 2.0)).pb(px(pad))
    }

    fn centered_line(&self, s: &str) -> AnyElement {
        self.screen()
            .items_center()
            .justify_center()
            .child(self.text(s.to_owned(), self.typography.t_body_md, self.theme.fg_muted))
            .into_any_element()
    }

    fn heading(&self, s: &'static str) -> Div {
        let ty = &self.typography;
        self.text(s, ty.t_display, self.theme.fg_base).font_weight(ty.w_semibold)
    }

    fn render_empty(&self, error: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let devices = self.devices(cx);
        // Only once a listing has landed does "none booted" mean it.
        let listed = self.hub.as_ref().is_some_and(|h| h.read(cx).devices_listed());
        let none_booted = listed && !devices.iter().any(|d| d.state == DeviceState::Booted);
        // One card per platform this Mac can run (at least one, or the panel
        // would be in Setup).
        let (ios, android, best_effort) = self.hub.as_ref().map_or((false, false, None), |h| {
            let hub = h.read(cx);
            let ios = hub.availability().filter(|a| a.is_ready());
            (ios.is_some(), hub.android_sdk().is_some(), ios.and_then(Availability::best_effort_note))
        });
        let mut top = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline))
            .w_full()
            .child(self.heading("Attach a device so agents can see your app"))
            .child(self.text(
                "Agents will control this device and take screenshots of its entire screen.",
                ty.t_body_md,
                theme.fg_muted,
            ))
            .child(self.text("Shut-down devices boot automatically.", ty.t_body_md, theme.fg_muted))
            .child(div().h(px(density.pad_panel)))
            .when(ios, |top| top.child(self.check_card(true, "Xcode and Simulator installed", None)))
            .children(best_effort.map(|note| self.text(note, ty.t_body_sm, theme.status_warn)))
            .when(android, |top| top.child(self.check_card(true, "Android SDK found", None)));
        if none_booted {
            top = top.child(self.text("No device is running yet; attaching boots one.", ty.t_body_sm, theme.fg_subtle));
        }
        if let Some(error) = error {
            top = top.child(self.text(error.to_owned(), ty.t_body_sm, theme.status_error));
        }
        self.screen()
            .child(top)
            .child(div().flex_1())
            .when(ios, |screen| screen.child(self.text(TRADEMARK, ty.t_sub_label, theme.fg_subtle)))
            .child(div().h(px(density.pad_panel)))
            .child(self.attach_split_button(devices, cx))
            .into_any_element()
    }

    /// Full width: primary "Attach device" (the automatic pick) + a
    /// chevron segment opening the device menu.
    fn attach_split_button(&self, devices: Vec<oximux_simulator::DeviceInfo>, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        div()
            .flex()
            .flex_row()
            .w_full()
            .gap(px(1.0))
            .child(
                div().flex_1().child(
                    Button::new("sim-attach")
                        .primary()
                        .large()
                        .w_full()
                        .label("Attach device")
                        .on_click(cx.listener(|this, _, _window, cx| this.attach(None, cx))),
                ),
            )
            .child(
                Button::new("sim-attach-pick")
                    .primary()
                    .large()
                    .icon(Icon::default().path("icons/chevron-down.svg"))
                    .tooltip("Choose a device")
                    .dropdown_menu(move |menu, _window, _cx| device_menu(menu, &devices, None, weak.clone()))
                    .on_open_change(watch_phones_while_open(self.hub.clone())),
            )
            .into_any_element()
    }

    /// Whether the attached device is a real iPhone.
    fn is_iphone(&self, cx: &App) -> bool {
        self.device(cx).is_some_and(|udid| udid.source() == oximux_simulator::Source::Devicectl)
    }

    /// Whether the attached phone's screen is off (it then streams nothing).
    fn screen_off(&self, cx: &App) -> bool {
        let (Some(hub), Some(udid)) = (self.hub.as_ref(), self.device(cx)) else { return false };
        hub.read(cx).screen_off(&udid)
    }

    /// The phone's screen is off. OxiMux never wakes a phone by itself; the
    /// user's click does.
    fn render_screen_off(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        self.screen()
            .items_center()
            .justify_center()
            .gap(px(density.pad_panel))
            .child(self.text("The phone's screen is off.", ty.t_body_md, theme.fg_muted))
            // Waking shows the lock screen, which sleeps again in seconds.
            .child(self.text("Wake it, then unlock the phone.", ty.t_body_sm, theme.fg_subtle))
            .child(Button::new("sim-wake").outline().small().label("Wake").on_click(cx.listener(|this, _, _window, cx| {
                if let (Some(hub), Some(udid)) = (this.hub.clone(), this.device(cx)) {
                    hub.update(cx, |hub, cx| hub.wake_screen(&udid, cx));
                }
            })))
            .into_any_element()
    }

    fn render_stopped(&self, reason: &str, action: &'static str, xcode_hint: bool, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let mut col = self
            .screen()
            .items_center()
            .justify_center()
            .gap(px(density.pad_panel))
            .child(self.text(reason.to_owned(), ty.t_body_md, theme.fg_muted))
            .child(
                Button::new("sim-reconnect")
                    .outline()
                    .small()
                    .label(action)
                    .on_click(cx.listener(|this, _, _window, cx| this.reconnect(cx))),
            )
            // Camera access is granted in System Settings, not here.
            .when(reason == crate::shell::simulator::hub::CAMERA_DENIED, |col| {
                col.child(
                    Button::new("sim-camera-settings")
                        .ghost()
                        .small()
                        .label("Open Camera settings")
                        .on_click(|_, _window, _cx| run_open(&[CAMERA_SETTINGS])),
                )
            });
        let hint = xcode_hint
            .then(|| self.hub.as_ref()?.read(cx).availability().map(|a| (a.best_effort_note(), a.switch_to_verified_hint())))
            .flatten();
        if let Some((note, switch)) = hint {
            col = col
                .children(note.map(|note| self.text(note, ty.t_body_sm, theme.status_warn)))
                .child(self.text(switch, ty.t_body_sm, theme.fg_muted));
        }
        col.into_any_element()
    }

    /// The setup checklist, live-updated while visible (the panel re-checks
    /// every few seconds), inside the phone like every other state.
    fn render_setup(&self, a: &Availability) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let xcode_ok = a.xcode_ok();
        let version = match &a.xcode {
            Xcode::Found { version: Some(v), .. } => Some(format!("Version {v}")),
            _ if a.xcode.unselected_app().is_some() => Some("Installed, not selected".to_owned()),
            _ => None,
        };
        let top = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline))
            .w_full()
            .child(self.heading("Set up the iOS simulator"))
            .child(self.text("Progress updates automatically as each step finishes.", ty.t_body_md, theme.fg_muted))
            .child(div().h(px(density.pad_panel)))
            .child(self.check_card(xcode_ok, "Xcode installed", version))
            .child(self.check_card(a.macos_ok && a.arch_ok, "Apple silicon Mac on macOS 14 or later", None))
            .child(self.check_card(!a.ios_runtimes.is_empty(), "iOS Simulator runtime installed", None))
            .child(self.check_card(matches!(a.helper, HelperStatus::Found(_)), "Simulator helper bundled", None))
            .child(self.text(a.blocking_reason().unwrap_or_default(), ty.t_body_sm, theme.fg_muted));
        self.screen()
            .child(top)
            .child(div().flex_1())
            .child(self.text(TRADEMARK, ty.t_sub_label, theme.fg_subtle))
            .child(div().h(px(density.pad_panel)))
            .child(setup_action(&a.xcode))
            .into_any_element()
    }

    /// One requirement as a rounded card: a filled status-ok disc with a
    /// check when met, an empty ring when not.
    fn check_card(&self, ok: bool, label: &'static str, detail: Option<String>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let mark = if ok {
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(CHECK_DISC))
                .rounded_full()
                .bg(theme.status_ok)
                .child(Icon::default().path("icons/check.svg").size(px(CHECK_DISC * 0.65)).text_color(theme.bg_base))
        } else {
            div().size(px(CHECK_DISC)).rounded_full().border_1().border_color(theme.fg_subtle)
        };
        let mut card = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.pad_panel))
            .w_full()
            .px(px(density.pad_panel * 1.5))
            .py(px(density.pad_panel * 1.5))
            .rounded(px(density.r_card))
            .bg(theme.bg_panel_alt)
            .child(mark.flex_none())
            .child(
                div()
                    .flex_1()
                    .text_size(px(ty.t_body_md))
                    .text_color(if ok { theme.fg_muted } else { theme.fg_base })
                    .child(label),
            );
        if let Some(detail) = detail {
            card = card.child(div().text_size(px(ty.t_sub_label)).text_color(theme.fg_subtle).child(detail));
        }
        card.into_any_element()
    }
}

/// Size of a checklist card's status disc.
const CHECK_DISC: f32 = 18.0;

/// The setup screen's primary button, for what actually unblocks this Mac:
/// switch `xcode-select` to an Xcode that is installed but not selected
/// (opening Xcode never does that), get Xcode when there is none, or open
/// it (to install a platform) when it is already selected.
fn setup_action(xcode: &Xcode) -> Button {
    let button = Button::new("sim-open-xcode").primary().large().w_full();
    if let Some(app) = xcode.unselected_app() {
        let app = app.to_path_buf();
        return button.label("Use this Xcode").on_click(move |_, _window, _cx| use_xcode(app.clone()));
    }
    match xcode {
        Xcode::Found { .. } => button.label("Open Xcode").on_click(|_, _window, _cx| open_xcode()),
        Xcode::Missing { .. } | Xcode::CommandLineToolsOnly { .. } => {
            button.label("Get Xcode").on_click(|_, _window, _cx| run_open(&[XCODE_APP_STORE]))
        }
    }
}

/// System Settings › Privacy & Security › Camera.
const CAMERA_SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera";

/// Xcode's Mac App Store page.
const XCODE_APP_STORE: &str = "macappstore://apps.apple.com/app/id497799835";

/// Switch the active developer directory to `app` behind macOS's
/// administrator prompt, on its own thread (the prompt blocks). The panel's
/// setup poll picks the new Xcode up within a few seconds; a dismissed
/// prompt changes nothing and says nothing. One prompt at a time: a second
/// click while one is up is ignored rather than stacking another.
fn use_xcode(app: std::path::PathBuf) {
    use std::sync::atomic::{AtomicBool, Ordering};

    use oximux_simulator::runner::SystemRunner;
    use oximux_simulator::xcode_app::{self, Selected};
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        match xcode_app::select(&SystemRunner, &app) {
            Ok(Selected::Switched) => tracing::info!(app = %app.display(), "selected Xcode as the developer directory"),
            Ok(Selected::Cancelled) => {}
            Err(err) => tracing::warn!(app = %app.display(), "could not select Xcode: {err}"),
        }
        IN_FLIGHT.store(false, Ordering::Release);
    });
}

/// Open Xcode (Window › Devices is not scriptable).
pub(super) fn open_xcode() {
    run_open(&["-a", "Xcode"]);
}

/// `/usr/bin/open args…`, reaped on a short-lived thread so it never lingers
/// as a zombie; a failure is logged rather than swallowed.
fn run_open(args: &'static [&'static str]) {
    match std::process::Command::new("/usr/bin/open").args(args).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => tracing::warn!("`open {}` failed: {status}", args.join(" ")),
                Err(err) => tracing::warn!("`open {}` failed: {err}", args.join(" ")),
                Ok(_) => {}
            });
        }
        Err(err) => tracing::warn!("`open {}` failed to spawn: {err}", args.join(" ")),
    }
}
