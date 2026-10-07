//! The hub's side of pairing Android phones over Wi-Fi (see
//! `oximux_simulator::android::wifi`): one pairing at a time, run off the UI
//! thread, its progress read by the panel's pairing card.
//!
//! A pairing that connects records the phone's mDNS instance against its
//! serial — in the database, never `simulator.toml` — so that transport joins
//! the phone's row. No address is remembered: the connect port changes every
//! time wireless debugging is turned on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{Context, EntityId};
use oximux_simulator::android::adb::Adb;
use oximux_simulator::android::devices::{connected_here, paired_phones};
use oximux_simulator::android::wifi::{self, Outcome, QrResult, QrSecret};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::{DeviceId, SimError};

use super::{HubEvent, SimulatorHub};
use crate::app_settings::sim_state_keys;

/// `ro.serialno` of a just-connected transport, and how often it is asked.
const SERIAL_TIMEOUT: Duration = Duration::from_secs(3);
const SERIAL_TRIES: usize = 3;

/// Where the pairing card stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PairStage {
    #[default]
    Idle,
    /// QR: waiting for the phone to scan the code.
    WaitingForScan,
    Pairing,
    Connecting,
    /// Paired, but the phone never advertised its connect port: ask for it
    /// (the port on the phone's Wireless debugging screen). `error`: the last
    /// port tried did not connect (the form stays, to try another).
    NeedConnectPort { host: String, error: Option<String> },
    /// Connected; the phone is in the device menu.
    Done { name: String },
    Failed(String),
}

/// The one pairing in progress, app-wide.
#[derive(Default)]
pub(crate) struct WifiPairing {
    /// The panel whose card started it: only that card shows it, submits to
    /// it or cancels it (another window's card leaves it alone).
    owner: Option<EntityId>,
    stage: PairStage,
    cancel: Arc<AtomicBool>,
    /// The QR code's payload while QR pairing runs (a secret: shown, never logged).
    qr: Option<String>,
    /// Whether adb's mDNS discovery runs (QR pairing needs it); `None` until checked.
    mdns: Option<bool>,
    /// The phone's mDNS instance adb named when it paired, kept for a connect
    /// that needs the user's port.
    guid: Option<String>,
}

/// How a background pairing step ended.
enum Step {
    /// Connected over `addr`; `instance` and `serial` are empty when they
    /// could not be read (the phone then works, as a row of its own).
    Connected { instance: String, serial: String, name: String, addr: String },
    NeedPort { host: String, guid: Option<String> },
    /// The port the user typed did not connect: ask again, still paired.
    PortFailed { host: String, guid: Option<String>, why: String },
    Failed(String),
}

/// Whether the card may start (or retry) a pairing now: never while one runs,
/// and not again once one succeeded — an Enter pressed twice must not send the
/// same code to a second `adb pair`.
pub(crate) fn can_submit(stage: &PairStage) -> bool {
    matches!(stage, PairStage::Idle | PairStage::Failed(_) | PairStage::NeedConnectPort { .. })
}

impl SimulatorHub {
    /// Where `owner`'s pairing stands (`Idle` when another card's runs).
    pub fn pair_stage(&self, owner: EntityId) -> PairStage {
        if self.wifi.owner == Some(owner) { self.wifi.stage.clone() } else { PairStage::Idle }
    }

    /// The QR payload to draw while `owner`'s QR pairing waits for a scan.
    pub fn pair_qr(&self, owner: EntityId) -> Option<&str> {
        self.wifi.qr.as_deref().filter(|_| self.wifi.owner == Some(owner))
    }

    /// `Some(false)`: adb's mDNS is off, so QR pairing cannot work.
    pub fn pair_mdns(&self) -> Option<bool> {
        self.wifi.mdns
    }

    /// Pair with the code the phone shows ("Pair device with pairing code"):
    /// `addr` is its `ip:port`. The code goes to adb on stdin.
    pub fn pair_with_code(&mut self, owner: EntityId, addr: String, code: String, cx: &mut Context<Self>) {
        let Some(adb) = self.begin_pairing(owner, PairStage::Pairing, cx) else { return };
        let cancel = self.wifi.cancel.clone();
        self.run_pairing(cx, move || {
            let adb = Adb::new(&SystemRunner, &adb);
            let guid = match adb.pair(&addr, &code)? {
                (Outcome::Ok, guid) => guid,
                (other, _) => return Ok(failed(other)),
            };
            let host = wifi::host_of(&addr).to_owned();
            // Closed while pairing: connect nothing nobody will record.
            if cancel.load(Ordering::Acquire) {
                return Err(SimError::Cancelled);
            }
            Ok(match wifi::connect_after_pairing(&adb, &host, &cancel)? {
                Some((service, outcome)) => connected(&adb, &service.addr, outcome, guid.or(Some(service.instance))),
                None => Step::NeedPort { host, guid },
            })
        });
    }

    /// Show a QR code for "Pair device with QR code" and pair once the phone
    /// scans it (≤ two minutes; closing the card cancels).
    pub fn pair_with_qr(&mut self, owner: EntityId, cx: &mut Context<Self>) {
        let Some(adb_path) = self.begin_pairing(owner, PairStage::WaitingForScan, cx) else { return };
        let secret = QrSecret::new();
        self.wifi.qr = Some(secret.payload());
        let cancel = self.wifi.cancel.clone();
        self.check_mdns(adb_path.clone(), cx);
        self.run_pairing(cx, move || {
            let adb = Adb::new(&SystemRunner, &adb_path);
            Ok(match wifi::pair_by_qr(&adb, &secret, &cancel)? {
                QrResult::Pairing(outcome) => failed(outcome),
                QrResult::Connected(service, outcome, guid) => connected(&adb, &service.addr, outcome, guid.or(Some(service.instance))),
                QrResult::PairedNotConnected(host, guid) => Step::NeedPort { host, guid },
            })
        });
    }

    /// Paired, but not connected: connect to `host:port` (the port the phone's
    /// Wireless debugging screen shows).
    pub fn connect_wifi(&mut self, owner: EntityId, host: String, port: String, cx: &mut Context<Self>) {
        // The instance adb named when it paired (this card's pairing).
        let guid = self.wifi.guid.clone().filter(|_| self.wifi.owner == Some(owner));
        let Some(adb) = self.begin_pairing(owner, PairStage::Connecting, cx) else { return };
        self.run_pairing(cx, move || {
            let adb = Adb::new(&SystemRunner, &adb);
            let addr = format!("{host}:{}", port.trim());
            // A wrong or stale port keeps the form, to try another.
            Ok(match connected(&adb, &addr, adb.connect(&addr)?, guid.clone()) {
                Step::Failed(why) => Step::PortFailed { host, guid, why },
                step => step,
            })
        });
    }

    /// `owner`'s card closed: stop its pairing, forget the QR secret. Another
    /// card's pairing is left alone.
    pub fn cancel_pairing(&mut self, owner: EntityId, cx: &mut Context<Self>) {
        if self.wifi.owner.is_some_and(|o| o != owner) {
            return;
        }
        self.wifi.cancel.store(true, Ordering::Release);
        self.wifi = WifiPairing { mdns: self.wifi.mdns, ..WifiPairing::default() };
        cx.emit(HubEvent::Pairing);
    }

    /// Whether `udid` is a phone OxiMux paired over Wi-Fi (the device menu
    /// offers to forget it).
    pub fn paired_over_wifi(udid: &DeviceId) -> bool {
        let Some(serial) = udid.as_str().strip_prefix("adb:") else { return false };
        paired_phones().lock().unwrap_or_else(std::sync::PoisonError::into_inner).values().any(|s| s == serial)
    }

    /// Disconnect a Wi-Fi phone and forget its pairing here. Unpairing it for
    /// good is done on the phone (Wireless debugging › Paired devices).
    pub fn forget_wifi(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let Some(serial) = udid.as_str().strip_prefix("adb:").map(str::to_owned) else { return };
        let instances: Vec<String> = {
            let mut paired = paired_phones().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let gone: Vec<String> = paired.iter().filter(|(_, s)| **s == serial).map(|(i, _)| i.clone()).collect();
            paired.retain(|_, s| *s != serial);
            sim_state_keys::save_wifi_paired(&self.repo, &paired);
            gone
        };
        // And the addresses OxiMux connected it under, attached or not.
        let addresses: Vec<String> = {
            let mut connected = connected_here().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let gone: Vec<String> = connected.iter().filter(|(_, s)| **s == serial).map(|(a, _)| a.clone()).collect();
            connected.retain(|_, s| *s != serial);
            gone
        };
        let Some(adb) = self.android_sdk.as_ref().map(|s| s.adb()) else { return };
        // The transport it streams over now (an address when connected by
        // one), and the mDNS names adb reconnects it under.
        let current = self.session(udid).and_then(|s| s.android().map(|a| a.serial().to_owned())).filter(|s| wifi::is_network_transport(s));
        let transports: Vec<String> = current
            .into_iter()
            .chain(addresses)
            .chain(instances.into_iter().map(|i| format!("{i}.{}", wifi::CONNECT_SERVICE)))
            .collect();
        cx.background_executor()
            .spawn(async move {
                let adb = Adb::new(&SystemRunner, &adb);
                for transport in transports {
                    let _ = adb.disconnect(&transport);
                }
            })
            .detach();
        self.refresh_devices(cx);
    }

    /// Start a pairing at `stage` (cancelling one in flight). `None` without
    /// an SDK.
    fn begin_pairing(&mut self, owner: EntityId, stage: PairStage, cx: &mut Context<Self>) -> Option<std::path::PathBuf> {
        self.wifi.cancel.store(true, Ordering::Release);
        let Some(adb) = self.android_sdk.as_ref().map(|s| s.adb()) else {
            self.wifi = WifiPairing { owner: Some(owner), mdns: self.wifi.mdns, ..WifiPairing::default() };
            self.wifi.stage = PairStage::Failed("The Android SDK (adb) was not found.".into());
            cx.emit(HubEvent::Pairing);
            return None;
        };
        self.wifi = WifiPairing { owner: Some(owner), stage, mdns: self.wifi.mdns, ..WifiPairing::default() };
        cx.emit(HubEvent::Pairing);
        Some(adb)
    }

    /// Run a pairing's blocking steps on a thread of their own (a QR wait
    /// lasts minutes) and land the result, unless it was cancelled.
    fn run_pairing(&mut self, cx: &mut Context<Self>, job: impl FnOnce() -> Result<Step, SimError> + Send + 'static) {
        let cancel = self.wifi.cancel.clone();
        cx.spawn(async move |this, cx| {
            let (tx, rx) = futures::channel::oneshot::channel();
            let spawned = std::thread::Builder::new().name("oximux-wifi-pairing".into()).spawn(move || {
                let _ = tx.send(job());
            });
            let step = match spawned {
                Ok(_) => rx.await.unwrap_or_else(|_| Ok(Step::Failed("the pairing stopped unexpectedly".into()))),
                Err(e) => Ok(Step::Failed(e.to_string())),
            };
            let _ = this.update(cx, |hub, cx| hub.land_if_current(&cancel, step, cx));
        })
        .detach();
    }

    /// Land a pairing's result unless it was cancelled meanwhile: the card was
    /// closed, or another pairing began (which cancels this one's flag).
    fn land_if_current(&mut self, cancel: &AtomicBool, step: Result<Step, SimError>, cx: &mut Context<Self>) {
        if !cancel.load(Ordering::Acquire) {
            self.land_pairing(step, cx);
        }
    }

    fn land_pairing(&mut self, step: Result<Step, SimError>, cx: &mut Context<Self>) {
        self.wifi.qr = None;
        self.wifi.stage = match step {
            Ok(Step::Connected { instance, serial, name, addr }) => {
                if !serial.is_empty() {
                    // This run's connection joins the phone's row now; adb's
                    // later reconnects (by instance) do from the database.
                    connected_here().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(addr, serial.clone());
                    if !instance.is_empty() {
                        let mut paired = paired_phones().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        paired.insert(instance, serial);
                        sim_state_keys::save_wifi_paired(&self.repo, &paired);
                    }
                }
                tracing::info!("phone paired over Wi-Fi");
                self.refresh_devices(cx);
                PairStage::Done { name }
            }
            Ok(Step::NeedPort { host, guid }) => {
                self.wifi.guid = guid;
                PairStage::NeedConnectPort { host, error: None }
            }
            Ok(Step::PortFailed { host, guid, why }) => {
                self.wifi.guid = guid;
                PairStage::NeedConnectPort { host, error: Some(why) }
            }
            Ok(Step::Failed(why)) => PairStage::Failed(why),
            Err(SimError::Cancelled) => PairStage::Idle,
            // Never the command line: nothing secret is on it, but the words
            // a person needs are adb's, not ours.
            Err(e) => PairStage::Failed(e.to_string()),
        };
        cx.emit(HubEvent::Pairing);
    }

    fn check_mdns(&mut self, adb: std::path::PathBuf, cx: &mut Context<Self>) {
        if self.wifi.mdns.is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let ok = cx.background_executor().spawn(async move { Adb::new(&SystemRunner, &adb).mdns_available() }).await;
            let _ = this.update(cx, |hub, cx| {
                hub.wifi.mdns = Some(ok);
                cx.emit(HubEvent::Pairing);
            });
        })
        .detach();
    }
}

/// A failed pairing or connection, in words for a person.
fn failed(outcome: Outcome) -> Step {
    Step::Failed(match outcome {
        Outcome::LocalNetworkBlocked => wifi::LOCAL_NETWORK_BLOCKED.into(),
        Outcome::Failed(why) => why,
        Outcome::Ok => "adb reported success but nothing happened".into(),
    })
}

/// Connected (or not) over `addr`: the phone's serial and name, read over the
/// new transport, so it joins the phone's row. `instance`: the mDNS name adb
/// gave the pairing, for its reconnects.
fn connected(adb: &Adb<'_>, addr: &str, outcome: Outcome, instance: Option<String>) -> Step {
    if outcome != Outcome::Ok {
        return failed(outcome);
    }
    // A fresh transport may not answer at once: a few tries, not a silent
    // "not paired".
    let serial = (0..SERIAL_TRIES)
        .find_map(|attempt| {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
            adb.getprop(addr, "ro.serialno", SERIAL_TIMEOUT).ok().filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    if serial.is_empty() {
        tracing::warn!("a phone connected over Wi-Fi did not say its serial: it shows as a row of its own");
    }
    let name = adb.getprop(addr, "ro.product.marketname", SERIAL_TIMEOUT).ok().filter(|n| !n.is_empty()).unwrap_or_else(|| addr.to_owned());
    Step::Connected { instance: instance.unwrap_or_default(), serial, name, addr: addr.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    /// An Enter pressed twice (the key handler and the focused button's own
    /// click) must not send the code to a second `adb pair`, nor re-send it
    /// after a success.
    #[test]
    fn a_pairing_is_submitted_once() {
        assert!(can_submit(&PairStage::Idle) && can_submit(&PairStage::Failed("x".into())));
        assert!(can_submit(&PairStage::NeedConnectPort { host: "h".into(), error: None }));
        for busy in [PairStage::Pairing, PairStage::Connecting, PairStage::WaitingForScan, PairStage::Done { name: "x".into() }] {
            assert!(!can_submit(&busy), "{busy:?}");
        }
    }

    /// A pairing cancelled (the card closed, or replaced by a new one) never
    /// lands, however late its job finishes; the current one does.
    #[gpui::test]
    fn only_the_current_pairing_lands(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let hub = cx.update(|cx| {
            super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), oximux_storage::SimApprovalRepo::new(db))
        });
        // Two cards' owners (any two distinct entity ids).
        let (me, other) = cx.update(|cx| (cx.new(|_| ()).entity_id(), cx.new(|_| ()).entity_id()));
        hub.update(cx, |hub, cx| {
            hub.android_sdk = Some(oximux_simulator::android::sdk::Sdk { root: "/nonexistent-sdk".into() });
            let late = || Ok(Step::Failed("late".into()));
            // Closed: the card's flag is set; the late result is dropped.
            hub.begin_pairing(me, PairStage::Pairing, cx);
            let first = hub.wifi.cancel.clone();
            hub.cancel_pairing(me, cx);
            hub.land_if_current(&first, late(), cx);
            assert_eq!(hub.pair_stage(me), PairStage::Idle);
            // Replaced: beginning a new pairing cancels the old one's flag.
            hub.begin_pairing(me, PairStage::Pairing, cx);
            let old = hub.wifi.cancel.clone();
            hub.begin_pairing(me, PairStage::Pairing, cx);
            assert!(old.load(Ordering::Acquire), "the old pairing is cancelled");
            hub.land_if_current(&old, late(), cx);
            assert_eq!(hub.pair_stage(me), PairStage::Pairing, "only the current one lands");
            // Another window's card neither sees nor cancels it.
            assert_eq!(hub.pair_stage(other), PairStage::Idle);
            hub.cancel_pairing(other, cx);
            assert_eq!(hub.pair_stage(me), PairStage::Pairing);
            let current = hub.wifi.cancel.clone();
            hub.land_if_current(&current, late(), cx);
            assert_eq!(hub.pair_stage(me), PairStage::Failed("late".into()));
            // A port that did not connect keeps the form, with why.
            hub.begin_pairing(me, PairStage::Connecting, cx);
            let flag = hub.wifi.cancel.clone();
            hub.land_if_current(&flag, Ok(Step::PortFailed { host: "h".into(), guid: Some("adb-x".into()), why: "refused".into() }), cx);
            assert_eq!(hub.pair_stage(me), PairStage::NeedConnectPort { host: "h".into(), error: Some("refused".into()) });
            assert_eq!(hub.wifi.guid.as_deref(), Some("adb-x"), "still paired as adb-x");
        });
    }
}
