//! Network shaping on Android (`sim net`): an emulator's speed through its
//! console, and offline/online through `svc` (an emulator or a phone).
//!
//! Every word is a constant, a preset from [`SPEED_PRESETS`] or a number:
//! `adb shell` joins its words for the device's `sh`. Offline turns both
//! radios off (Wi-Fi and mobile data); online turns both back on.
//!
//! The emulator shapes only its mobile data: Wi-Fi (netsim) traffic is
//! never slowed. So a speed below `full` also turns Wi-Fi off (measured:
//! 263 KB in 10 s at full, 2.9 KB at `gsm` once on mobile data, unchanged
//! on Wi-Fi), and `full` turns it back on.

use super::Target;
use super::state::Step;
use super::wifi::is_network_transport;
use crate::state::{NetChange, Reject, StateChange};

/// The speeds `emu network speed` names (the console's own list).
pub const SPEED_PRESETS: [&str; 9] = ["gsm", "hscsd", "gprs", "edge", "umts", "hsdpa", "lte", "evdo", "full"];

/// The largest speed taken as kbit/s (10 Gbit/s).
const MAX_KBPS: u64 = 10_000_000;

/// `speed` as the console word: a preset, or a positive kbit/s.
pub fn speed_word(speed: &str) -> Result<String, Reject> {
    let word = speed.trim().to_ascii_lowercase();
    if SPEED_PRESETS.contains(&word.as_str()) {
        return Ok(word);
    }
    match word.parse::<u64>() {
        Ok(kbps) if word.chars().all(|c| c.is_ascii_digit()) && (1..=MAX_KBPS).contains(&kbps) => Ok(kbps.to_string()),
        _ => Err(Reject::BadInput(format!("`{speed}` is not a network speed: one of {}, or kbit/s (1–{MAX_KBPS})", SPEED_PRESETS.join(", ")))),
    }
}

/// The calls that make `change` on `target`.
pub fn steps(target: &Target, change: &NetChange) -> Result<Vec<Step>, Reject> {
    let words = |w: &[&str]| Step::Shell(w.iter().map(|w| (*w).to_owned()).collect());
    Ok(match change {
        NetChange::Speed(_) if matches!(target, Target::Serial(_)) => {
            return Err(Reject::Unsupported("setting the network speed is available on an Android emulator only".into()));
        }
        NetChange::Speed(speed) => {
            let word = speed_word(speed)?;
            let wifi = if word == "full" { "enable" } else { "disable" };
            vec![Step::Console(vec!["emu".into(), "network".into(), "speed".into(), word]), words(&["svc", "wifi", wifi])]
        }
        NetChange::Offline => vec![words(&["svc", "wifi", "disable"]), words(&["svc", "data", "disable"])],
        NetChange::Online => vec![words(&["svc", "wifi", "enable"]), words(&["svc", "data", "enable"])],
    })
}

/// Whether `change` may run over the transport `serial` names: a phone
/// connected over Wi-Fi would cut its own connection going offline.
pub fn check_transport(serial: &str, change: &StateChange) -> Result<(), Reject> {
    if matches!(change, StateChange::Net(NetChange::Offline)) && is_network_transport(serial) {
        return Err(Reject::Unsupported(
            "going offline would cut adb's own Wi-Fi connection to this phone; connect it by USB first".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVD: fn() -> Target = || Target::Avd("Medium_Phone".into());
    const PHONE: fn() -> Target = || Target::Serial("R58".into());

    fn words(steps: Vec<Step>) -> Vec<String> {
        steps
            .into_iter()
            .map(|s| match s {
                Step::Console(w) | Step::Shell(w) => w.join(" "),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn an_emulators_speed_goes_to_its_console() {
        // Below full, on mobile data (the only path the emulator shapes).
        assert_eq!(words(steps(&AVD(), &NetChange::Speed("LTE".into())).unwrap()), ["emu network speed lte", "svc wifi disable"]);
        assert_eq!(words(steps(&AVD(), &NetChange::Speed("512".into())).unwrap()), ["emu network speed 512", "svc wifi disable"]);
        assert_eq!(words(steps(&AVD(), &NetChange::Speed("full".into())).unwrap()), ["emu network speed full", "svc wifi enable"]);
        assert!(matches!(steps(&PHONE(), &NetChange::Speed("lte".into())), Err(Reject::Unsupported(_))), "emulator only");
    }

    #[test]
    fn a_speed_is_a_preset_or_a_number() {
        for bad in ["12;x", "", "0", "-5", "1.5", "lte fast", "$(id)", "99999999999", "+5"] {
            assert!(matches!(speed_word(bad), Err(Reject::BadInput(_))), "{bad}");
        }
        assert_eq!(speed_word(" Full ").unwrap(), "full");
    }

    #[test]
    fn offline_turns_both_radios_off_and_online_back_on() {
        for target in [AVD(), PHONE()] {
            assert_eq!(words(steps(&target, &NetChange::Offline).unwrap()), ["svc wifi disable", "svc data disable"]);
            assert_eq!(words(steps(&target, &NetChange::Online).unwrap()), ["svc wifi enable", "svc data enable"]);
        }
    }

    #[test]
    fn a_phone_on_wifi_adb_is_never_taken_offline() {
        let offline = StateChange::Net(NetChange::Offline);
        for serial in ["192.168.1.9:5555", "adb-R58-x1Y2._adb-tls-connect._tcp"] {
            assert!(matches!(check_transport(serial, &offline), Err(Reject::Unsupported(_))), "{serial}");
            assert!(check_transport(serial, &StateChange::Net(NetChange::Online)).is_ok(), "online is the way back");
        }
        assert!(check_transport("R58M123", &offline).is_ok());
        assert!(check_transport("emulator-5554", &offline).is_ok());
    }
}
