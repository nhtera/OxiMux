//! `sim rn network|profile` (v32): a look into the React Native app on the
//! worktree's device, through its Metro dev server's debugger.
//!
//! The app is found here (Metro's targets on this Mac, matched to the
//! attached device's name), so an agent never names a debugger to connect
//! to. One short session per verb, on the app's tokio runtime
//! ([`super::rn_cdp`]); what it collected is redacted as it comes in
//! (`oximux_simulator::rn::redact`) and scrubbed before it is sent back.

use std::time::Duration;

use gpui::{AsyncApp, Entity};
use oximux_agent_core::scrub_text::scrub_text;
use oximux_remote_proto::simulator::{
    SimErrorWire, SimReplyWire, SimRnCmdWire, SimRnFunctionWire, SimRnNetworkWire, SimRnProfileWire, SimRnRequestWire, SimRnWire,
};
use oximux_simulator::rn::network::{Collector, Request};
use oximux_simulator::rn::profile;
use oximux_simulator::android::adb::Adb;
use oximux_simulator::rn::targets::{self, NoPick, RnTarget};
use oximux_simulator::runner::{Runner, SystemRunner};
use oximux_simulator::state::is_package;
use oximux_simulator::{DeviceId, Platform, Source};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::rn_cdp::{CALL_TIMEOUT, Cdp, CdpError};
use super::verbs::{Out, android_device, on_own_thread};
use crate::shell::simulator::hub::SimulatorHub;

/// The longest look.
const MAX_SECONDS: u32 = 60;
/// The most functions listed.
const MAX_TOP: u32 = 100;
/// `Profiler.stop` sends the whole profile: a minute of it takes a while.
/// With the setup calls, within the CLI's allowance (`OVERHEAD` there).
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
/// A teardown call (the app may be gone).
const QUIET: Duration = Duration::from_secs(2);
/// Asking a device whether the app runs.
const RUNNING_TIMEOUT: Duration = Duration::from_secs(5);

/// What a debugger without the Network domain (React Native before 0.83)
/// gets.
fn needs_083(found: Option<&str>) -> SimErrorWire {
    let found = found.map_or_else(String::new, |v| format!(" (found {v})"));
    SimErrorWire::Failed(format!(
        "network inspection needs React Native 0.83 or later{found}. Upgrade, or use React Native DevTools"
    ))
}

pub(super) async fn run(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, cmd: SimRnCmdWire, cx: &mut AsyncApp) -> Out {
    let app = match &cmd {
        SimRnCmdWire::Network { app, .. } | SimRnCmdWire::Profile { app, .. } => app.clone(),
    };
    let names = device_names(hub, udid, name, cx).await;
    let found = on_own_thread(targets::discover).await?;
    let target = targets::pick(&found, &names, app.as_deref()).map_err(|e| match e {
        NoPick::Several(_) => SimErrorWire::BadInput(e.to_string()),
        NoPick::NoMetro | NoPick::NotOnDevice { .. } | NoPick::NoSuchApp { .. } | NoPick::SameName(_) => SimErrorWire::NotFound(e.to_string()),
    })?;
    // A name can belong to several devices (every arm64 emulator is the
    // same model): the app must be running on this one.
    if runs_here(hub, udid, &target.app_id, cx).await == Some(false) {
        return Err(SimErrorWire::NotFound(format!(
            "{} is connected to Metro from a device called “{}”, but it is not running on this one",
            target.app_id, target.device_name
        )));
    }
    // The WebSocket needs a tokio reactor: the runtime the app entered on
    // this (the main) thread.
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| SimErrorWire::Unavailable("no async runtime for the debugger".into()))?;
    let look = runtime.spawn(async move {
        match cmd {
            SimRnCmdWire::Network { seconds, grep, bodies, .. } => network(target, seconds, grep, bodies).await,
            SimRnCmdWire::Profile { seconds, top, .. } => profile(target, seconds, top).await,
        }
    });
    let wire = look.await.map_err(|_| SimErrorWire::Failed("the look into the app stopped unexpectedly".into()))??;
    Ok(SimReplyWire::Rn(scrubbed(wire)))
}

/// What the app may call this device: its name, and on Android its model.
async fn device_names(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, cx: &mut AsyncApp) -> Vec<String> {
    let mut names = vec![name.to_owned()];
    if udid.platform() == Platform::Android
        && let Ok(device) = android_device(hub, udid, cx).await
        && let Ok(Some(model)) = on_own_thread(move || device.model()).await
    {
        names.push(model);
    }
    names
}

/// Whether `app` runs on `udid` (`None`: cannot tell, as on a real iPhone).
async fn runs_here(hub: &Entity<SimulatorHub>, udid: &DeviceId, app: &str, cx: &mut AsyncApp) -> Option<bool> {
    match udid.source() {
        Source::Simctl => {
            let sim = udid.sim_udid()?;
            let app = app.to_owned();
            let list = on_own_thread(move || {
                let args = ["simctl", "spawn", sim.as_str(), "launchctl", "list"];
                SystemRunner.run("xcrun", &args, None, RUNNING_TIMEOUT).ok().filter(|o| o.success()).map(|o| o.stdout_str())
            });
            Some(list.await.ok()?.is_some_and(|l| targets::runs_in_launchctl(&l, &app)))
        }
        Source::Adb => {
            if !is_package(app) {
                return Some(false);
            }
            let device = android_device(hub, udid, cx).await.ok()?;
            let app = app.to_owned();
            let pid = on_own_thread(move || {
                oximux_simulator::diag::logs::app_pid(&Adb::new(&SystemRunner, device.adb()), device.serial(), &app)
            });
            Some(pid.await.ok()?.ok()?.is_some())
        }
        Source::Devicectl => None,
    }
}

/// Said when this look may have closed an open React Native DevTools
/// session, and when the debugger connection ended early.
fn warnings(target: &RnTarget, lost: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if !target.multiple_debuggers {
        out.push("this app's Metro allows one debugger at a time: a React Native DevTools session open on it was disconnected; reopen it".into());
    }
    if let Some(why) = lost {
        out.push(format!("the watch ended early: {why}"));
    }
    out
}

async fn network(target: RnTarget, seconds: u32, grep: Option<String>, bodies: bool) -> Result<SimRnWire, SimErrorWire> {
    let mut cdp = Cdp::connect(&target).await.map_err(failed)?;
    let version = rn_version(&mut cdp).await;
    match cdp.call("Network.enable", json!({}), CALL_TIMEOUT).await {
        Ok(_) => {}
        Err(CdpError::Remote(_)) => {
            cdp.close().await;
            return Err(needs_083(version.as_deref()));
        }
        Err(e) => return Err(failed(e)),
    }
    let mut collector = Collector::new(grep, bodies);
    let until = Instant::now() + Duration::from_secs(u64::from(seconds.clamp(1, MAX_SECONDS)));
    while let Some((method, params)) = cdp.next_event(until).await {
        let Some(id) = collector.event(&method, &params) else { continue };
        // Bodies only within the watch: none fetched after it ends.
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            continue;
        }
        // A body the app no longer holds is simply not shown.
        if let Ok(body) = cdp.call("Network.getResponseBody", json!({ "requestId": id }), left.min(CALL_TIMEOUT)).await {
            let text = body.get("body").and_then(Value::as_str).unwrap_or_default();
            collector.body(&id, text, body.get("base64Encoded").and_then(Value::as_bool) == Some(true));
        }
    }
    let lost = cdp.lost().map(str::to_owned);
    if lost.is_none() {
        let _ = cdp.call("Network.disable", json!({}), QUIET).await;
    }
    cdp.close().await;
    let (requests, truncated) = collector.finish();
    Ok(SimRnWire::Network(SimRnNetworkWire {
        warnings: warnings(&target, lost.as_deref()),
        app: target.app_id,
        requests: requests.into_iter().map(request_wire).collect(),
        truncated,
    }))
}

async fn profile(target: RnTarget, seconds: u32, top: u32) -> Result<SimRnWire, SimErrorWire> {
    let mut cdp = Cdp::connect(&target).await.map_err(failed)?;
    // Hermes has no `Profiler.enable`; `start` is what counts.
    let _ = cdp.call("Profiler.enable", json!({}), QUIET).await;
    if let Err(e) = cdp.call("Profiler.start", json!({}), CALL_TIMEOUT).await {
        cdp.close().await;
        return Err(SimErrorWire::Failed(format!("the app's debugger cannot profile: {e}")));
    }
    let until = Instant::now() + Duration::from_secs(u64::from(seconds.clamp(1, MAX_SECONDS)));
    while cdp.next_event(until).await.is_some() {}
    if let Some(why) = cdp.lost().map(str::to_owned) {
        return Err(SimErrorWire::Failed(format!("the profile was lost: {why}")));
    }
    let stopped = cdp.call("Profiler.stop", json!({}), STOP_TIMEOUT).await;
    cdp.close().await;
    let stopped = stopped.map_err(|e| SimErrorWire::Failed(format!("the profile did not come back: {e}")))?;
    let profile = stopped.get("profile").ok_or_else(|| SimErrorWire::Failed("the debugger sent no profile".into()))?;
    let summary = profile::summarize(profile, top.clamp(1, MAX_TOP) as usize).map_err(SimErrorWire::Failed)?;
    Ok(SimRnWire::Profile(SimRnProfileWire {
        warnings: warnings(&target, None),
        app: target.app_id,
        duration_ms: summary.duration_ms,
        sampled_ms: summary.sampled_ms,
        functions: summary
            .functions
            .into_iter()
            .map(|f| SimRnFunctionWire { name: f.name, url: oximux_simulator::rn::redact::url(&f.url), line: f.line, self_ms: f.self_ms })
            .collect(),
    }))
}

/// The app's React Native version, as its debugger says (for the
/// "needs 0.83" message): `ReactNativeApplication.enable` answers with it.
async fn rn_version(cdp: &mut Cdp) -> Option<String> {
    cdp.call("ReactNativeApplication.enable", json!({}), CALL_TIMEOUT).await.ok()?;
    let until = Instant::now() + Duration::from_millis(500);
    while let Some((method, params)) = cdp.next_event(until).await {
        if method == "ReactNativeApplication.metadataUpdated" {
            return params.get("reactNativeVersion").and_then(Value::as_str).map(str::to_owned);
        }
    }
    None
}

fn failed(e: CdpError) -> SimErrorWire {
    match e {
        CdpError::Remote(why) => SimErrorWire::Failed(why),
        // The app reaches Metro over the network: offline (`sim net
        // offline`), paused in a debugger or reloading, it cannot answer.
        CdpError::Transport(why) => SimErrorWire::Failed(format!(
            "{why}; the app did not answer through Metro (offline, paused at a breakpoint, or reloading?)"
        )),
    }
}

fn request_wire(r: Request) -> SimRnRequestWire {
    SimRnRequestWire {
        method: r.method,
        url: r.url,
        status: r.status,
        ms: r.ms,
        size: r.size,
        mime: r.mime,
        failed: r.failed,
        request_headers: r.request_headers,
        response_headers: r.response_headers,
        request_body: r.request_body,
        response_body: r.response_body,
    }
}

/// Every word of `wire` through `scrub_text`: the last line of defence after
/// the per-field redaction.
fn scrubbed(wire: SimRnWire) -> SimRnWire {
    let s = |t: String| scrub_text(&t);
    let pairs = |p: Vec<(String, String)>| p.into_iter().map(|(k, v)| (k, s(v))).collect();
    match wire {
        SimRnWire::Network(n) => SimRnWire::Network(SimRnNetworkWire {
            requests: n
                .requests
                .into_iter()
                .map(|r| SimRnRequestWire {
                    url: s(r.url),
                    failed: r.failed.map(s),
                    request_headers: pairs(r.request_headers),
                    response_headers: pairs(r.response_headers),
                    request_body: r.request_body.map(s),
                    response_body: r.response_body.map(s),
                    ..r
                })
                .collect(),
            ..n
        }),
        SimRnWire::Profile(p) => {
            SimRnWire::Profile(SimRnProfileWire { functions: p.functions.into_iter().map(|f| SimRnFunctionWire { name: s(f.name), ..f }).collect(), ..p })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_react_native_is_told_what_it_needs() {
        let SimErrorWire::Failed(why) = needs_083(Some("0.79.2")) else { panic!() };
        assert!(why.contains("0.83 or later (found 0.79.2)") && why.contains("React Native DevTools"), "{why}");
    }

    #[test]
    fn a_single_debugger_metro_is_warned_about() {
        let mut target = RnTarget {
            app_id: "a.b".into(),
            device_name: "iPhone 17".into(),
            device_id: "d".into(),
            host: "127.0.0.1".into(),
            port: 8081,
            path: "/".into(),
            multiple_debuggers: true,
        };
        assert!(warnings(&target, None).is_empty());
        target.multiple_debuggers = false;
        assert!(warnings(&target, None)[0].contains("DevTools"));
        assert!(warnings(&target, Some("Metro closed the debugger connection"))[1].starts_with("the watch ended early: Metro"));
    }

    #[test]
    fn what_is_sent_back_is_scrubbed() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let r = SimRnRequestWire {
            method: "GET".into(),
            url: "https://a.b/x".into(),
            status: Some(200),
            ms: None,
            size: None,
            mime: None,
            failed: None,
            request_headers: vec![("X-Trace".into(), jwt.into())],
            response_headers: Vec::new(),
            request_body: None,
            response_body: Some(format!("token is {jwt}")),
        };
        let wire = SimRnWire::Network(SimRnNetworkWire { app: "a.b".into(), requests: vec![r], truncated: false, warnings: Vec::new() });
        let out = format!("{:?}", scrubbed(wire));
        assert!(!out.contains(jwt), "{out}");
    }
}
