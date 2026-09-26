//! The panel header: title + Beta chip + maximize/close, and — only while a
//! device is attached — the device row (device menu, Detach) and the stream
//! settings row.

use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement as _, SharedString, Styled as _, WeakEntity, Window, div, px,
};
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::{DeviceId, DeviceInfo, DeviceState, Platform};

use super::SimulatorPanel;
use crate::actions::{ToggleRightSidebar, ToggleSimulatorMaximized};
use crate::shell::simulator::state::PanelState;

impl SimulatorPanel {
    pub(super) fn render_header(&self, state: &PanelState, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        // "Simulator" once Android devices are offered too.
        let android = self.hub.as_ref().is_some_and(|h| h.read(cx).android_sdk().is_some());
        let title = if android { "Simulator" } else { "iOS Simulator" };
        let title_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .flex_none()
            .h(px(density.h_top_bar))
            .pl(px(density.pad_panel))
            .pr(px(density.pad_row))
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                // Plain title, as in the reference; "Beta" rides on the tab's
                // tooltip ("iOS Simulator (Beta)").
                div()
                    .text_size(px(ty.t_body_md))
                    .text_color(theme.fg_base)
                    .child(title),
            )
            .child(div().flex_1())
            .child(self.render_layout_button())
            .child(
                Button::new("sim-close")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/x.svg"))
                    .tooltip("Close sidebar")
                    .on_click(|_, window: &mut Window, cx: &mut App| {
                        window.dispatch_action(Box::new(ToggleRightSidebar), cx)
                    }),
            );

        let mut header = div().flex().flex_col().w_full().flex_none().child(title_row);
        // The device row appears only once something is attached (reference
        // screenshots): the empty state carries its own Attach control.
        if let Some(udid) = self.device(cx)
            && !matches!(state, PanelState::Setup(_) | PanelState::Checking)
        {
            header = header.child(self.render_device_row(udid, cx));
            header = header.child(self.render_stream_row(cx));
        }
        header.into_any_element()
    }

    /// ⤢ opens the layout popover: "Fill" takes the whole content area,
    /// "Split" sits beside the centre panes. (Moving the panel to another
    /// edge is not offered: the sidebar lives on the right.)
    fn render_layout_button(&self) -> AnyElement {
        let fill = self.is_maximized();
        Button::new("sim-layout")
            .ghost()
            .xsmall()
            .icon(Icon::default().path(if fill { "icons/minimize-2.svg" } else { "icons/maximize-2.svg" }))
            .dropdown_menu(move |menu, _window, _cx| {
                menu.label("Fill and arrange")
                    .item(PopupMenuItem::new("Fill").checked(fill).on_click(move |_, window, cx| {
                        if !fill {
                            window.dispatch_action(Box::new(ToggleSimulatorMaximized), cx);
                        }
                    }))
                    .item(PopupMenuItem::new("Split").checked(!fill).on_click(move |_, window, cx| {
                        if fill {
                            window.dispatch_action(Box::new(ToggleSimulatorMaximized), cx);
                        }
                    }))
            })
            .into_any_element()
    }

    fn render_device_row(&self, udid: DeviceId, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density) = (self.theme, self.density);
        let devices = self.devices(cx);
        let info = devices.iter().find(|d| d.udid == udid).cloned();
        let label = info.as_ref().map(|d| format!("{} · {}", d.name, os_label(d))).unwrap_or_else(|| udid.to_string());
        let dot = info.as_ref().map(|d| state_color(&d.state, theme)).unwrap_or(theme.fg_subtle);
        let weak = cx.weak_entity();
        let current = Some(udid);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .h(px(density.h_action_row))
            .px(px(density.pad_panel))
            .child(div().size(px(6.0)).rounded_full().bg(dot).flex_none())
            .child(
                Button::new("sim-device")
                    .outline()
                    .small()
                    .label(SharedString::from(label))
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _window, _cx| {
                        device_menu(menu, &devices, current.as_ref(), weak.clone())
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("sim-detach")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/unplug.svg"))
                    .label("Detach")
                    .tooltip("Detach this simulator from the worktree")
                    .on_click(cx.listener(|this, _, _window, cx| this.detach(cx))),
            )
            .into_any_element()
    }

    pub(crate) fn devices(&self, cx: &App) -> Vec<DeviceInfo> {
        self.hub.as_ref().map(|h| h.read(cx).devices().to_vec()).unwrap_or_default()
    }
}

/// Booted = ok, booting = warn, anything else = subtle.
pub(super) fn state_color(state: &DeviceState, theme: oximux_settings::Theme) -> gpui::Hsla {
    match state {
        DeviceState::Booted => theme.status_ok,
        DeviceState::Booting => theme.status_warn,
        _ => theme.fg_subtle,
    }
}

/// "iOS 26.3", or the Android runtime as listed ("Android API 37.1").
pub(crate) fn os_label(d: &DeviceInfo) -> String {
    match d.udid.platform() {
        Platform::Ios => format!("iOS {}", d.os_version),
        Platform::Android => d.runtime.clone(),
    }
}

/// The device menu: iOS then Android; in each, booted devices first, then
/// everything else ("will boot"), newest runtime first. Picking a row
/// attaches it.
pub(super) fn device_menu(
    mut menu: PopupMenu,
    devices: &[DeviceInfo],
    current: Option<&DeviceId>,
    panel: WeakEntity<SimulatorPanel>,
) -> PopupMenu {
    let usable: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.is_available && d.kind != oximux_simulator::DeviceKind::Other)
        .collect();
    if usable.is_empty() {
        return menu.label("No simulators found");
    }
    let (ios, android): (Vec<&DeviceInfo>, Vec<&DeviceInfo>) = usable.into_iter().partition(|d| d.udid.platform() == Platform::Ios);
    let (ios_booted, ios_rest) = booted_first(ios);
    let (android_running, android_rest) = booted_first(android);
    let groups = [
        ("iOS · Booted", ios_booted),
        ("iOS · Available (will boot)", ios_rest),
        ("Android · Running", android_running),
        ("Android · Emulators (will boot)", android_rest),
    ];
    for (title, mut group) in groups {
        if group.is_empty() {
            continue;
        }
        // Newest runtime first, then by name.
        group.sort_by(|a, b| version_key(&b.os_version).cmp(&version_key(&a.os_version)).then(a.name.cmp(&b.name)));
        menu = menu.label(title);
        for device in group {
            let udid = device.udid.clone();
            let panel = panel.clone();
            menu = menu.item(
                PopupMenuItem::new(format!("{} — {}", device.name, os_label(device)))
                    .checked(current == Some(&device.udid))
                    .on_click(move |_, _window, cx| {
                        let udid = udid.clone();
                        let _ = panel.update(cx, |panel, cx| panel.attach(Some(udid), cx));
                    }),
            );
        }
    }
    let refresh = panel.clone();
    menu.separator()
        .item(PopupMenuItem::new("Refresh").on_click(move |_, _window, cx| {
            let _ = refresh.update(cx, |panel, cx| panel.refresh(cx));
        }))
        .item(PopupMenuItem::new("Open Xcode").on_click(|_, _window, _cx| super::body::open_xcode()))
}

fn booted_first(list: Vec<&DeviceInfo>) -> (Vec<&DeviceInfo>, Vec<&DeviceInfo>) {
    list.into_iter().partition(|d| d.state == DeviceState::Booted)
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}
