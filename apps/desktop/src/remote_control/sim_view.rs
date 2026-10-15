//! A paired phone watching a device: the desktop's half of `SimViewables`
//! and `SimSubscribe`.
//!
//! What has to hold, and where it is enforced:
//! - **Off unless turned on.** Settings' "Let paired phones view the Mobile
//!   Emulator" is stored in the app's database (agents' own tools can write
//!   `simulator.toml`), and mirrored into [`PhoneShare::on`] so the pump's
//!   per-frame check never waits on the UI thread.
//! - **Asked per phone and device, once per run.** The first subscribe to a
//!   device raises a toast with Allow and Deny and answers `ConsentPending`;
//!   the phone asks again. An allow lasts until OxiMux quits and is never
//!   saved. A real device's question says what its screen can show.
//! - **Visible while it lasts.** The status bar says "Viewed from …" while
//!   any phone watches; clicking it stops sharing (every live view ends, and
//!   every phone has to be allowed again, but is not asked for a minute: a
//!   phone whose view ended subscribes again by itself).
//! - **A watcher is a viewer.** The phone counts as a shown viewer of the
//!   device at the reduced profile, so a watched device is never paused or
//!   parked, and the desktop's own panel, when it shows the device too, still
//!   wins the stream's quality.
//!
//! The frames themselves never touch the UI thread: the session hands them to
//! its [`EncodedTap`](oximux_simulator::encoded_tap::EncodedTap), and the
//! pump reads them from a [`PhoneFeed`] on the host's runtime.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Global};
use oximux_remote_host::{SimViewFeed, SimViewOut, SimViewSource, SimViewer};
use oximux_remote_proto::simulator::{SimErrorWire, SimViewClassWire, SimViewKind, SimViewableWire};
use oximux_simulator::encoded_tap::{TapKind, TapReceiver};
use oximux_simulator::stream::FrameData;
use oximux_simulator::viewers::{StreamProfile, Viewer, ViewerId};
use oximux_simulator::{DeviceId, Platform};
use tokio::sync::{mpsc, oneshot};

use crate::shell::chrome::toast::{ToastAction, ToastKind, toast, toast_with_actions};

/// The phone's feed rate: about 0.8 Mbit/s, whatever the desktop streams at.
pub const PHONE_BITS_PER_SEC: u64 = 800_000;

/// How often a phone that cannot decode (it just joined, or lost a frame)
/// asks again for a key frame. Android's is longer: a key frame there
/// restarts the device's encoder, which the desktop's own panel sees too.
const KEY_EVERY: Duration = Duration::from_secs(2);
const KEY_EVERY_ANDROID: Duration = Duration::from_secs(5);

/// How long the feed waits for a frame before looking at its key-frame and
/// end-of-session state again.
const POLL: Duration = Duration::from_millis(250);

/// A question nobody answered expires after this (the toast's own
/// lifetime): the phone is told so, and only its next explicit try asks
/// again, so an unattended desktop is not shown the question over and over.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(20);

/// After a Deny, the phone is refused for this long without asking.
const DENY_COOLDOWN: Duration = Duration::from_secs(60);

/// Where the user stands on one phone watching one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grant {
    Asked(Instant),
    Allowed,
    Denied(Instant),
}

/// One live view, for the status bar.
#[derive(Clone, Debug)]
struct LiveView {
    phone: String,
}

/// The sharing state both threads read: the UI thread changes it, and the
/// pump asks it before every frame.
#[derive(Default)]
pub struct PhoneShare {
    /// Settings' switch.
    on: AtomicBool,
    /// Bumped by Stop sharing: a feed from an older epoch ends.
    epoch: AtomicU64,
    grants: Mutex<HashMap<([u8; 32], DeviceId), Grant>>,
    live: Mutex<BTreeMap<u64, LiveView>>,
    /// When the user last clicked Stop sharing: for a while after, a phone
    /// is refused as after a Deny, not asked again (its view just ended, and
    /// it subscribes again on its own).
    stopped_at: Mutex<Option<Instant>>,
}

impl PhoneShare {
    fn grant(&self, phone: &[u8; 32], device: &DeviceId) -> Option<Grant> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner).get(&(*phone, device.clone())).copied()
    }

    fn set_grant(&self, phone: [u8; 32], device: DeviceId, grant: Grant) {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner).insert((phone, device), grant);
    }

    /// The toast's answer, unless sharing stopped or was turned off since
    /// the question was asked: an old toast must not grant a new run.
    fn answer(&self, phone: [u8; 32], device: DeviceId, asked_at: u64, grant: Grant) {
        let current = self.on.load(Ordering::Acquire) && self.epoch.load(Ordering::Acquire) == asked_at;
        tracing::info!(device = %device, allowed = grant == Grant::Allowed, current, "the user answered a phone's ask to watch");
        if current {
            self.set_grant(phone, device, grant);
        }
    }

    /// Whether a feed begun at `epoch` may go on.
    fn allows(&self, phone: &[u8; 32], device: &DeviceId, epoch: u64) -> bool {
        self.on.load(Ordering::Acquire)
            && self.epoch.load(Ordering::Acquire) == epoch
            && self.grant(phone, device) == Some(Grant::Allowed)
    }

    /// Settings' switch changed (also at launch, from the database).
    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Release);
        if !on {
            self.stop();
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// Stop sharing, as the user asked: [`Self::stop`], and no new question
    /// for [`DENY_COOLDOWN`].
    fn stop_by_user(&self, now: Instant) {
        self.stop();
        *self.stopped_at.lock().unwrap_or_else(PoisonError::into_inner) = Some(now);
    }

    /// How long a phone must still wait after the user stopped sharing.
    fn stopped_for(&self, now: Instant) -> Option<Duration> {
        let at = (*self.stopped_at.lock().unwrap_or_else(PoisonError::into_inner))?;
        DENY_COOLDOWN.checked_sub(now.saturating_duration_since(at)).filter(|left| !left.is_zero())
    }

    /// End every live view and forget every allow.
    fn stop(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.forget_grants();
    }

    fn forget_grants(&self) {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// The status bar's label while any phone watches: "Viewed from X",
    /// or how many phones.
    pub fn viewed_from(&self) -> Option<String> {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let mut phones: Vec<&str> = live.values().map(|v| v.phone.as_str()).collect();
        phones.sort_unstable();
        phones.dedup();
        match phones.as_slice() {
            [] => None,
            [one] => Some(format!("Viewed from {one}")),
            many => Some(format!("Viewed from {} phones", many.len())),
        }
    }
}

/// The app's one [`PhoneShare`].
#[derive(Clone, Default)]
pub struct PhoneViewing(pub Arc<PhoneShare>);

impl Global for PhoneViewing {}

/// The shared state, created on first use.
pub fn share(cx: &mut App) -> Arc<PhoneShare> {
    if cx.try_global::<PhoneViewing>().is_none() {
        cx.set_global(PhoneViewing::default());
    }
    cx.global::<PhoneViewing>().0.clone()
}

/// The status bar's "Viewed from …" label, when a phone watches.
pub fn viewed_from(cx: &App) -> Option<String> {
    cx.try_global::<PhoneViewing>().and_then(|v| v.0.viewed_from())
}

/// Stop sharing: every live view ends within a second, and every phone has
/// to be allowed again.
pub fn stop_sharing(cx: &mut App) {
    if let Some(viewing) = cx.try_global::<PhoneViewing>() {
        viewing.0.stop_by_user(Instant::now());
    }
    cx.refresh_windows();
}

/// One view RPC crossing to the UI thread.
pub enum ViewRequest {
    List { viewer: SimViewer, reply: oneshot::Sender<Result<Vec<SimViewableWire>, SimErrorWire>> },
    Subscribe { viewer: SimViewer, device: String, reply: oneshot::Sender<Result<SimViewFeed, SimErrorWire>> },
}

/// A feed ended: release its viewer on the UI thread.
struct Release {
    worktree: PathBuf,
    viewer: u64,
}

/// The dispatcher's end of the view bridge.
pub fn view_bridge() -> (mpsc::Sender<ViewRequest>, mpsc::Receiver<ViewRequest>) {
    mpsc::channel(8)
}

fn sharing_off() -> SimErrorWire {
    SimErrorWire::Refused(
        "Viewing is off on the desktop. Turn on \u{201c}Let paired phones view the Mobile Emulator\u{201d} in Settings \u{2192} Mobile Emulator."
            .into(),
    )
}

fn class_of(udid: &DeviceId) -> SimViewClassWire {
    match (udid.platform(), udid.is_physical()) {
        (Platform::Ios, false) => SimViewClassWire::IosSimulator,
        (Platform::Ios, true) => SimViewClassWire::Iphone,
        (Platform::Android, false) => SimViewClassWire::AndroidEmulator,
        (Platform::Android, true) => SimViewClassWire::AndroidPhone,
    }
}

/// A worktree as the phone lists it: its folder's name.
fn worktree_label(worktree: &std::path::Path) -> String {
    worktree.file_name().map_or_else(|| worktree.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Every attached device, as the phone lists it.
fn list(cx: &mut App) -> Result<Vec<SimViewableWire>, SimErrorWire> {
    if !share(cx).is_on() {
        return Err(sharing_off());
    }
    let Some(hub) = crate::shell::simulator::hub(cx) else { return Ok(Vec::new()) };
    let hub = hub.read(cx);
    let mut rows: Vec<SimViewableWire> = hub
        .attachments()
        .into_iter()
        .map(|(worktree, udid)| {
            let name = hub.attached_info(&worktree).and_then(|(_, info)| info).map_or_else(|| udid.as_str().to_owned(), |i| i.name);
            SimViewableWire {
                device: udid.as_str().to_owned(),
                name,
                worktree: worktree_label(&worktree),
                class: class_of(&udid),
                streaming: hub.session(&udid).is_some(),
            }
        })
        .collect();
    rows.sort_by(|a, b| (&a.worktree, &a.name).cmp(&(&b.worktree, &b.name)));
    Ok(rows)
}

/// Ask the user whether `viewer` may watch `name`.
fn ask(viewer: &SimViewer, udid: &DeviceId, name: &str, worktree: &str, share: &Arc<PhoneShare>, cx: &mut App) {
    let mut text = format!(
        "\u{201c}{}\u{201d} wants to watch {name} ({worktree}) from the paired phone. It can only watch, never control. Allowed until OxiMux quits.",
        viewer.phone_name
    );
    if udid.is_physical() {
        text.push_str(" This is a real device: whatever its screen shows, notifications and codes included, shows on the phone.");
    }
    let asked_at = share.epoch.load(Ordering::Acquire);
    let (allow_share, deny_share) = (share.clone(), share.clone());
    let (allow_key, deny_key) = ((viewer.phone, udid.clone()), (viewer.phone, udid.clone()));
    let allow = ToastAction::new("Allow", move |cx| {
        allow_share.answer(allow_key.0, allow_key.1.clone(), asked_at, Grant::Allowed);
        cx.refresh_windows();
    });
    let deny = ToastAction::new("Deny", move |_| {
        deny_share.answer(deny_key.0, deny_key.1.clone(), asked_at, Grant::Denied(Instant::now()));
    });
    toast_with_actions(cx, ToastKind::Warning, text, vec![allow, deny]);
}

/// Start `viewer` watching `device`, or say why not.
fn subscribe(viewer: &SimViewer, device: &str, releases: &mpsc::UnboundedSender<Release>, cx: &mut App) -> Result<SimViewFeed, SimErrorWire> {
    let share = share(cx);
    if !share.is_on() {
        return Err(sharing_off());
    }
    let hub = crate::shell::simulator::hub(cx).ok_or(SimErrorWire::NoDevice)?;
    let (worktree, udid) = hub
        .read(cx)
        .attachments()
        .into_iter()
        .find(|(_, udid)| udid.as_str() == device)
        .ok_or_else(|| SimErrorWire::NotFound("that device is no longer attached on the desktop".into()))?;
    let session = hub.read(cx).session(&udid).ok_or(SimErrorWire::NotStreaming)?;
    // A JPEG stream has frames but never a config. (One with no frame yet
    // cannot tell; a later switch to JPEG leaves the phone's picture still.)
    let latest = session.latest_frame(0);
    if matches!(latest, Some((_, FrameData::Jpeg(_)))) || (latest.is_some() && session.tap().config().is_none()) {
        return Err(SimErrorWire::Refused(
            "The device streams JPEG. Switch its Encoding to H.264 in the panel's stream settings.".into(),
        ));
    }
    let now = Instant::now();
    if let Some(left) = share.stopped_for(now)
        && share.grant(&viewer.phone, &udid).is_none()
    {
        return Err(SimErrorWire::ConsentDenied { retry_after_secs: left.as_secs().max(1) });
    }
    match share.grant(&viewer.phone, &udid) {
        Some(Grant::Allowed) => {}
        Some(Grant::Denied(at)) if now.saturating_duration_since(at) < DENY_COOLDOWN => {
            let left = DENY_COOLDOWN.saturating_sub(now.saturating_duration_since(at));
            return Err(SimErrorWire::ConsentDenied { retry_after_secs: left.as_secs().max(1) });
        }
        Some(Grant::Asked(at)) if now.saturating_duration_since(at) < ASK_AGAIN_AFTER => {
            return Err(SimErrorWire::ConsentPending);
        }
        Some(Grant::Asked(_)) => {
            share.grants.lock().unwrap_or_else(PoisonError::into_inner).remove(&(viewer.phone, udid.clone()));
            return Err(SimErrorWire::Refused("Nobody answered on the desktop. Try again when someone is there.".into()));
        }
        _ => {
            share.set_grant(viewer.phone, udid.clone(), Grant::Asked(now));
            let name = hub.read(cx).attached_info(&worktree).and_then(|(_, i)| i).map_or_else(|| udid.as_str().to_owned(), |i| i.name);
            ask(viewer, &udid, &name, &worktree_label(&worktree), &share, cx);
            return Err(SimErrorWire::ConsentPending);
        }
    }
    let (width, height) = session.framebuffer_size().unwrap_or((0, 0));
    let rx = session.tap().subscribe(PHONE_BITS_PER_SEC);
    let notify = Arc::new(tokio::sync::Notify::new());
    let wake = notify.clone();
    rx.set_waker(move || wake.notify_one());
    hub.update(cx, |hub, cx| {
        hub.set_viewer(&worktree, ViewerId::Remote(viewer.id), Viewer { shown: true, profile: StreamProfile::Reduced }, cx);
    });
    share.live.lock().unwrap_or_else(PoisonError::into_inner).insert(viewer.id, LiveView { phone: viewer.phone_name.clone() });
    cx.refresh_windows();
    tracing::info!(device = %udid, viewer = viewer.id, "a paired phone started watching a device");
    let feed = PhoneFeed {
        key_every: if udid.platform() == Platform::Android { KEY_EVERY_ANDROID } else { KEY_EVERY },
        epoch: share.epoch.load(Ordering::Acquire),
        rx,
        notify,
        share,
        phone: viewer.phone,
        device: udid,
        release: Some((releases.clone(), Release { worktree, viewer: viewer.id })),
    };
    Ok(SimViewFeed { width, height, bits_per_sec: PHONE_BITS_PER_SEC, source: Box::new(feed) })
}

/// One phone's frames of one device.
struct PhoneFeed {
    rx: TapReceiver,
    notify: Arc<tokio::sync::Notify>,
    share: Arc<PhoneShare>,
    epoch: u64,
    /// How often to ask again for a key frame while unable to decode.
    key_every: Duration,
    phone: [u8; 32],
    device: DeviceId,
    release: Option<(mpsc::UnboundedSender<Release>, Release)>,
}

#[async_trait::async_trait]
impl SimViewSource for PhoneFeed {
    async fn next(&mut self) -> Option<SimViewOut> {
        loop {
            if let Some(frame) = self.rx.try_take() {
                let kind = match frame.kind {
                    TapKind::Config => SimViewKind::Config,
                    TapKind::Key => SimViewKind::Key,
                    TapKind::Delta => SimViewKind::Delta,
                };
                return Some(SimViewOut { kind, pts_us: frame.pts_us, payload: frame.data });
            }
            if self.rx.is_closed() {
                return None;
            }
            if self.rx.wants_key(Instant::now(), self.key_every) {
                self.rx.request_key();
            }
            // A permit left by a frame queued meanwhile returns at once.
            let _ = tokio::time::timeout(POLL, self.notify.notified()).await;
        }
    }

    fn allowed(&self) -> bool {
        self.share.allows(&self.phone, &self.device, self.epoch)
    }
}

impl Drop for PhoneFeed {
    fn drop(&mut self) {
        if let Some((tx, release)) = self.release.take() {
            let _ = tx.send(release);
        }
    }
}

/// Drain view requests and finished feeds on the UI thread.
pub fn serve_views(mut requests: mpsc::Receiver<ViewRequest>, cx: &mut App) {
    // Settings' switch, as saved.
    let on = crate::shell::simulator::hub(cx).is_some_and(|hub| hub.read(cx).phone_view_allowed());
    share(cx).set_on(on);
    let (releases_tx, mut releases) = mpsc::unbounded_channel::<Release>();
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    Some(ViewRequest::List { reply, .. }) => {
                        let _ = reply.send(cx.update(list));
                    }
                    Some(ViewRequest::Subscribe { viewer, device, reply }) => {
                        let _ = reply.send(cx.update(|cx| subscribe(&viewer, &device, &releases_tx, cx)));
                    }
                    None => break,
                },
                Some(Release { worktree, viewer }) = releases.recv() => {
                    cx.update(|cx| {
                        if let Some(viewing) = cx.try_global::<PhoneViewing>() {
                            viewing.0.live.lock().unwrap_or_else(PoisonError::into_inner).remove(&viewer);
                        }
                        if let Some(hub) = crate::shell::simulator::hub(cx) {
                            hub.update(cx, |hub, cx| hub.release_viewer(&worktree, ViewerId::Remote(viewer), cx));
                        }
                        cx.refresh_windows();
                    });
                    tracing::info!(viewer, "a paired phone stopped watching a device");
                }
            }
        }
    })
    .detach();
}

/// Settings turned viewing off: say what happened to anyone watching.
pub fn turned_off(cx: &mut App) {
    if viewed_from(cx).is_some() {
        toast(cx, ToastKind::Info, "Stopped sharing the Mobile Emulator with paired phones.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: [u8; 32] = [7; 32];

    fn device() -> DeviceId {
        DeviceId("E0E0".into())
    }

    #[test]
    fn a_feed_lives_only_while_on_allowed_and_not_stopped() {
        let share = PhoneShare::default();
        let epoch = share.epoch.load(Ordering::Acquire);
        assert!(!share.allows(&PHONE, &device(), epoch), "off by default");
        share.set_on(true);
        assert!(!share.allows(&PHONE, &device(), epoch), "on, but never allowed");
        share.set_grant(PHONE, device(), Grant::Allowed);
        assert!(share.allows(&PHONE, &device(), epoch));
        assert!(!share.allows(&[8; 32], &device(), epoch), "allowed per phone");
        share.stop();
        assert!(!share.allows(&PHONE, &device(), epoch), "Stop sharing ends a live feed");
        assert_eq!(share.grant(&PHONE, &device()), None, "and the phone must be allowed again");
        share.set_grant(PHONE, device(), Grant::Allowed);
        share.set_on(false);
        assert!(!share.allows(&PHONE, &device(), share.epoch.load(Ordering::Acquire)), "the switch off ends it");
        assert_eq!(share.grant(&PHONE, &device()), None, "and forgets the allows");
    }

    #[test]
    fn an_old_toast_cannot_grant_after_sharing_stopped() {
        let share = PhoneShare::default();
        share.set_on(true);
        let asked_at = share.epoch.load(Ordering::Acquire);
        share.stop();
        share.answer(PHONE, device(), asked_at, Grant::Allowed);
        assert_eq!(share.grant(&PHONE, &device()), None, "asked before Stop sharing");
        let asked_at = share.epoch.load(Ordering::Acquire);
        share.set_on(false);
        share.set_on(true);
        share.answer(PHONE, device(), asked_at, Grant::Allowed);
        assert_eq!(share.grant(&PHONE, &device()), None, "asked before the switch went off and on");
        let asked_at = share.epoch.load(Ordering::Acquire);
        share.answer(PHONE, device(), asked_at, Grant::Allowed);
        assert_eq!(share.grant(&PHONE, &device()), Some(Grant::Allowed));
    }

    #[test]
    fn after_stop_sharing_a_phone_waits_out_the_cooldown_before_being_asked() {
        let share = PhoneShare::default();
        share.set_on(true);
        let now = Instant::now();
        assert_eq!(share.stopped_for(now), None);
        share.stop_by_user(now);
        assert_eq!(share.stopped_for(now + Duration::from_secs(10)), Some(DENY_COOLDOWN - Duration::from_secs(10)));
        assert_eq!(share.stopped_for(now + DENY_COOLDOWN), None, "then the next try asks");
    }

    #[test]
    fn the_status_label_names_the_phone_or_counts_them() {
        let share = PhoneShare::default();
        assert_eq!(share.viewed_from(), None);
        share.live.lock().unwrap().insert(1, LiveView { phone: "Test phone".into() });
        share.live.lock().unwrap().insert(2, LiveView { phone: "Test phone".into() });
        assert_eq!(share.viewed_from().as_deref(), Some("Viewed from Test phone"), "one phone, two devices");
        share.live.lock().unwrap().insert(3, LiveView { phone: "Other".into() });
        assert_eq!(share.viewed_from().as_deref(), Some("Viewed from 2 phones"));
    }

    #[test]
    fn devices_are_classed_by_platform_and_kind() {
        assert_eq!(class_of(&DeviceId("E0E0A1DD-2DAC-448E-86D3-07B7F809BCD2".into())), SimViewClassWire::IosSimulator);
        assert_eq!(class_of(&DeviceId("avd:Medium_Phone".into())), SimViewClassWire::AndroidEmulator);
        assert_eq!(class_of(&DeviceId("adb:R5CT1234".into())), SimViewClassWire::AndroidPhone);
    }
}
