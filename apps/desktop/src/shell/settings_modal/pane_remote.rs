//! Remote-control pane — one master switch that exposes running agent sessions to
//! the OxiMux mobile app and, while on, binds the in-app iroh host so a phone can
//! pair. The toggle flips the shared [`RemoteControl`] global's `enabled` flag and
//! starts/stops the host: enabling binds an endpoint off-thread (on the tokio
//! runtime) and publishes a `PairingTicket` back here; disabling stops the host and
//! stops advertising.
//!
//! **Pairing is its own act, not a side effect of binding.** The pane at rest holds
//! no live code; "Pair a device" opens a sub-view, and opening that view is what
//! mints the one-time secret it renders as a scannable QR (see
//! [`pairing_qr`](super::pairing_qr)). Leaving the view — or closing the modal —
//! retires the secret again.
//!
//! That split is what lets a second device pair without disturbing the first. The
//! endpoint id is pinned to the durable host identity and the secret is just a
//! value in the live `AuthStore`, so a new code costs a mutex rather than a
//! rebind. Opening a window from the toggle instead (the shape this replaced)
//! meant the only way to reach a fresh code was to toggle off and on, which
//! closes the endpoint under every device already connected through it.
//!
//! The switch itself is persisted (`ENABLED_SETTING`), so a desktop that had
//! remote access on comes back up serving it — otherwise a paired phone would
//! silently fail to reach a restarted Mac. The pairing window is deliberately not
//! persisted: a relaunch resumes the host with no slot open, so already-paired
//! devices reconnect (they authenticate by key against the durable device store)
//! while no new device can claim this Mac unasked. The handshake secret is never
//! displayed or logged.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Image, ImageFormat, ImageSource, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Styled, div, img, px, svg,
};
use oximux_remote_proto::PairingTicket;
use oximux_settings::{Density, Theme, Typography};

use tokio::sync::broadcast;

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use crate::shell::toast::{ToastKind, toast};
use super::layout::{
    SettingEntry, card_surface, entries_card, entry, section_card, section_title,
};
use crate::remote_control::{PAIRING_WINDOW_SECS, RemoteControl};
use oximux_remote_host::PairingEvent;

/// The pairing sub-view's state. Held on the modal; `None` means the pane is at
/// rest and, by construction, that no pairing code is live.
pub(crate) enum PairingState {
    /// The user asked to pair while remote access was off (or mid-bind), so the
    /// host is coming up. Becomes [`Waiting`](Self::Waiting) once it binds.
    Starting,
    /// A live code. `expires_at` is unix seconds, mirroring the host's own copy of
    /// the window so the countdown can't disagree with what the host will accept.
    ///
    /// `note` reports something that happened without ending the wait — currently
    /// an already-paired device re-scanning. That case leaves the code unspent, so
    /// it belongs *beside* the still-valid QR rather than replacing it: another
    /// device can still use this same code.
    Waiting { ticket: PairingTicket, expires_at: u64, note: Option<String> },
    /// A device redeemed the code. Held until dismissed rather than auto-closing:
    /// during the scan the user is looking at the phone, and this is what they
    /// look back to the Mac to read.
    Paired { name: String },
    /// The window lapsed unscanned.
    Expired,
}

/// Wall clock in unix seconds, saturating at the epoch on a clock set before it.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Leave the pairing sub-view and retire whatever code it opened. Idempotent, so
/// the modal's `close()` can call it unconditionally.
pub(super) fn close_pairing(modal: &mut SettingsModal, cx: &mut gpui::App) {
    if modal.remote_pairing.take().is_some() {
        // Retires this window's watcher along with its code.
        modal.remote_pairing_epoch += 1;
        if let Some(rc) = cx.try_global::<RemoteControl>() {
            rc.close_pairing();
        }
    }
}

/// Mint a window on the already-bound host and show it. No-op (leaving the caller's
/// state alone) if no host is bound.
fn open_window(modal: &mut SettingsModal, cx: &mut gpui::Context<SettingsModal>) {
    let Some(ticket) = cx.try_global::<RemoteControl>().and_then(|rc| rc.open_pairing()) else {
        return;
    };
    modal.remote_pairing = Some(PairingState::Waiting {
        ticket,
        expires_at: now_secs() + PAIRING_WINDOW_SECS,
        note: None,
    });
    modal.remote_pairing_epoch += 1;
    let epoch = modal.remote_pairing_epoch;
    start_countdown(cx, epoch);
    watch_for_pairing(cx, epoch);
}

/// Report devices that un-enrol themselves, for as long as the modal lives.
///
/// Separate from [`watch_for_pairing`] because it answers a different question.
/// That one is scoped to a pairing window and dies with it; this one has to
/// outlive every window, since a phone's "Forget this desktop" is not an answer
/// to anything the desktop is doing — it can land with no code on screen and the
/// Remote pane nowhere in sight.
///
/// The repaint is the point: the paired-devices list renders from a snapshot
/// taken per frame, so an open pane would otherwise keep listing a phone that had
/// already left. The toast covers the other case, where the pane is closed and
/// the repaint alone would tell the user nothing.
pub(super) fn watch_for_unpair(modal: &mut SettingsModal, cx: &mut gpui::Context<SettingsModal>) {
    if modal.remote_unpair_watch {
        return;
    }
    let Some(mut pairings) = cx.try_global::<RemoteControl>().and_then(|rc| rc.subscribe_pairings())
    else {
        return;
    };
    modal.remote_unpair_watch = true;
    cx.spawn(async move |this, cx| {
        loop {
            match pairings.recv().await {
                Ok(PairingEvent::Unpaired(device)) => {
                    let updated = this.update(cx, |_this, cx| {
                        toast(
                            cx,
                            ToastKind::Info,
                            format!(
                                "\u{201c}{}\u{201d} unpaired itself \u{2014} it needs a new code to return",
                                device.name
                            ),
                        );
                        cx.notify();
                    });
                    // The modal is gone, so nothing is left to repaint or toast.
                    if updated.is_err() {
                        break;
                    }
                }
                // Pairing outcomes belong to the window that opened the code.
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                // The store this listened to is gone — turning remote access off
                // and on again builds a new one. Clear the flag on the way out so
                // the next open subscribes to the replacement instead of trusting
                // a watcher that has nothing left to hear.
                Err(broadcast::error::RecvError::Closed) => {
                    let _ = this.update(cx, |this, _cx| this.remote_unpair_watch = false);
                    break;
                }
            }
        }
    })
    .detach();
}

/// Wait for a device to redeem the window opened as `epoch`, then show it.
///
/// Scoped to the window rather than to the host, because a window is the only
/// thing that can admit a device and only this view opens one. Subscribing at
/// bind time instead — which is what this used to do — left the common case
/// unwatched entirely: a desktop that restored remote access at launch never ran
/// the bind path from this pane, so a scan completed on the host while the view
/// sat on a QR that had already been spent.
///
/// `epoch` is what stops the *other* failure mode. This parks on `recv()` and
/// nothing wakes it when its window closes unredeemed, so every window opened in
/// a session would otherwise leave a live listener behind — and the first device
/// to pair would wake all of them, toasting once per window ever opened.
fn watch_for_pairing(cx: &mut gpui::Context<SettingsModal>, epoch: u64) {
    let Some(mut pairings) = cx.try_global::<RemoteControl>().and_then(|rc| rc.subscribe_pairings())
    else {
        return;
    };
    cx.spawn(async move |this, cx| {
        loop {
            match pairings.recv().await {
                // A known device re-scanned. It is refused and reconnects by key
                // instead, so nothing was admitted and the code is still good —
                // the wait continues, annotated. Without this the desktop sat on
                // an untouched-looking QR while the phone happily connected.
                Ok(PairingEvent::AlreadyPaired(device)) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.remote_pairing_epoch != epoch {
                            return;
                        }
                        toast(
                            cx,
                            ToastKind::Info,
                            format!(
                                "\u{201c}{}\u{201d} is already paired — it reconnected instead",
                                device.name
                            ),
                        );
                        if let Some(PairingState::Waiting { note, .. }) = &mut this.remote_pairing {
                            *note = Some(format!(
                                "{} is already paired, so it reconnected instead. \
                                 This code is still good for a new device.",
                                device.name
                            ));
                        }
                        cx.notify();
                    });
                    // Deliberately no `break`: this window never admitted anyone.
                    continue;
                }
                Ok(PairingEvent::Paired(device)) => {
                    let _ = this.update(cx, |this, cx| {
                        // A later window owns this outcome; stay silent and let
                        // its own watcher report it.
                        if this.remote_pairing_epoch != epoch {
                            return;
                        }
                        toast(
                            cx,
                            ToastKind::Success,
                            format!("Paired \u{201c}{}\u{201d} — it has full access", device.name),
                        );
                        // Retire the code explicitly. Redemption already marks a
                        // one-time slot spent, but dropping it leaves nothing
                        // behind to reason about — the window is closed, not
                        // merely used up. Bumping the epoch with it keeps this
                        // watcher from being mistaken for a live one.
                        if let Some(rc) = cx.try_global::<RemoteControl>() {
                            rc.close_pairing();
                        }
                        this.remote_pairing_epoch += 1;
                        // Only while the view is open; a pairing that lands after
                        // the user walked away belongs in the toast alone.
                        if this.remote_pairing.is_some() {
                            this.remote_pairing =
                                Some(PairingState::Paired { name: device.name.clone() });
                        }
                        cx.notify();
                    });
                    // Done either way: this window admitted its one device, or a
                    // later one superseded it. `Pair another` spawns a fresh watcher.
                    break;
                }
                // A device leaving says nothing about the window this watcher
                // guards — the code is untouched and still waiting on a scan.
                // Reported by the pane's own long-lived watcher instead.
                Ok(PairingEvent::Unpaired(_)) => continue,
                // Fell behind the ring; the pairing is still in the device list,
                // so keep listening rather than giving up.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .detach();
}

/// Repaint once a second while the window opened as `epoch` is live, and retire it
/// the moment that window closes.
///
/// The epoch check is what keeps one ticker per window: a `Pair another` opens a
/// new `Waiting` state, and without it the previous ticker would see `Waiting`
/// again and keep running alongside the new one, so every window opened in a
/// session would leave another timer repainting each second.
fn start_countdown(cx: &mut gpui::Context<SettingsModal>, epoch: u64) {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = this.update(cx, |this, cx| {
                if this.remote_pairing_epoch != epoch {
                    return false;
                }
                let Some(PairingState::Waiting { expires_at, .. }) = &this.remote_pairing else {
                    return false;
                };
                if now_secs() < *expires_at {
                    cx.notify();
                    return true;
                }
                // Expire in both places at once. Dropping the host's slot here
                // rather than leaning on its own check keeps a lapsed code from
                // lingering as something a later scan could still race.
                if let Some(rc) = cx.try_global::<RemoteControl>() {
                    rc.close_pairing();
                }
                this.remote_pairing = Some(PairingState::Expired);
                cx.notify();
                false
            });
            if !matches!(alive, Ok(true)) {
                break;
            }
        }
    })
    .detach();
}

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
/// still binding (on, not yet bound), or what is currently exposed and to how many
/// devices (on, ready). Pure so it can be unit-tested.
///
/// Carries no pairing instruction. It used to end with "turn remote access off and
/// on to pair another device", which described a workaround rather than a state;
/// the pairing row below is the affordance that sentence stood in for. Never
/// mentions the handshake secret.
fn status_text(enabled: bool, host_ready: bool, devices: usize, exposed: usize) -> String {
    if !enabled {
        return "Turn on to expose your running agent sessions to the OxiMux mobile app. \
                Applies to sessions started after it's enabled."
            .to_string();
    }
    if !host_ready {
        return "Starting the host — this takes a moment.".to_string();
    }
    let exposure = match exposed {
        0 => "no agent sessions are running yet".to_string(),
        1 => "1 running agent session is exposed".to_string(),
        n => format!("{n} running agent sessions are exposed"),
    };
    let paired = match devices {
        0 => "No devices paired yet".to_string(),
        1 => "1 device paired".to_string(),
        n => format!("{n} devices paired"),
    };
    format!("{paired} · {exposure}.")
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
    // The sub-view replaces the pane rather than layering over it: the modal is
    // already a dialog, and a second floating layer on top of it would be one more
    // than this surface should ever stack.
    if modal.remote_pairing.is_some() {
        return pairing_view(modal, theme, density, typography, cx);
    }

    let ticket = pairing_ticket(cx);
    let devices = cx.try_global::<RemoteControl>().map(|rc| rc.paired_devices()).unwrap_or_default();
    let status = status_text(enabled(cx), ticket.is_some(), devices.len(), exposed_count(cx));

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

    // The consent question in one line, kept next to the switch that answers it.
    // The full detail moved to the foot of the pane: it used to sit here as five
    // lines of prose because it had to precede the pairing code, but the code
    // lives in the sub-view now, and a wall of grey between the status and the
    // primary action pushed that action to the bottom of the pane. What has to be
    // read before flipping the switch is that an address gets published, and that
    // is what stays.
    col = col.child(
        div()
            .pt(px(6.0))
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child("Turning this on publishes this Mac's address to a public discovery service."),
    );

    // The action sits above the inventory it adds to: pair a device, then watch it
    // appear in the list underneath. Below the list it would read as a footnote to
    // the devices rather than the way you get one.
    col = col.child(pair_row(devices.is_empty(), theme, density, typography, cx));

    // Paired devices + one-tap revoke. Listed whether or not remote is on, so a
    // device can be cut off without first turning remote access back on. The
    // local CLI appears here as a synthetic row while enabled, so "who can
    // control my sessions" stays answerable in this one list.
    let local_on =
        cx.try_global::<RemoteControl>().is_some_and(|rc| rc.local_enabled());
    if !devices.is_empty() || local_on {
        col = col.child(devices_section(devices, local_on, theme, density, typography, cx));
    }

    // Reference material, below the things you came here to do: what the network
    // exposure actually is, and which host this is. Neither is re-read on a normal
    // visit, and both are here when the one-liner above prompts the question.
    col = col.child(network_disclosure(theme, typography));
    if let Some(ticket) = ticket {
        col = col.child(
            div()
                .pt(px(8.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(format!("Host {}", short_endpoint_id(&ticket.endpoint_id))),
        );
    }
    col.into_any_element()
}

/// The pairing affordance: a full-width navigation row, deliberately not a filled
/// accent button.
///
/// Every other control in this pane changes a setting in place; this one goes
/// somewhere, and the chevron is what says so. An accent button would also read as
/// "do this now", which is wrong for maintenance a user performs a handful of
/// times — except when nothing is paired yet, where it genuinely is the next step,
/// so only the subtitle leans in.
fn pair_row(
    first_device: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let subtitle = if first_device {
        "Scan a code from the mobile app to connect your first device"
    } else {
        "Scan a code or paste a link into the mobile app"
    };
    let row = div()
        .id("remote-pair-device")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .w_full()
        .py(px(10.0))
        .cursor_pointer()
        .group("pair-row")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, window, cx| begin_pairing(this, window, cx)),
        )
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(16.0))
                .flex_none()
                .text_color(theme.fg_subtle),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .font_weight(typography.w_medium)
                        .text_color(theme.fg_base)
                        .child("Pair a device"),
                )
                .child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(subtitle),
                ),
        )
        // The affordance that says "this navigates" rather than "this toggles".
        // A literal '›' glyph rendered at a fraction of the icon's weight and
        // sat off the optical centre; the asset matches the rest of the app.
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .size(px(14.0))
                .flex_none()
                .text_color(theme.fg_muted),
        )
        .into_any_element();

    div()
        .pt(px(16.0))
        .child(card_surface(theme, density, row))
        .into_any_element()
}

/// Enter the pairing sub-view, turning remote access on first if it is off.
///
/// Enabling from here rather than disabling the row is deliberate: "Pair a device"
/// is an unambiguous statement of intent, and a row that refuses until you find
/// the toggle above it would be a worse answer to that intent than simply doing
/// what it says. The consent this carries — publishing addresses to a public
/// discovery service — travels into the sub-view with it.
fn begin_pairing(
    this: &mut SettingsModal,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<SettingsModal>,
) {
    if !enabled(cx) {
        if let Some(rc) = cx.try_global::<RemoteControl>() {
            rc.set_enabled(true);
        }
        this.persist_flag(crate::remote_control::ENABLED_SETTING, true, cx);
        // Shown while the endpoint binds; the bind completion opens the window.
        this.remote_pairing = Some(PairingState::Starting);
        start_host(this, cx, true);
        cx.notify();
        return;
    }
    // Enabled already — but the bind may still be in flight, in which case there
    // is no host to open a window on yet and the earlier bind will not do it.
    if cx.try_global::<RemoteControl>().and_then(|rc| rc.pairing_ticket()).is_none() {
        this.remote_pairing = Some(PairingState::Starting);
        start_host(this, cx, true);
        cx.notify();
        return;
    }
    open_window(this, cx);
    cx.notify();
}

/// How long the code has left, as `m:ss`. Pure so it can be unit-tested.
fn countdown_label(remaining_secs: u64) -> String {
    format!("Code expires in {}:{:02}", remaining_secs / 60, remaining_secs % 60)
}

/// The pairing sub-view: a code to scan, and then what happened.
///
/// It has to survive being unattended. Between opening this and the device
/// appearing, the user is looking at their phone — so the moment a device lands
/// this becomes a confirmation naming it, rather than a code that silently blinks
/// out. `Pair another` is the same reasoning applied twice: setting up two phones
/// should be two scans, not two trips through the pane.
fn pairing_view(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let mut col = div().flex().flex_col().w_full().child(
        div()
            .id("remote-pairing-back")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .pb(px(12.0))
            .cursor_pointer()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .hover(|s| s.text_color(theme.fg_base))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    close_pairing(this, cx);
                    cx.notify();
                }),
            )
            // `text_color` on the parent does not reach an `svg()` — it paints
            // from its own colour, and without one it renders as nothing at all
            // (a blank gap where the arrow should be, not a faint arrow).
            .child(
                svg()
                    .path("icons/arrow-left.svg")
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.fg_subtle),
            )
            .child("Pair a device"),
    );

    match &modal.remote_pairing {
        None => {}
        Some(PairingState::Starting) => {
            col = col.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("Starting the host — the code will appear here in a moment."),
            );
        }
        Some(PairingState::Waiting { ticket, expires_at, note }) => {
            // Centred as a column: the code is the one thing on this screen, and
            // the instruction and countdown belong under it rather than beside a
            // left-aligned block of white.
            let mut body = div().flex().flex_col().items_center().w_full().gap(px(10.0));
            if let Some(image) = pairing_qr_image(modal, ticket) {
                body = body.child(
                    div()
                        .flex_none()
                        .p(px(8.0))
                        .rounded(px(density.r_xs))
                        // The code is painted black-on-white regardless of theme —
                        // an inverted QR is unreadable to many scanners.
                        .bg(gpui::white())
                        .child(img(ImageSource::Image(image)).size(px(QR_SIZE))),
                );
            }
            col = col.child(
                body.child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child("Scan this from the OxiMux app on your phone."),
                )
                .child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_muted)
                        .child(countdown_label(expires_at.saturating_sub(now_secs()))),
                )
                .when_some(note.clone(), |c, note| {
                    c.child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.status_info)
                            .child(note),
                    )
                })
                .child(copy_link_row(ticket, theme, density, typography, cx)),
            );
        }
        Some(PairingState::Paired { name }) => {
            col = col.child(
                // Centred and given room, so the outcome reads from across a desk
                // — this is what the user looks back to the Mac to check after
                // scanning, and a left-aligned line of text does not carry that.
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .w_full()
                    .py(px(28.0))
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(44.0))
                            .rounded_full()
                            // A soft disc of the ok hue rather than a filled
                            // badge: the same colour the rest of the app uses for
                            // a good outcome, at a weight that reads as a
                            // confirmation and not an alert.
                            .bg(theme.status_ok.opacity(0.16))
                            .child(
                                svg()
                                    .path("icons/check.svg")
                                    .size(px(22.0))
                                    .flex_none()
                                    .text_color(theme.status_ok),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(typography.t_body_md))
                            .font_weight(typography.w_medium)
                            .text_color(theme.fg_base)
                            .child(format!("{name} paired")),
                    )
                    .child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            // Naming the tier here is the point of confirming at
                            // all: a scan grants full access, and this is where a
                            // user who did not intend that can still catch it.
                            .child("Full access — narrow it to read-only in the list below."),
                    )
                    .child(
                        div()
                            .pt(px(6.0))
                            .flex()
                            .flex_row()
                            .gap(px(density.gap_inline))
                            .child(value_chip(
                                "remote-pair-another",
                                "Pair another",
                                theme,
                                density,
                                typography,
                                |this, _window, cx| {
                                    open_window(this, cx);
                                    cx.notify();
                                },
                                cx,
                            ))
                            .child(done_chip(theme, density, typography, cx)),
                    ),
            );
        }
        Some(PairingState::Expired) => {
            col = col
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child("That code expired."),
                )
                .child(
                    div()
                        .pt(px(4.0))
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child("Codes are short-lived so one left on screen stops working."),
                )
                .child(
                    div()
                        .pt(px(12.0))
                        .flex()
                        .flex_row()
                        .gap(px(density.gap_inline))
                        .child(value_chip(
                            "remote-pair-new-code",
                            "New code",
                            theme,
                            density,
                            typography,
                            |this, _window, cx| {
                                open_window(this, cx);
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(done_chip(theme, density, typography, cx)),
                );
        }
    }
    col.into_any_element()
}

/// Leaves the sub-view, retiring the code with it.
fn done_chip(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    value_chip(
        "remote-pair-done",
        "Done",
        theme,
        density,
        typography,
        |this, _window, cx| {
            close_pairing(this, cx);
            cx.notify();
        },
        cx,
    )
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
    local_on: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();

    // The local CLI's synthetic row: not a paired device (no pubkey, no
    // durable record — its lifetime IS the toggle), but it belongs in the
    // one list that answers "who can control my sessions". Disabling here is
    // its revocation: the listener and every live CLI connection drop.
    if local_on {
        rows.push(
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
                                .child("This computer — oximux CLI"),
                        )
                        .child(
                            div()
                                .text_size(px(typography.t_sub_label))
                                .text_color(theme.fg_subtle)
                                .child("local socket · full access while enabled"),
                        ),
                )
                .child(value_chip(
                    "local-cli-disable",
                    "Disable",
                    theme,
                    density,
                    typography,
                    move |this, _window, cx| {
                        if let Some(rc) = cx.try_global::<RemoteControl>() {
                            rc.stop_local();
                        }
                        this.persist_flag(
                            crate::remote_control::LOCAL_ENABLED_SETTING,
                            false,
                            cx,
                        );
                        cx.notify();
                    },
                    cx,
                ))
                .into_any_element(),
        );
    }

    for (idx, device) in devices.into_iter().enumerate() {
        let oximux_remote_host::DeviceInfo { pubkey, name, read_only, last_seen, revoked } = device;
        // One pubkey per closure below; each needs its own copy.
        let revoke_key = pubkey;
        let forget_key = pubkey;
        rows.push(
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
                })
                .into_any_element(),
        );
    }

    // Titled + carded, matching the toggles above: each cluster reads as its own
    // panel instead of a bare list floating on the pane body.
    div()
        .flex()
        .flex_col()
        .w_full()
        .pt(px(16.0))
        .gap(px(6.0))
        .child(section_title("Paired devices", "", theme, typography))
        .child(card_surface(theme, density, section_card(theme, density, rows)))
        .into_any_element()
}

pub(super) fn entries(
    _modal: &SettingsModal,
    theme: Theme,
    _density: Density,
    _typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![
        entry(
            "Allow remote access",
            "Expose running agent sessions so the OxiMux mobile app can view and drive them.",
            remote_toggle(theme, cx),
        ),
        entry(
            "Keep this Mac awake while on",
            "Stop it idle-sleeping, so a paired device can still reach it. Sleeping with the lid closed is unaffected.",
            keep_awake_toggle(theme, cx),
        ),
        // Deliberately names who else can reach it. The socket is owner-only,
        // which is easy to read as "only me" — but every program running as this
        // account qualifies, agents included, and that is the part worth knowing
        // before leaving it on. Kept to roughly the length of the descriptions
        // above: this row renders in a flex row without `min_w_0`, so prose that
        // outgrows its neighbours is the kind that stops wrapping cleanly.
        entry(
            "Allow local CLI access",
            "Let the oximux command view and drive sessions over a local socket, never \
             the network. Any program running as you — agents included — can use it too.",
            local_toggle(theme, cx),
        ),
    ]
}

/// Local CLI access — its own switch beside remote, never implied by it.
fn local_toggle(theme: Theme, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    let current =
        cx.try_global::<RemoteControl>().is_some_and(|rc| rc.local_enabled());
    toggle_switch("local-cli-enabled", current, theme, on_local_toggle, cx)
}

/// Flip local CLI access: on binds the control socket (minting a fresh token);
/// off tears down the listener and every live CLI connection with it.
fn on_local_toggle(
    this: &mut SettingsModal,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<SettingsModal>,
) {
    let Some(turning_on) =
        cx.try_global::<RemoteControl>().map(|rc| !rc.local_enabled())
    else {
        return;
    };
    if turning_on {
        RemoteControl::start_local(cx);
    } else if let Some(rc) = cx.try_global::<RemoteControl>() {
        rc.stop_local();
    }
    this.persist_flag(crate::remote_control::LOCAL_ENABLED_SETTING, turning_on, cx);
    cx.notify();
}

/// The keep-awake companion. Reads the live flag on the shared assertion rather
/// than a copy on the modal, so it reflects what is actually in force.
fn keep_awake_toggle(theme: Theme, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    let current = crate::agent_awake::global().remote_enabled();
    toggle_switch(
        "remote-keep-awake",
        current,
        theme,
        move |this, _window, cx| {
            let next = !current;
            crate::agent_awake::global().set_remote_enabled(next);
            this.persist_flag(crate::remote_control::KEEP_AWAKE_SETTING, next, cx);
            cx.notify();
        },
        cx,
    )
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
    let Some(turning_on) = cx.try_global::<RemoteControl>().map(|rc| {
        let turning_on = !rc.enabled();
        rc.set_enabled(turning_on);
        if !turning_on {
            rc.stop_host();
        }
        turning_on
    }) else {
        return;
    };

    // Persist the choice so it survives a relaunch rather than silently reverting.
    this.persist_flag(crate::remote_control::ENABLED_SETTING, turning_on, cx);

    if turning_on {
        // Bind only. A bind admits no new device on its own — pairing is the
        // separate, explicit act in `begin_pairing`.
        start_host(this, cx, false);
    } else {
        // Turning remote access off takes the pairing view with it; a code with no
        // host behind it is a QR that cannot resolve to anything.
        close_pairing(this, cx);
    }
    cx.notify();
}

/// Bind the iroh endpoint off-thread and fold the handle back in on the UI thread.
/// When `then_pair`, opens a pairing window the instant the host is up and moves
/// the sub-view from `Starting` into `Waiting`.
///
/// Watching for the pairing itself is [`open_window`]'s job, not this one's — see
/// [`watch_for_pairing`] for why that distinction matters.
fn start_host(this: &mut SettingsModal, cx: &mut gpui::Context<SettingsModal>, then_pair: bool) {
    // Scope the global borrow so `cx` is free for the async bridge below.
    let Some((dispatcher, secret, endpoint_secret)) = cx.try_global::<RemoteControl>().map(|rc| {
        let (dispatcher, secret) = rc.prepare_host();
        (dispatcher, secret, rc.endpoint_secret())
    }) else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let _ = this;

    // Bind on the tokio runtime (iroh needs it), then publish the handle back.
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle.spawn(async move {
        let _ = tx.send(oximux_remote_iroh::start_host(dispatcher, secret, endpoint_secret).await);
    });
    cx.spawn(async move |this, cx| {
        if let Ok(Ok(host)) = rx.await {
            let _ = this.update(cx, |this, cx| {
                if let Some(rc) = cx.try_global::<RemoteControl>() {
                    rc.set_host(host);
                }
                // Only if the user is still waiting on it — they may have left the
                // sub-view during the bind, and minting a code for a view nobody is
                // looking at is the thing this design exists to avoid.
                if then_pair && matches!(this.remote_pairing, Some(PairingState::Starting)) {
                    open_window(this, cx);
                }
                cx.notify();
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{countdown_label, last_seen_label, short_endpoint_id, status_text};

    #[test]
    fn status_reflects_enablement_host_readiness_and_count() {
        assert!(
            status_text(false, false, 1, 3).starts_with("Turn on"),
            "disabled explains enabling",
        );
        assert!(
            status_text(true, false, 1, 0).contains("Starting the host"),
            "on but host not yet bound",
        );
        assert!(
            status_text(true, true, 1, 0).contains("no agent sessions are running"),
            "ready, none running",
        );
        assert!(status_text(true, true, 1, 1).contains("1 running agent session"), "singular");
        assert!(status_text(true, true, 1, 4).contains("4 running agent sessions"), "plural");
    }

    /// The status line reports how many devices are paired and stops there. The
    /// old copy ended by telling the user to toggle remote access off and on to
    /// pair another — a workaround for a missing affordance, which the pairing row
    /// now provides. Leaving that sentence in place would send users back to the
    /// path that drops every connected device.
    #[test]
    fn status_counts_devices_without_prescribing_a_toggle() {
        assert!(status_text(true, true, 0, 1).starts_with("No devices paired yet"));
        assert!(status_text(true, true, 1, 1).starts_with("1 device paired"), "singular");

        let two = status_text(true, true, 2, 2);
        assert!(two.starts_with("2 devices paired"), "plural: {two}");
        assert!(two.contains("2 running agent sessions"), "still reports exposure");
        assert!(!two.contains("off and on"), "never prescribes a toggle cycle: {two}");
    }

    #[test]
    fn the_countdown_reads_as_minutes_and_padded_seconds() {
        assert_eq!(countdown_label(300), "Code expires in 5:00");
        assert_eq!(countdown_label(272), "Code expires in 4:32");
        assert_eq!(countdown_label(9), "Code expires in 0:09");
        assert_eq!(countdown_label(0), "Code expires in 0:00");
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
