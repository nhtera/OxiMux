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
        // One name for iOS simulators and Android emulators and phones.
        let title = "Mobile Emulator";
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
                // tooltip ("Mobile Emulator (Beta)").
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
                    })
                    .on_open_change(watch_phones_while_open(self.hub.clone())),
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

/// The title of the device menu's group of real devices (phones, iPhones).
pub(crate) const PHYSICAL_GROUP: &str = "Physical devices";

/// The devices in menu order: iOS simulators then Android emulators — in
/// each, booted devices first, then everything else ("will boot") — then the
/// real devices ([`PHYSICAL_GROUP`]), which keep their unavailable rows (a
/// phone that has not approved this Mac yet: shown disabled, with its note).
/// Newest runtime first, then by name; empty groups are left out. Shared by
/// the panel's device menu and the Settings pane (whose default-device menu
/// leaves the real devices out: one is never attached automatically).
pub(crate) fn device_groups(devices: &[DeviceInfo]) -> Vec<(&'static str, Vec<&DeviceInfo>)> {
    let (physical, virtual_devices): (Vec<&DeviceInfo>, Vec<&DeviceInfo>) =
        devices.iter().filter(|d| d.kind != oximux_simulator::DeviceKind::Other).partition(|d| d.udid.is_physical());
    let usable = virtual_devices.into_iter().filter(|d| d.is_available);
    let (ios, android): (Vec<&DeviceInfo>, Vec<&DeviceInfo>) = usable.partition(|d| d.udid.platform() == Platform::Ios);
    let (ios_booted, ios_rest) = booted_first(ios);
    let (android_running, android_rest) = booted_first(android);
    [
        ("iOS · Booted", ios_booted),
        ("iOS · Available (will boot)", ios_rest),
        ("Android · Running", android_running),
        ("Android · Emulators (will boot)", android_rest),
        (PHYSICAL_GROUP, physical),
    ]
    .into_iter()
    .filter(|(_, group)| !group.is_empty())
    .map(|(title, mut group)| {
        group.sort_by(|a, b| version_key(&b.os_version).cmp(&version_key(&a.os_version)).then(a.name.cmp(&b.name)));
        (title, group)
    })
    .collect()
}

/// For a device menu's `on_open_change`: phones are watched while it is open,
/// so a row enables the moment its phone approves this Mac.
pub(super) fn watch_phones_while_open(
    hub: Option<gpui::Entity<crate::shell::simulator::SimulatorHub>>,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    move |open, _window, cx| {
        if let Some(hub) = &hub {
            hub.update(cx, |hub, _| hub.set_device_menu_open(*open));
        }
    }
}

/// The device menu ([`device_groups`]). Picking a row attaches it.
pub(super) fn device_menu(
    mut menu: PopupMenu,
    devices: &[DeviceInfo],
    current: Option<&DeviceId>,
    panel: WeakEntity<SimulatorPanel>,
) -> PopupMenu {
    let groups = device_groups(devices);
    if groups.is_empty() {
        return menu.label("No devices found");
    }
    for (title, group) in groups {
        menu = menu.label(title);
        for device in group {
            let udid = device.udid.clone();
            let panel = panel.clone();
            menu = menu.item(
                PopupMenuItem::new(menu_label(device))
                    .checked(current == Some(&device.udid))
                    .disabled(!device.is_available)
                    .on_click(move |_, _window, cx| {
                        let udid = udid.clone();
                        let _ = panel.update(cx, |panel, cx| panel.attach(Some(udid), cx));
                    }),
            );
        }
    }
    let (refresh, pair) = (panel.clone(), panel.clone());
    menu = menu.separator().item(PopupMenuItem::new("Pair over Wi-Fi…").on_click(move |_, window, cx| {
        let _ = pair.update(cx, |panel, cx| panel.open_pairing(window, cx));
    }));
    if let Some(udid) = current.filter(|u| crate::shell::simulator::SimulatorHub::paired_over_wifi(u)).cloned() {
        let forget = panel.clone();
        menu = menu.item(PopupMenuItem::new("Forget Wi-Fi phone").on_click(move |_, _window, cx| {
            let udid = udid.clone();
            let _ = forget.update(cx, |panel, cx| {
                if let Some(hub) = panel.hub.clone() {
                    hub.update(cx, |hub, cx| hub.forget_wifi(&udid, cx));
                }
            });
        }));
    }
    menu.separator()
        .item(PopupMenuItem::new("Refresh").on_click(move |_, _window, cx| {
            let _ = refresh.update(cx, |panel, cx| panel.refresh(cx));
        }))
        .item(PopupMenuItem::new("Open Xcode").on_click(|_, _window, _cx| super::body::open_xcode()))
}

/// "Pixel 8 — Android 16", with the row's note when it has one: why it cannot
/// be used yet ("… · Unlock the phone and tap Allow USB debugging"), or what
/// it is ("… · Not paired by OxiMux").
pub(super) fn menu_label(device: &DeviceInfo) -> String {
    let label = format!("{} — {}", device.name, os_label(device));
    match device.note.as_deref() {
        Some(note) => format!("{label} · {note}"),
        None => label,
    }
}

fn booted_first(list: Vec<&DeviceInfo>) -> (Vec<&DeviceInfo>, Vec<&DeviceInfo>) {
    list.into_iter().partition(|d| d.state == DeviceState::Booted)
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}
