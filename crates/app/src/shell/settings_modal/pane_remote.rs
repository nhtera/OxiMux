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
        // Without this the column sizes to its widest child, so a `w_full` child
        // resolves against a content-derived width — circular, and long prose
        // then lays out unwrapped and overflows the pane instead of wrapping.
        .w_full()
        .child(entries_card(theme, density, typography, entries(modal, theme, density, typography, cx)))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(status),
        );

    // Shown whether or not remote access is on, and above the pairing code, so it
    // is read *before* the toggle rather than discovered afterwards: enabling this
    // publishes the Mac's addresses to a third-party discovery service, which is a
    // consent question, not a footnote.
    col = col.child(network_disclosure(theme, typography));

    // Once the host is bound, show the scannable pairing code plus the host's
    // identity underneath (the id is a public key — safe to show; the secret rides
    // inside the code and is never rendered as text).
    if let Some(ticket) = ticket {
        // Only while the code is still redeemable — a spent code on screen invites
        // a scan that would just fail.
        if pairing_open {
            if let Some(image) = pairing_qr_image(modal, &ticket) {
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
            col = col.child(copy_link_row(&ticket, theme, density, typography, cx));
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

/// A second way to hand the pairing ticket to a phone: copy the deep link.
///
/// The QR alone is a dead end whenever a camera can't be pointed at the screen —
/// a simulator, a phone with a broken camera, or a Mac being driven remotely. The
/// mobile app already accepts a pasted `oximux://connect?ticket=…` (its manual
/// path), so the link was expected on this end and simply had no affordance.
///
/// The link carries the same one-time secret the QR encodes, so this widens where
/// that secret can land — the clipboard is readable by any running app — without
/// changing what it grants. It stays single-use and dies on redemption, and the
/// row is gated on `pairing_open` so a spent link is never offered. The secret is
/// still never rendered as text or logged.
fn copy_link_row(
    ticket: &PairingTicket,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    // Encode once here rather than inside the click handler: a ticket that can't
    // be encoded should not offer a button that silently does nothing.
    let Ok(url) = ticket.to_url() else {
        return div().into_any_element();
    };
    div()
        .pt(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(value_chip(
            "remote-copy-link",
            "Copy pairing link",
            theme,
            density,
            typography,
            move |_this, _window, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(url.clone()));
                toast(cx, ToastKind::Success, "Pairing link copied — paste it in the mobile app");
            },
            cx,
        ))
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Can't scan? Paste the link in the app instead."),
        )
        .into_any_element()
}

/// What turning remote access on actually exposes to third parties.
///
/// Kept deliberately concrete — "uses a relay" tells a user nothing actionable.
/// The two facts that matter are that an address is *published* to a public
/// discovery service, and that a fallback path sees connection metadata even
/// though it cannot read the traffic. Both are consequences of running with no
/// self-hosted infrastructure, which is the trade this feature deliberately made.
fn network_disclosure(theme: Theme, typography: &Typography) -> AnyElement {
    // Each line is kept short enough to fit the pane on its own. Long prose here
    // does NOT wrap — it lays out at its full single-line length and is clipped at
    // the pane's right edge — and neither `w_full`, a `flex_1` text column, nor
    // mirroring `setting_row_desc`'s row/column pairing changes that. Every other
    // descriptive line in this modal is short enough to dodge the issue, so this
    // is the pane's actual working pattern rather than a workaround around it.
    // Splitting the text is also better reading for a consent notice.
    div()
        .flex()
        .flex_col()
        .w_full()
        .pt(px(12.0))
        .gap(px(4.0))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child("While remote access is on, this Mac publishes its network addresses")
        .child("to n0's public discovery service, so a paired device can find it.")
        .child("If a direct connection isn't possible, traffic falls back to n0's public relays.")
        .child("Relays forward encrypted traffic and cannot read it, but they do see")
        .child("the IP addresses of both ends.")
        .into_any_element()
}

/// How long ago a device last authenticated, in the coarsest useful unit.
///
/// "Never" is the notable case, not an empty state: a device that paired but has
/// never come back is worth a second look — it is what a pairing the user does
/// not recognize would look like. Rounded to whole units because the exact minute
/// carries no decision value here; the question this answers is "is this device
/// still in use?".
fn last_seen_label(last_seen: Option<u64>) -> String {
    let Some(seen) = last_seen else { return "never connected".into() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // A clock that moved backwards (NTP correction, timezone-adjacent skew) would
    // otherwise underflow into an absurd "584942417355 days ago".
    let ago = now.saturating_sub(seen);
    match ago {
        0..=59 => "last seen just now".into(),
        60..=3599 => format!("last seen {}m ago", ago / 60),
        3600..=86_399 => format!("last seen {}h ago", ago / 3600),
        _ => format!("last seen {}d ago", ago / 86_400),
    }
}

/// The secondary line under a device's name.
///
/// Revocation displaces the last-seen time rather than joining it: once a device
/// is cut off, when it last connected is trivia, and the state it is in is the
/// only thing worth reading at a glance.
fn device_state_label(revoked: bool, last_seen: Option<u64>) -> String {
    if revoked {
        return "revoked — forget it to allow pairing again".into();
    }
    last_seen_label(last_seen)
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
        let oximux_remote_host::DeviceInfo { pubkey, name, read_only, last_seen, revoked } = device;
        // One pubkey per closure below; each needs its own copy.
        let revoke_key = pubkey;
        let forget_key = pubkey;
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
                                .child(format!(
                                    "{} · {}",
                                    short_endpoint_id(&pubkey),
                                    device_state_label(revoked, last_seen)
                                )),
                        ),
                )
                .child({
                    let mut actions =
                        div().flex().items_center().gap(px(density.gap_inline));

                    // A revoked device is already cut off, so re-tiering or
                    // re-revoking it means nothing; the only useful action left is
                    // the one that undoes it.
                    if !revoked {
                        actions = actions
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
                            ));
                    }

                    // Erasing the record, unlike revoking it, lets the device pair
                    // again — the host refuses to register a key it already knows,
                    // including a revoked one.
                    actions.child(value_chip(
                        ("remote-forget", idx),
                        "Forget",
                        theme,
                        density,
                        typography,
                        move |_this, _window, cx| {
                            if let Some(rc) = cx.try_global::<RemoteControl>() {
                                rc.forget_device(&forget_key);
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                }),
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
    this: &mut SettingsModal,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<SettingsModal>,
) {
    // Scope the global borrow so `cx` is free for the async bridge below. Returns
    // the dispatcher + secret to bind with when turning on; `None` when turning off,
    // plus the new state to persist once the borrow is released.
    let (prep, turning_on) = {
        let Some(rc) = cx.try_global::<RemoteControl>() else {
            return;
        };
        let turning_on = !rc.enabled();
        rc.set_enabled(turning_on);
        let prep = if turning_on {
            let (dispatcher, secret) = rc.prepare_host();
            // Subscribe before the host starts serving, so a fast scan can't pair
            // in the window between bind and subscribe and go unannounced.
            Some((dispatcher, secret, rc.endpoint_secret(), rc.subscribe_pairings()))
        } else {
            rc.stop_host();
            None
        };
        (prep, turning_on)
    };

    // Persist the choice so it survives a relaunch rather than silently reverting.
    this.persist_flag(crate::remote_control::ENABLED_SETTING, turning_on, cx);

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
    use super::{last_seen_label, short_endpoint_id, status_text};

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

    /// A device that paired but never came back is the case worth surfacing — it
    /// is what an unrecognized pairing looks like — so it gets its own wording
    /// rather than a blank.
    #[test]
    fn a_device_that_never_connected_says_so() {
        assert_eq!(last_seen_label(None), "never connected");
    }

    #[test]
    fn elapsed_time_is_reported_in_the_coarsest_useful_unit() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(last_seen_label(Some(now)), "last seen just now");
        assert_eq!(last_seen_label(Some(now - 120)), "last seen 2m ago");
        assert_eq!(last_seen_label(Some(now - 3 * 3600)), "last seen 3h ago");
        assert_eq!(last_seen_label(Some(now - 5 * 86_400)), "last seen 5d ago");
    }

    /// A timestamp in the future (a clock correction, or a row written by a
    /// machine whose clock ran ahead) must not underflow into an absurd
    /// "584942417355 days ago".
    #[test]
    fn a_future_timestamp_does_not_underflow() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(last_seen_label(Some(now + 10_000)), "last seen just now");
    }
}
