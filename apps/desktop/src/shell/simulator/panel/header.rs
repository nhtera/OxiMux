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
use oximux_simulator::{DeviceId, DeviceInfo, DeviceState};

use super::SimulatorPanel;
use crate::actions::{ToggleRightSidebar, ToggleSimulatorMaximized};
use crate::shell::simulator::state::PanelState;

impl SimulatorPanel {
    pub(super) fn render_header(&self, state: &PanelState, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
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
                    .child("iOS Simulator"),
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
        let label = info
            .as_ref()
            .map(|d| format!("{} · iOS {}", d.name, d.os_version))
            .unwrap_or_else(|| udid.to_string());
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

/// The device menu: booted devices first, then everything else ("will
/// boot"), each group by runtime. Picking a row attaches it.
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
        return menu.label("No iOS simulators found");
    }
    let (booted, rest): (Vec<&DeviceInfo>, Vec<&DeviceInfo>) =
        usable.into_iter().partition(|d| d.state == DeviceState::Booted);
    for (title, mut group) in [("Booted", booted), ("Available (will boot)", rest)] {
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
                PopupMenuItem::new(format!("{} — iOS {}", device.name, device.os_version))
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

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}
