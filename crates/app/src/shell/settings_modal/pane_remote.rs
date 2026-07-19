//! Remote-control pane — one master switch that exposes running agent sessions to
//! the OxiMux mobile app and, while on, binds the in-app iroh host so a phone can
//! pair. The toggle flips the shared [`RemoteControl`] global's `enabled` flag and
//! starts/stops the host: enabling binds an endpoint off-thread (on the tokio
//! runtime) and publishes a `PairingTicket` back here; disabling stops the host and
//! stops advertising. The ticket is shown as the host's pairing identity for now —
//! the scannable QR image lands in a later slice.
//!
//! Session-scoped for now: enabling is not persisted across restarts, and each
//! enable rotates the pairing secret (durable paired-device persistence is later).
//! The handshake secret is never displayed or logged.

use std::sync::Arc;

use gpui::{AnyElement, Image, ImageFormat, ImageSource, IntoElement, ParentElement, Styled, div, img, px};
use oximux_remote_proto::PairingTicket;
use oximux_settings::{Density, Theme, Typography};

use tokio::sync::broadcast;

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use crate::shell::toast::{ToastKind, toast};
use super::layout::{SettingEntry, entries_card, entry};
use crate::remote_control::RemoteControl;

/// Is remote control currently enabled? `false` if the global is somehow absent
/// (so the pane renders rather than panicking).
fn enabled(cx: &mut gpui::Context<SettingsModal>) -> bool {
    cx.try_global::<RemoteControl>().is_some_and(|rc| rc.enabled())
}

/// How many agent sessions are exposed right now (a snapshot at render time).
fn exposed_count(cx: &mut gpui::Context<SettingsModal>) -> usize {
    cx.try_global::<RemoteControl>().map(|rc| rc.registry().len()).unwrap_or(0)
}

/// The current pairing ticket, if the host has finished binding. `None` while off
/// or during the brief async bind.
fn pairing_ticket(cx: &mut gpui::Context<SettingsModal>) -> Option<PairingTicket> {
    cx.try_global::<RemoteControl>().and_then(|rc| rc.pairing_ticket())
}

/// The status line under the toggle: what enabling does (off), that the host is
/// still binding (on, no ticket yet), or how many sessions are exposed (on, ready).
/// Pure so it can be unit-tested. Never mentions the handshake secret.
fn status_text(enabled: bool, host_ready: bool, pairing_open: bool, exposed: usize) -> String {
    if !enabled {
        return "Turn on to expose your running agent sessions to the OxiMux mobile app. \
                Applies to sessions started after it's enabled."
            .to_string();
    }
    if !host_ready {
        return "Starting the pairing host — the pairing code will appear here in a moment."
            .to_string();
    }
    let exposure = match exposed {
        0 => "No agent sessions are running yet — start one and it will be exposed.".to_string(),
        1 => "1 running agent session is exposed.".to_string(),
        n => format!("{n} running agent sessions are exposed."),
    };
    if !pairing_open {
        // The code is single-use, so once redeemed it is gone rather than left on
        // screen looking valid.
        return format!(
            "Paired. Turn remote access off and on to pair another device. {exposure}"
        );
    }
    format!("Ready to pair. Scan the code from the OxiMux mobile app. {exposure}")
}

/// On-screen edge of the pairing code, in logical pixels.
const QR_SIZE: f32 = 180.0;

/// The pairing code as a renderable image, cached on the modal by the deep link it
/// encodes so a repaint neither re-encodes the PNG nor mints a new `Arc` (which
/// would re-upload the texture every frame). `None` if the ticket can't be encoded.
fn pairing_qr_image(modal: &SettingsModal, ticket: &PairingTicket) -> Option<Arc<Image>> {
    let url = ticket.to_url().ok()?;
    if let Some((cached_url, image)) = modal.qr_cache.borrow().as_ref()
        && *cached_url == url
    {
        return Some(image.clone());
    }
    // Render at 2x the on-screen size so the code stays crisp on a Retina display.
    let png = super::pairing_qr::qr_png(&url, 8)?;
    let image = Arc::new(Image::from_bytes(ImageFormat::Png, png));
    *modal.qr_cache.borrow_mut() = Some((url, image.clone()));
    Some(image)
}

/// A short, human-scannable form of the host endpoint id (an Ed25519 public key —
/// safe to show; it is not the secret). Confirms a real endpoint bound.
fn short_endpoint_id(id: &[u8; 32]) -> String {
    let head: String = id[..4].iter().map(|b| format!("{b:02x}")).collect();
    let tail: String = id[28..].iter().map(|b| format!("{b:02x}")).collect();
    format!("{head}…{tail}")
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let ticket = pairing_ticket(cx);
    let pairing_open = cx.try_global::<RemoteControl>().is_some_and(|rc| rc.pairing_open());
    let status = status_text(enabled(cx), ticket.is_some(), pairing_open, exposed_count(cx));

    let mut col = div()
        .flex()
        .flex_col()
        .child(entries_card(theme, density, typography, entries(modal, theme, density, typography, cx)))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(status),
        );

    // Once the host is bound, show the scannable pairing code plus the host's
    // identity underneath (the id is a public key — safe to show; the secret rides
    // inside the code and is never rendered as text).
    if let Some(ticket) = ticket {
        // Only while the code is still redeemable — a spent code on screen invites
        // a scan that would just fail.
        if pairing_open
            && let Some(image) = pairing_qr_image(modal, &ticket)
        {
            col = col.child(
                // Row + `flex_none` so the white card hugs the code; a plain child
                // of the column would stretch across the whole pane.
                div().pt(px(12.0)).flex().flex_row().items_start().child(
                    div()
                        .flex_none()
                        .p(px(8.0))
                        .rounded(px(density.r_xs))
                        // The code is painted black-on-white regardless of theme —
                        // an inverted QR is unreadable to many scanners.
                        .bg(gpui::white())
                        .child(img(ImageSource::Image(image)).size(px(QR_SIZE))),
                ),
            );
        }
        col = col.child(
            div()
                .pt(px(8.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(format!("Host {}", short_endpoint_id(&ticket.endpoint_id))),
        );
    }

    // Paired devices + one-tap revoke. Listed whether or not remote is on, so a
    // device can be cut off without first turning remote access back on.
    let devices = cx.try_global::<RemoteControl>().map(|rc| rc.paired_devices()).unwrap_or_default();
    if !devices.is_empty() {
        col = col.child(devices_section(devices, theme, density, typography, cx));
    }
    col.into_any_element()
}

/// The paired-devices list: one row per authorized device with a Revoke action.
/// Revoking takes effect on a running host immediately (per-RPC recheck) and is
/// persisted, so it survives a restart.
fn devices_section(
    devices: Vec<oximux_remote_host::DeviceInfo>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let mut section = div()
        .flex()
        .flex_col()
        .pt(px(16.0))
        .child(
            div()
                .pb(px(6.0))
                .text_size(px(typography.t_sub_label))
                .font_weight(typography.w_medium)
                .text_color(theme.fg_base)
                .child("Paired devices"),
        );

    for (idx, device) in devices.into_iter().enumerate() {
        let oximux_remote_host::DeviceInfo { pubkey, name, read_only } = device;
        // One pubkey per closure below; each needs its own copy.
        let revoke_key = pubkey;
        section = section.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(density.gap_inline))
                .h(px(density.h_action_row))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(typography.t_body_sm))
                                .text_color(theme.fg_base)
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(px(typography.t_sub_label))
                                .text_color(theme.fg_subtle)
                                .child(short_endpoint_id(&pubkey)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(density.gap_inline))
                        // The opt-down: pairing grants full access, so this is how a
                        // device gets narrowed to view-only without cutting it off.
                        .child(
                            div()
                                .text_size(px(typography.t_sub_label))
                                .text_color(theme.fg_subtle)
                                .child("Read-only"),
                        )
                        .child(toggle_switch(
                            ("remote-device-read-only", idx),
                            read_only,
                            theme,
                            move |_this, _window, cx| {
                                if let Some(rc) = cx.try_global::<RemoteControl>() {
                                    rc.set_device_read_only(&pubkey, !read_only);
                                }
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(value_chip(
                            ("remote-revoke", idx),
                            "Revoke",
                            theme,
                            density,
                            typography,
                            move |_this, _window, cx| {
                                if let Some(rc) = cx.try_global::<RemoteControl>() {
                                    rc.revoke_device(&revoke_key);
                                }
                                cx.notify();
                            },
                            cx,
                        )),
                ),
        );
    }
    section.into_any_element()
}

pub(super) fn entries(
    _modal: &SettingsModal,
    theme: Theme,
    _density: Density,
    _typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![entry(
        "Allow remote access",
        "Expose running agent sessions so the OxiMux mobile app can view and drive them.",
        remote_toggle(theme, cx),
    )]
}

/// The master switch. Reads the live global flag; clicking flips it and starts or
/// stops the iroh host (see [`on_toggle`]), then repaints.
fn remote_toggle(theme: Theme, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    toggle_switch("remote-enabled", enabled(cx), theme, on_toggle, cx)
}

/// Flip `RemoteControl.enabled` and start/stop the host. Turning on binds the iroh
/// endpoint off-thread (on the tokio runtime) and folds the resulting handle +
/// pairing ticket back in on the UI thread; turning off drops the handle, which
/// stops the accept loop and closes the endpoint.
fn on_toggle(
    _this: &mut SettingsModal,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<SettingsModal>,
) {
    // Scope the global borrow so `cx` is free for the async bridge below. Returns
    // the dispatcher + secret to bind with when turning on; `None` when turning off.
    let prep = {
        let Some(rc) = cx.try_global::<RemoteControl>() else {
            return;
        };
        let turning_on = !rc.enabled();
        rc.set_enabled(turning_on);
        if turning_on {
            let (dispatcher, secret) = rc.prepare_host();
            // Subscribe before the host starts serving, so a fast scan can't pair
            // in the window between bind and subscribe and go unannounced.
            Some((dispatcher, secret, rc.endpoint_secret(), rc.subscribe_pairings()))
        } else {
            rc.stop_host();
            None
        }
    };

    if let Some((dispatcher, secret, endpoint_secret, pairings)) = prep
        && let Ok(handle) = tokio::runtime::Handle::try_current()
    {
        // Bind on the tokio runtime (iroh needs it), then publish the handle back.
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.spawn(async move {
            let _ = tx
                .send(oximux_remote_iroh::start_host(dispatcher, secret, endpoint_secret).await);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(host)) = rx.await {
                let _ = this.update(cx, |_this, cx| {
                    if let Some(rc) = cx.try_global::<RemoteControl>() {
                        rc.set_host(host);
                    }
                    cx.notify();
                });
            }
            // Confirm each pairing to the user: with full access granted on the
            // first scan, a silent pairing is the failure mode to avoid. Ends on
            // its own when the host stops and the sender drops.
            let Some(mut pairings) = pairings else {
                return;
            };
            loop {
                match pairings.recv().await {
                    Ok(device) => {
                        let _ = this.update(cx, |_this, cx| {
                            toast(
                                cx,
                                ToastKind::Success,
                                format!("Paired \u{201c}{}\u{201d} — it has full access", device.name),
                            );
                            // Repaint: the code is spent and the device joins the list.
                            cx.notify();
                        });
                    }
                    // Fell behind the ring; the pairings are still in the device
                    // list, so keep listening rather than giving up.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .detach();
    }
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::{short_endpoint_id, status_text};

    #[test]
    fn status_reflects_enablement_host_readiness_and_count() {
        assert!(
            status_text(false, false, true, 3).starts_with("Turn on"),
            "disabled explains enabling",
        );
        assert!(
            status_text(true, false, true, 0).contains("Starting the pairing host"),
            "on but host not yet bound",
        );
        assert!(
            status_text(true, true, true, 0).contains("No agent sessions"),
            "ready, none running",
        );
        assert!(status_text(true, true, true, 1).contains("1 running agent session"), "singular");
        assert!(status_text(true, true, true, 4).contains("4 running agent sessions"), "plural");
        assert!(
            status_text(true, true, true, 2).starts_with("Ready to pair"),
            "ready leads with pairing",
        );
    }

    /// Once the single-use code is spent the pane stops inviting a scan and says
    /// how to pair another device.
    #[test]
    fn status_reports_a_spent_pairing_code() {
        let spent = status_text(true, true, false, 2);
        assert!(spent.starts_with("Paired"), "leads with the outcome: {spent}");
        assert!(!spent.contains("Scan the code"), "no longer invites a scan");
        assert!(spent.contains("off and on"), "says how to pair another device");
        assert!(spent.contains("2 running agent sessions"), "still reports exposure");
    }

    #[test]
    fn short_endpoint_id_shows_head_and_tail_only() {
        let mut id = [0u8; 32];
        id[0] = 0xab;
        id[1] = 0xcd;
        id[31] = 0xef;
        let short = short_endpoint_id(&id);
        assert!(short.starts_with("abcd"), "leads with the first bytes");
        assert!(short.ends_with("ef"), "ends with the last byte");
        assert!(short.contains('…'), "elides the middle");
    }
}
