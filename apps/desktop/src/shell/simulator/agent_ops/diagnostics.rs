//! `sim logs` and `sim crashes`, once consent for the device is settled:
//! bounded reads (see `oximux_simulator::diag`), every line passed through
//! `scrub_text` before it reaches the agent.
//!
//! A real Android phone's log is read one app at a time (`--app`), so the
//! whole phone's never leaves it; a real iPhone's is refused — reading it
//! needs a system log tool OxiMux does not ship.

use gpui::{AsyncApp, Entity};
use oximux_agent_core::scrub_text::scrub_text;
use oximux_remote_proto::simulator::{SimCrashListWire, SimCrashWire, SimCrashesWire, SimErrorWire, SimLogLevelWire, SimLogsWire, SimReplyWire};
use oximux_simulator::diag::crashes::{self, CrashList};
use oximux_simulator::diag::logs::{self, Level, LogQuery};
use oximux_simulator::diag::{self, Crash, DiagError};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::{DeviceId, SimError, Source};

use super::verbs::{Out, android_device, on_own_thread, simctl_ready};
use crate::shell::simulator::hub::SimulatorHub;

const IPHONE: &str = "logs and crashes need a system log tool OxiMux doesn't ship; use Xcode's console (Window › Devices and Simulators)";
const WHOLE_PHONE: &str = "a real phone's log is read one app at a time: pass --app with its package name";

pub(super) async fn logs(hub: &Entity<SimulatorHub>, udid: &DeviceId, wire: SimLogsWire, cx: &mut AsyncApp) -> Out {
    let query = LogQuery {
        since_secs: wire.since_secs,
        app: wire.app,
        grep: wire.grep,
        level: match wire.level {
            SimLogLevelWire::Default => Level::Default,
            SimLogLevelWire::Error => Level::Error,
            SimLogLevelWire::Info => Level::Info,
            SimLogLevelWire::Debug => Level::Debug,
        },
        lines: wire.lines,
    };
    check(udid, query.app.as_deref())?;
    logs::check(&query).map_err(SimErrorWire::BadInput)?;
    let read = match udid.source() {
        Source::Adb => {
            let device = android_device(hub, udid, cx).await?;
            on_own_thread(move || device.logs(&query)).await?
        }
        _ => {
            let sim = simctl_ready(hub, udid, cx).await?;
            on_own_thread(move || logs::sim_logs(&SystemRunner, &sim, &query)).await?
        }
    };
    let read = read.map_err(failed)?;
    Ok(SimReplyWire::Logs { lines: read.lines.iter().map(|l| scrub_text(l)).collect(), truncated: read.truncated })
}

pub(super) async fn crashes(hub: &Entity<SimulatorHub>, udid: &DeviceId, wire: SimCrashesWire, cx: &mut AsyncApp) -> Out {
    let SimCrashesWire { since_secs, app } = wire;
    check(udid, app.as_deref())?;
    if since_secs == 0 || since_secs > diag::MAX_SINCE_SECS {
        return Err(SimErrorWire::BadInput(format!("--since is between 1s and {}m", diag::MAX_SINCE_SECS / 60)));
    }
    if let Some(app) = &app {
        logs::check(&LogQuery { since_secs, app: Some(app.clone()), grep: None, level: Level::Default, lines: 1 }).map_err(SimErrorWire::BadInput)?;
    }
    let list = match udid.source() {
        Source::Adb => {
            let device = android_device(hub, udid, cx).await?;
            on_own_thread(move || device.crashes(since_secs, app.as_deref())).await?
        }
        _ => {
            let sim = simctl_ready(hub, udid, cx).await?;
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
            on_own_thread(move || crashes::sim_crashes(&SystemRunner, &home, &sim, since_secs, app.as_deref())).await?
        }
    };
    Ok(SimReplyWire::Crashes(list_wire(list.map_err(failed)?)))
}

/// What the device class allows before anything runs.
fn check(udid: &DeviceId, app: Option<&str>) -> Result<(), SimErrorWire> {
    match udid.source() {
        Source::Devicectl => Err(SimErrorWire::Refused(IPHONE.into())),
        Source::Adb if udid.is_physical() && app.is_none() => Err(SimErrorWire::BadInput(WHOLE_PHONE.into())),
        _ => Ok(()),
    }
}

fn failed(e: DiagError) -> SimErrorWire {
    match e {
        DiagError::BadInput(why) => SimErrorWire::BadInput(why),
        DiagError::NotFound(why) => SimErrorWire::NotFound(why),
        DiagError::Sim(SimError::Timeout { secs, .. }) => {
            SimErrorWire::Failed(format!("reading the log took over {secs} s; narrow --since or add --app"))
        }
        DiagError::Sim(e) => SimErrorWire::Failed(format!("could not read the device's log: {e}")),
    }
}

/// A crash as the agent sees it: every word scrubbed.
pub(crate) fn crash_wire(crash: Crash) -> SimCrashWire {
    SimCrashWire {
        id: crash.id,
        time: crash.time,
        app: crash.app,
        exception: scrub_text(&crash.exception),
        reason: scrub_text(&crash.reason),
        frames: crash.frames.iter().map(|f| scrub_text(f)).collect(),
    }
}

fn list_wire(list: CrashList) -> SimCrashListWire {
    SimCrashListWire {
        crashes: list.crashes.into_iter().map(crash_wire).collect(),
        warnings: list.warnings.iter().map(|w| scrub_text(w)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_device_class_is_checked_before_anything_runs() {
        let iphone = DeviceId("iosdev:00008130-0001".into());
        assert!(matches!(check(&iphone, Some("com.a.b")), Err(SimErrorWire::Refused(_))));
        let phone = DeviceId("adb:R58M123".into());
        assert!(matches!(check(&phone, None), Err(SimErrorWire::BadInput(why)) if why.contains("--app")));
        assert!(check(&phone, Some("com.a.b")).is_ok());
        assert!(check(&DeviceId("avd:Pixel".into()), None).is_ok(), "an emulator's whole log is fine");
        assert!(check(&DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into()), None).is_ok());
    }

    #[test]
    fn a_crash_reaches_the_agent_scrubbed() {
        let crash = Crash {
            id: "1".into(),
            time: "t".into(),
            app: "com.a.b".into(),
            exception: "java.lang.IllegalStateException".into(),
            reason: "Checkout failed: token=abc123secret".into(),
            frames: vec!["com.a.b.Pay.go(Pay.kt:1)".into()],
            pid: Some(1),
        };
        let wire = crash_wire(crash);
        assert_eq!(wire.reason, "Checkout failed: token=[redacted]");
        assert_eq!(wire.frames, ["com.a.b.Pay.go(Pay.kt:1)"]);
    }

    #[test]
    fn a_slow_read_says_how_to_narrow_it() {
        let slow = failed(DiagError::Sim(SimError::Timeout { what: "log show".into(), secs: 30 }));
        assert!(matches!(slow, SimErrorWire::Failed(why) if why.contains("--since")));
        assert!(matches!(failed(DiagError::NotFound("x".into())), SimErrorWire::NotFound(_)));
    }
}
