//! The pane's Availability block: a status badge and Refresh on the row, then
//! a nested card with one line per platform (the Android SDK, Xcode's iOS
//! Simulator), each with a status mark and what to do about it.
//!
//! Search lists the two platform lines as rows of their own
//! ([`platform_entries`]), so "android" or "xcode" still finds them.

use gpui::{AnyElement, Context, Entity, Hsla, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder, px};
use gpui_component::Icon;
use oximux_settings::{Density, Theme, Typography};
use oximux_simulator::availability::Xcode;
use oximux_simulator::Platform;

use super::super::SettingsModal;
use super::super::controls::{icon_button, value_chip};
use super::super::layout::{SettingEntry, entry};
use super::{change, hub};
use crate::shell::simulator::SimulatorHub;

/// Size of a platform line's status disc.
const DISC: f32 = 18.0;

/// Whether each platform is ready, and the line that says so.
struct Platforms {
    android: (bool, String),
    ios: (bool, String),
    /// Availability not answered yet: neither verdict is final.
    checking: bool,
}

fn platforms(hub: Option<&Entity<SimulatorHub>>, chosen_sdk: Option<&str>, cx: &Context<SettingsModal>) -> Platforms {
    let found = hub.and_then(|h| h.read(cx).android_sdk().map(|s| s.root.clone()));
    let android = (found.is_some(), android_summary_text(found.as_deref(), chosen_sdk.map(std::path::Path::new)));
    let Some(hub) = hub.map(|h| h.read(cx)) else {
        return Platforms { android, ios: (false, "Needs an Apple silicon Mac.".into()), checking: false };
    };
    let ios = match hub.availability() {
        None => (false, "Not checked yet.".to_owned()),
        Some(a) => match a.blocking_reason() {
            Some(reason) => (false, reason),
            None => {
                let xcode = match &a.xcode {
                    Xcode::Found { version: Some(v), .. } => format!("Xcode {v}"),
                    _ => "Xcode".to_owned(),
                };
                let runtimes = a.ios_runtimes.len();
                let plural = if runtimes == 1 { "" } else { "s" };
                (true, format!("Ready: {xcode}, {runtimes} iOS runtime{plural}."))
            }
        },
    };
    Platforms { android, ios, checking: hub.availability().is_none() }
}

/// "N devices detected", split by platform, from the hub's last listing.
fn devices_text(hub: Option<&Entity<SimulatorHub>>, cx: &Context<SettingsModal>) -> String {
    let Some(hub) = hub.map(|h| h.read(cx)) else { return "Needs an Apple silicon Mac.".into() };
    if !hub.devices_listed() {
        let nothing = hub.availability().is_some_and(|a| a.blocking_reason().is_some()) && hub.android_sdk().is_none();
        return if nothing {
            "No devices: this Mac has neither a usable Xcode nor the Android SDK.".into()
        } else {
            "Looking for devices…".into()
        };
    }
    let groups = crate::shell::simulator::panel::device_groups(hub.devices());
    let count = |platform: Platform| -> usize {
        groups.iter().flat_map(|(_, g)| g.iter()).filter(|d| d.is_available && d.udid.platform() == platform).count()
    };
    let (ios, android) = (count(Platform::Ios), count(Platform::Android));
    match ios + android {
        0 => "No devices found. Install an iOS runtime in Xcode, or create an emulator in Android Studio.".into(),
        n => format!("{n} device{} detected: {ios} iOS, {android} Android.", if n == 1 { "" } else { "s" }),
    }
}

/// The Availability row plus its nested platform card, as one block of the
/// pane's first card.
pub(super) fn block(
    chosen_sdk: Option<&str>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let hub = hub(cx);
    // Xcode is first checked when the panel is first used; a pane opened
    // before that would say "Checking" for good. Start it after this draw
    // (one check at a time: the hub ignores repeats while one runs).
    if let Some(unchecked) = hub.as_ref().filter(|h| h.read(cx).availability().is_none()) {
        let unchecked = unchecked.clone();
        cx.defer(move |cx| unchecked.update(cx, |hub, cx| hub.refresh_availability(cx)));
    }
    let p = platforms(hub.as_ref(), chosen_sdk, cx);
    let row = super::super::layout::entry_row(availability_entry(&p, hub.as_ref(), theme, density, typography, cx), theme, typography);
    let card = div()
        .flex()
        .flex_col()
        .w_full()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel_alt)
        .child(platform_line(
            p.android.0,
            false,
            "Android SDK",
            android_detail(&p.android.1, theme, density, typography),
            android_actions(chosen_sdk.is_some(), theme, density, typography, cx),
            theme,
            typography,
        ))
        .child(div().w_full().h(px(1.0)).bg(theme.border_inactive))
        .child(platform_line(
            p.ios.0,
            p.checking,
            "iOS Simulator (Xcode)",
            detail_text(p.ios.1, theme, typography),
            div().into_any_element(),
            theme,
            typography,
        ));
    div().flex().flex_col().w_full().child(row).child(div().w_full().pb(px(12.0)).child(card)).into_any_element()
}

fn availability_entry(
    p: &Platforms,
    hub: Option<&Entity<SimulatorHub>>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> SettingEntry {
    let (label, color) = if p.android.0 || p.ios.0 {
        ("Ready", theme.status_ok)
    } else if p.checking {
        ("Checking", theme.fg_muted)
    } else {
        ("Needs setup", theme.status_warn)
    };
    let controls = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(badge(label, color, typography))
        .child(icon_button("sim-refresh", "icons/refresh-cw.svg", "Check again", false, theme, density, |_, _, cx| refresh(cx), cx));
    entry("Availability", devices_text(hub, cx), controls)
}

/// A small pill in `color`: the verdict next to Refresh.
fn badge(label: &'static str, color: Hsla, typography: &Typography) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(1.0))
        .rounded_full()
        .border_1()
        .border_color(Hsla { a: 0.45, ..color })
        .bg(Hsla { a: 0.12, ..color })
        .text_size(px(typography.t_sub_label))
        .text_color(color)
        .child(label)
        .into_any_element()
}

/// One platform: a status disc, the name over its detail, actions at the right.
fn platform_line(
    ok: bool,
    pending: bool,
    title: &'static str,
    detail: AnyElement,
    actions: AnyElement,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let mark = if ok {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(DISC))
            .rounded_full()
            .bg(theme.status_ok)
            .child(Icon::default().path("icons/check.svg").size(px(DISC * 0.65)).text_color(theme.bg_base))
    } else if pending {
        div().size(px(DISC)).rounded_full().border_1().border_color(theme.fg_subtle)
    } else {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(DISC))
            .child(Icon::default().path("icons/alert-triangle.svg").size(px(DISC * 0.8)).text_color(theme.status_warn))
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .px(px(12.0))
        .py(px(10.0))
        .child(mark.flex_none())
        .child(
            // `min_w_0`: the detail wraps inside the row instead of pushing
            // the actions off the card.
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(typography.t_body_md)).text_color(theme.fg_base).child(title))
                .child(detail),
        )
        .child(div().flex_none().child(actions))
        .into_any_element()
}

fn detail_text(text: impl Into<SharedString>, theme: Theme, typography: &Typography) -> AnyElement {
    div().text_size(px(typography.t_body_sm)).text_color(theme.fg_subtle).child(text.into()).into_any_element()
}

/// "Detected at <path>" with the path set in mono, or the summary as is.
fn android_detail(summary: &str, theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    let Some(path) = summary.strip_prefix(ANDROID_FOUND) else { return detail_text(summary.to_owned(), theme, typography) };
    // One line: a long path shortens to "…" rather than running off the card
    // (gpui truncates only text that does not fit, so a short path shows whole).
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .min_w_0()
        .child(div().flex_none().child(detail_text("Detected at", theme, typography)))
        .child(
            div()
                .min_w_0()
                .truncate()
                .px(px(4.0))
                .rounded(px(density.r_xs))
                .bg(theme.bg_overlay)
                .font_family(typography.family_mono.clone())
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(path.to_owned()),
        )
        .into_any_element()
}

fn android_actions(chosen: bool, theme: Theme, density: Density, typography: &Typography, cx: &mut Context<SettingsModal>) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .gap(px(6.0))
        .when(chosen, |row| {
            row.child(value_chip("sim-android-sdk-clear", "Clear", theme, density, typography, |_, _, cx| set_android_sdk(None, cx), cx))
        })
        .child(value_chip("sim-android-sdk", "Locate SDK folder", theme, density, typography, |_, window, cx| choose_android_sdk(window, cx), cx))
        .into_any_element()
}

/// The two platform lines as searchable rows.
// Only the settings search reads this, and it lists the pane only where the
// pane can run (Apple silicon).
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub(super) fn platform_entries(
    chosen_sdk: Option<&str>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let hub = hub(cx);
    let p = platforms(hub.as_ref(), chosen_sdk, cx);
    vec![
        availability_entry(&p, hub.as_ref(), theme, density, typography, cx),
        entry("Android SDK", p.android.1, android_actions(chosen_sdk.is_some(), theme, density, typography, cx)),
        entry("iOS Simulator (Xcode)", p.ios.1, div()),
    ]
}

fn refresh(cx: &mut Context<SettingsModal>) {
    if let Some(hub) = hub(cx) {
        hub.update(cx, |hub, cx| {
            hub.refresh_availability(cx);
            // A changed SDK re-lists on its own; this covers devices created
            // since the last listing (it lists nothing when neither platform
            // is there).
            hub.refresh_android_sdk(cx);
            hub.refresh_devices(cx);
        });
    }
}

/// How [`android_summary_text`] starts when the SDK in use is the one found.
const ANDROID_FOUND: &str = "Detected at ";

/// The SDK line: `found` is the SDK in use, `chosen` the folder picked here.
/// A chosen folder without adb is said so, even when another SDK (the
/// environment's, Android Studio's) is used instead.
pub(super) fn android_summary_text(found: Option<&std::path::Path>, chosen: Option<&std::path::Path>) -> String {
    const NO_ADB: &str = "No adb in the chosen folder (it wants the SDK root, with platform-tools inside)";
    match (found, chosen) {
        (Some(root), Some(chosen)) if root != chosen => format!("{NO_ADB}; using {}.", root.display()),
        (Some(root), _) => format!("{ANDROID_FOUND}{}", root.display()),
        (None, Some(_)) => format!("{NO_ADB}."),
        (None, None) => "Not found. Install Android Studio, set ANDROID_HOME, or locate the SDK folder.".into(),
    }
}

/// Pick the SDK folder, save it, and look again. Rooted in the window: the
/// native panel resolves outside GPUI's window context.
fn choose_android_sdk(window: &mut gpui::Window, cx: &mut Context<SettingsModal>) {
    cx.spawn_in(window, async move |this, cx| {
        let Some(folder) = rfd::AsyncFileDialog::new().set_title("Android SDK folder").pick_folder().await else { return };
        let path = folder.path().to_string_lossy().into_owned();
        let _ = this.update_in(cx, |_, _, cx| set_android_sdk(Some(path), cx));
    })
    .detach();
}

/// Save the SDK folder (`None`: back to the environment and Android Studio's
/// default) and look again.
fn set_android_sdk(path: Option<String>, cx: &mut Context<SettingsModal>) {
    change(cx, |s| s.android_sdk = path);
    if let Some(hub) = hub(cx) {
        hub.update(cx, |hub, cx| hub.refresh_android_sdk(cx));
    }
}
