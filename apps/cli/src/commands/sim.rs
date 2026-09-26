//! `oximux sim` — drive the iOS Simulator attached to a worktree in the
//! desktop app.
//!
//! Every verb is one `Request::Simulator`. The host resolves the worktree
//! (from `--worktree`, else the current directory) and, for anything that
//! looks at or touches the device, checks the user's consent — which it asks
//! for *without blocking*: the first such verb exits 7 at once, and the agent
//! runs `sim wait-consent` and retries.
//!
//! Screenshots come back as bytes and are written **here**, by the CLI, so the
//! desktop never writes files on an agent's behalf.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::simulator::{
    SimAxNodeWire, SimButtonWire, SimCmdWire, SimConsentWire, SimDeviceWire, SimErrorWire, SimOrientationWire,
    SimPointWire, SimReplyWire, SimRequestWire, SimStatusWire, SimTargetWire,
};
use serde_json::{Value, json};

use crate::cli::{SimButtonArg, SimCommand, SimOrientationArg, SimPlatformArg, exit};
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

type Outcome = Result<(Value, String), Failure>;

/// The folder a screenshot lands in by default, under `$TMPDIR`. Transcripts
/// mirrored to a phone scrub reads of it (see `agent-core`'s redaction).
pub const SCREENSHOT_DIR: &str = "oximux-sim";

pub async fn run(client: &Client, worktree: Option<PathBuf>, command: SimCommand) -> Outcome {
    let worktree = worktree_arg(worktree)?;
    match command {
        SimCommand::Status => {
            let status = status(client, &worktree).await?;
            Ok((status_json(&status), status_human(&status)))
        }
        SimCommand::WaitConsent { max_wait } => wait_consent(client, &worktree, max_wait).await,
        SimCommand::Devices { platform } => {
            let SimReplyWire::Devices(devices) = call(client, &worktree, SimCmdWire::Devices, QUICK).await? else {
                return Err(unexpected("Devices"));
            };
            let devices: Vec<SimDeviceWire> = devices.into_iter().filter(|d| on_platform(d, platform)).collect();
            let human = if devices.is_empty() {
                "no simulators (create one in Xcode › Devices and Simulators, or an emulator in Android Studio)".to_string()
            } else {
                devices.iter().map(device_line).collect::<Vec<_>>().join("\n")
            };
            Ok((json!({ "devices": devices.iter().map(device_json).collect::<Vec<_>>() }), human))
        }
        SimCommand::Attach { device, platform: Some(platform) } => {
            // Resolved here, among that platform's devices; the host then
            // attaches by id.
            let SimReplyWire::Devices(devices) = call(client, &worktree, SimCmdWire::Devices, QUICK).await? else {
                return Err(unexpected("Devices"));
            };
            let id = pick_on_platform(&devices, device.as_deref(), platform).ok_or_else(|| {
                Failure::new("not-found", exit::ERROR, "no such device on that platform (see `oximux sim devices`)")
            })?;
            attached(call(client, &worktree, SimCmdWire::Attach { device: Some(id) }, Duration::from_secs(45)).await?)
        }
        SimCommand::Screenshot { out, full } => {
            let reply = call(client, &worktree, SimCmdWire::Screenshot { full }, SLOW).await?;
            let SimReplyWire::Screenshot { png, width, height, scale } = reply else {
                return Err(unexpected("Screenshot"));
            };
            let path = write_screenshot(out, &png)?;
            let note = if scale == 1.0 {
                "1 px = 1 pt: positions read off it are tap coordinates".to_string()
            } else {
                format!("{scale} px per pt: divide by {scale} for tap coordinates")
            };
            let human = format!("{}  {width}×{height} px ({note})", path.display());
            Ok((json!({ "path": path, "width": width, "height": height, "scale": scale }), human))
        }
        SimCommand::Ax { flat, max } => {
            let SimReplyWire::Ax(nodes) = call(client, &worktree, SimCmdWire::Ax { max }, SLOW).await? else {
                return Err(unexpected("Ax"));
            };
            let human = if nodes.is_empty() {
                "(no accessible elements)".to_string()
            } else {
                nodes.iter().map(|n| ax_line(n, flat)).collect::<Vec<_>>().join("\n")
            };
            Ok((json!({ "nodes": nodes.iter().map(ax_json).collect::<Vec<_>>() }), human))
        }
        other => {
            let cwd = std::env::current_dir()
                .map_err(|e| Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}")))?;
            let (cmd, floor, said) = request_for(other, &cwd)?;
            let reply = call(client, &worktree, cmd, floor).await?;
            match reply {
                reply @ SimReplyWire::Attached(_) => attached(reply),
                SimReplyWire::Done => Ok((json!({ "ok": true }), said)),
                other => Err(unexpected_reply("Simulator", &Response::Simulator(Ok(other)))),
            }
        }
    }
}

/// How long a verb may take on the host: a parked device wakes in seconds, a
/// shut-down one boots for up to a minute, an install copies a whole app.
const QUICK: Duration = Duration::ZERO;
const SLOW: Duration = Duration::from_secs(90);
/// Waking (≤ 60 s) plus a relaunch's terminate and launch (≤ 60 s each).
const APP: Duration = Duration::from_secs(200);
const INSTALL: Duration = Duration::from_secs(270);

/// The request for a verb without a reply of its own, its time floor, and what
/// to say when it is done.
fn request_for(command: SimCommand, cwd: &Path) -> Result<(SimCmdWire, Duration, String), Failure> {
    Ok(match command {
        SimCommand::Attach { device, .. } => (SimCmdWire::Attach { device }, Duration::from_secs(45), String::new()),
        SimCommand::Detach => (SimCmdWire::Detach, QUICK, "detached".into()),
        SimCommand::Tap { x, y, label, id } => {
            let (target, said) = match (x, y, label, id) {
                (_, _, Some(label), _) => (SimTargetWire::Label(label.clone()), format!("tapped “{label}”")),
                (_, _, _, Some(id)) => (SimTargetWire::Id(id.clone()), format!("tapped #{id}")),
                (Some(x), Some(y), None, None) => (SimTargetWire::Point(SimPointWire { x, y }), format!("tapped ({x}, {y})")),
                _ => return Err(usage("`sim tap` wants X Y (points), or --label TEXT, or --id AXID")),
            };
            (SimCmdWire::Tap(target), SLOW, said)
        }
        SimCommand::Swipe { x1, y1, x2, y2, duration } => (
            SimCmdWire::Swipe { from: SimPointWire { x: x1, y: y1 }, to: SimPointWire { x: x2, y: y2 }, duration_ms: duration },
            SLOW,
            format!("swiped ({x1}, {y1}) → ({x2}, {y2})"),
        ),
        SimCommand::Type { text, paste } => {
            let said = format!("typed {} characters", text.chars().count());
            (SimCmdWire::Type { text, paste }, SLOW, said)
        }
        SimCommand::Button { button } => {
            let wire = match button {
                SimButtonArg::Home => SimButtonWire::Home,
                SimButtonArg::Lock => SimButtonWire::Lock,
                SimButtonArg::Siri => SimButtonWire::Siri,
                SimButtonArg::SideButton => SimButtonWire::SideButton,
                SimButtonArg::AppSwitcher => SimButtonWire::AppSwitcher,
                SimButtonArg::Back => SimButtonWire::Back,
                SimButtonArg::VolumeUp => SimButtonWire::VolumeUp,
                SimButtonArg::VolumeDown => SimButtonWire::VolumeDown,
            };
            (SimCmdWire::Button(wire), SLOW, format!("pressed {button:?}").to_lowercase())
        }
        SimCommand::Rotate { to } => {
            let wire = match to {
                SimOrientationArg::Portrait => SimOrientationWire::Portrait,
                SimOrientationArg::LandscapeLeft => SimOrientationWire::LandscapeLeft,
                SimOrientationArg::LandscapeRight => SimOrientationWire::LandscapeRight,
                SimOrientationArg::UpsideDown => SimOrientationWire::UpsideDown,
            };
            (SimCmdWire::Rotate(wire), SLOW, "rotated".into())
        }
        SimCommand::Launch { bundle_id, relaunch } => {
            let said = format!("launched {bundle_id}");
            (SimCmdWire::Launch { bundle_id, relaunch }, APP, said)
        }
        SimCommand::OpenUrl { url } => {
            let said = format!("opened {url}");
            (SimCmdWire::OpenUrl { url }, APP, said)
        }
        SimCommand::Install { path } => {
            // Relative to where the caller stands (like any command-line
            // path), not to `--worktree` or wherever the host runs.
            let path = absolute(&path, cwd);
            let said = format!("installed {}", path.display());
            (SimCmdWire::Install { path: path.to_string_lossy().into_owned() }, INSTALL, said)
        }
        SimCommand::Shutdown { force } => (SimCmdWire::Shutdown { force }, SLOW, "shut down".into()),
        SimCommand::Status
        | SimCommand::WaitConsent { .. }
        | SimCommand::Screenshot { .. }
        | SimCommand::Ax { .. }
        | SimCommand::Devices { .. } => {
            unreachable!("answered with a reply of their own")
        }
    })
}

fn attached(reply: SimReplyWire) -> Outcome {
    let SimReplyWire::Attached(device) = reply else { return Err(unexpected("Attached")) };
    let booting = if device.state == "Booted" { "" } else { " — booting" };
    let human = format!(
        "attached {}{booting}\nif agents are not allowed on it yet, the first verb that looks at it asks the user (exit 7)",
        device_line(&device)
    );
    Ok((json!({ "device": device_json(&device) }), human))
}

/// Android ids are `avd:<name>` / `adb:<serial>`; iOS ids are bare UDIDs.
fn is_android(device: &SimDeviceWire) -> bool {
    device.udid.starts_with("avd:") || device.udid.starts_with("adb:")
}

fn on_platform(device: &SimDeviceWire, platform: Option<SimPlatformArg>) -> bool {
    match platform {
        None => true,
        Some(SimPlatformArg::Android) => is_android(device),
        Some(SimPlatformArg::Ios) => !is_android(device),
    }
}

/// The device `wanted` names (id, else name, case-insensitive) on
/// `platform` — or, without a name, its booted one, else its first.
fn pick_on_platform(devices: &[SimDeviceWire], wanted: Option<&str>, platform: SimPlatformArg) -> Option<String> {
    let mut candidates = devices.iter().filter(|d| on_platform(d, Some(platform)));
    let found = match wanted.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => candidates.find(|d| d.udid == w || d.name.eq_ignore_ascii_case(w)),
        None => {
            let all: Vec<&SimDeviceWire> = candidates.collect();
            all.iter().find(|d| d.state == "Booted").or(all.first()).copied()
        }
    };
    found.map(|d| d.udid.clone())
}

/// `--worktree`, else the current directory, absolute.
fn worktree_arg(worktree: Option<PathBuf>) -> Result<PathBuf, Failure> {
    let cwd = std::env::current_dir()
        .map_err(|e| Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}")))?;
    Ok(match worktree {
        Some(dir) => absolute(&dir, &cwd),
        None => cwd,
    })
}

fn absolute(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() { path.to_path_buf() } else { base.join(path) }
}

async fn call(client: &Client, worktree: &Path, cmd: SimCmdWire, floor: Duration) -> Result<SimReplyWire, Failure> {
    let req = Request::Simulator(SimRequestWire { worktree: Some(worktree.to_string_lossy().into_owned()), cmd });
    match client.call_at_least(req, floor).await? {
        Response::Simulator(Ok(reply)) => Ok(reply),
        Response::Simulator(Err(e)) => Err(sim_failure(e)),
        // The one capability failure worth saying plainly: most hosts have
        // no simulator at all.
        Response::Error(RpcError::Unsupported) => Err(Failure::new(
            "unsupported",
            exit::ERROR,
            "this host has no simulators (the OxiMux desktop app on an Apple silicon Mac does)",
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("Simulator", &other)),
    }
}

async fn status(client: &Client, worktree: &Path) -> Result<SimStatusWire, Failure> {
    match call(client, worktree, SimCmdWire::Status, QUICK).await? {
        SimReplyWire::Status(status) => Ok(status),
        _ => Err(unexpected("Status")),
    }
}

/// Poll `status` once a second until the user answers.
async fn wait_consent(client: &Client, worktree: &Path, max_wait: u64) -> Outcome {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(max_wait.max(1));
    loop {
        let status = status(client, worktree).await?;
        if !status.agent_control {
            return Err(sim_failure(SimErrorWire::AgentControlDisabled));
        }
        if status.device.is_none() {
            return Err(sim_failure(SimErrorWire::NoDevice));
        }
        match status.consent {
            SimConsentWire::Allowed => return Ok((json!({ "consent": "allowed" }), "allowed — retry the verb".into())),
            SimConsentWire::Denied { retry_after_secs } => {
                return Err(sim_failure(SimErrorWire::ConsentDenied { retry_after_secs }));
            }
            SimConsentWire::NotAsked => {
                return Err(Failure::new("not-asked", exit::ERROR, "nothing is waiting for the user's answer").with_steps([
                    "run the verb you need (e.g. `oximux sim screenshot`): it asks, then exits 7".into(),
                ]));
            }
            SimConsentWire::Pending => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Failure::new(
                "timeout",
                exit::TIMEOUT,
                format!("the user has not answered after {max_wait}s"),
            )
            .with_steps(["tell the user the Simulator panel in OxiMux is asking, then wait again".into()]));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Write the PNG to `out`, or a fresh file under `$TMPDIR/oximux-sim/`.
fn write_screenshot(out: Option<PathBuf>, png: &[u8]) -> Result<PathBuf, Failure> {
    let path = match out {
        Some(path) => path,
        None => {
            let dir = std::env::temp_dir().join(SCREENSHOT_DIR);
            std::fs::create_dir_all(&dir)
                .map_err(|e| Failure::new("write", exit::ERROR, format!("cannot create {}: {e}", dir.display())))?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            dir.join(format!("screenshot-{stamp}.png"))
        }
    };
    std::fs::write(&path, png)
        .map_err(|e| Failure::new("write", exit::ERROR, format!("cannot write {}: {e}", path.display())))?;
    Ok(path)
}

fn sim_failure(e: SimErrorWire) -> Failure {
    let message = e.to_string();
    match e {
        SimErrorWire::ConsentPending => Failure::new("consent-pending", exit::PENDING, message).with_steps([
            "the user is being asked in OxiMux's Simulator panel; tell them, then run `oximux sim wait-consent`".into(),
            "retry this verb once it exits 0".into(),
        ]),
        SimErrorWire::ConsentDenied { retry_after_secs } => Failure::new("consent-denied", exit::DENIED, message)
            .with_steps([format!("do not retry for {} min; tell the user what you needed the simulator for", retry_after_secs.div_ceil(60))])
            .with_data(json!({ "retry_after_secs": retry_after_secs })),
        SimErrorWire::AgentControlDisabled => Failure::new("agent-control-off", exit::DENIED, message)
            .with_steps(["the user can turn it on in OxiMux Settings › Simulator".into()]),
        SimErrorWire::NoDevice => Failure::new("no-device", exit::ERROR, message).with_steps([
            "`oximux sim attach` attaches one (`oximux sim devices` lists them)".into(),
            "run inside a worktree that is open in OxiMux, or pass --worktree".into(),
        ]),
        SimErrorWire::NotStreaming => Failure::new("not-streaming", exit::ERROR, message)
            .with_steps(["retry in a few seconds; `oximux sim status` says when it streams".into()]),
        SimErrorWire::PathOutsideWorktree => Failure::new("path-outside-worktree", exit::DENIED, message)
            .with_steps(["build into this worktree or Xcode's DerivedData, then install from there".into()]),
        SimErrorWire::NotFound(_) => Failure::new("not-found", exit::ERROR, message),
        SimErrorWire::BadInput(_) => Failure::new("bad-input", exit::USAGE, message),
        SimErrorWire::Unavailable(_) => Failure::new("unavailable", exit::ERROR, message)
            .with_steps(["`oximux sim status` shows what is missing".into()]),
        SimErrorWire::Failed(_) => Failure::new("failed", exit::ERROR, message),
        SimErrorWire::Refused(_) => Failure::new("refused", exit::DENIED, message),
    }
}

fn usage(message: &str) -> Failure {
    Failure::new("usage", exit::USAGE, message.to_string())
}

fn unexpected(verb: &str) -> Failure {
    Failure::new("protocol", exit::ERROR, format!("the host answered `sim` with the wrong reply for {verb}"))
}

fn device_json(d: &SimDeviceWire) -> Value {
    json!({ "udid": d.udid, "name": d.name, "runtime": d.runtime, "state": d.state })
}

fn device_line(d: &SimDeviceWire) -> String {
    format!("{}  {} ({})  {}", d.udid, d.name, d.runtime, d.state)
}

fn consent_str(c: SimConsentWire) -> &'static str {
    match c {
        SimConsentWire::NotAsked => "not-asked",
        SimConsentWire::Pending => "pending",
        SimConsentWire::Allowed => "allowed",
        SimConsentWire::Denied { .. } => "denied",
    }
}

fn status_json(s: &SimStatusWire) -> Value {
    let retry_after = match s.consent {
        SimConsentWire::Denied { retry_after_secs } => Some(retry_after_secs),
        _ => None,
    };
    json!({
        "available": s.available,
        "reason": s.reason,
        "xcode": s.xcode,
        "worktree": s.worktree,
        "device": s.device.as_ref().map(device_json),
        "streaming": s.streaming,
        "consent": consent_str(s.consent),
        "retry_after_secs": retry_after,
        "agent_control": s.agent_control,
    })
}

fn status_human(s: &SimStatusWire) -> String {
    let mut lines = vec![format!("worktree   {}", s.worktree)];
    if !s.available {
        lines.push(format!("simulator  unavailable: {}", s.reason.as_deref().unwrap_or("unknown")));
    }
    lines.push(match &s.device {
        Some(d) => format!(
            "device     {} ({}, {})  {}{}",
            d.name,
            d.runtime,
            d.udid,
            d.state,
            if s.streaming { ", streaming" } else { "" }
        ),
        None => "device     none attached — `oximux sim attach`".into(),
    });
    let agents = if !s.agent_control {
        "turned off in OxiMux Settings".to_string()
    } else {
        match s.consent {
            SimConsentWire::Allowed => "allowed".into(),
            SimConsentWire::Pending => "waiting for the user to allow it in OxiMux".into(),
            SimConsentWire::NotAsked => "not asked yet (the first control verb asks)".into(),
            SimConsentWire::Denied { retry_after_secs } => {
                format!("refused (ask again in {} min)", retry_after_secs.div_ceil(60))
            }
        }
    };
    lines.push(format!("agents     {agents}"));
    if let Some(xcode) = &s.xcode {
        lines.push(format!("xcode      {xcode}"));
    }
    lines.join("\n")
}

fn ax_json(n: &SimAxNodeWire) -> Value {
    json!({
        "depth": n.depth,
        "role": n.role,
        "label": n.label,
        "identifier": n.identifier,
        "value": n.value,
        "enabled": n.enabled,
        "frame": { "x": n.frame[0], "y": n.frame[1], "width": n.frame[2], "height": n.frame[3] },
    })
}

fn ax_line(n: &SimAxNodeWire, flat: bool) -> String {
    let indent = if flat { String::new() } else { "  ".repeat(n.depth as usize) };
    let mut line = format!("{indent}{}", n.role);
    if let Some(label) = n.label.as_deref().filter(|l| !l.is_empty()) {
        line.push_str(&format!(" “{label}”"));
    }
    if let Some(id) = n.identifier.as_deref().filter(|i| !i.is_empty()) {
        line.push_str(&format!(" #{id}"));
    }
    if let Some(value) = n.value.as_deref().filter(|v| !v.is_empty()) {
        line.push_str(&format!(" = {value}"));
    }
    if !n.enabled {
        line.push_str(" (disabled)");
    }
    let [x, y, w, h] = n.frame;
    line.push_str(&format!("  [{x:.0},{y:.0} {w:.0}×{h:.0}]"));
    line
}

#[cfg(test)]
mod tests {
    fn device(udid: &str, name: &str, state: &str) -> SimDeviceWire {
        SimDeviceWire { udid: udid.into(), name: name.into(), runtime: String::new(), state: state.into() }
    }

    /// A name on both platforms resolves on the one asked for; without a name
    /// the platform's booted device wins.
    #[test]
    fn a_platform_narrows_the_lookup() {
        let devices = [
            device("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3", "Pixel", "Shutdown"),
            device("avd:Pixel", "Pixel", "Shutdown"),
            device("avd:Medium_Phone", "Medium Phone", "Booted"),
        ];
        assert_eq!(pick_on_platform(&devices, Some("pixel"), SimPlatformArg::Android).as_deref(), Some("avd:Pixel"));
        assert_eq!(
            pick_on_platform(&devices, Some("Pixel"), SimPlatformArg::Ios).as_deref(),
            Some("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3")
        );
        assert_eq!(pick_on_platform(&devices, None, SimPlatformArg::Android).as_deref(), Some("avd:Medium_Phone"));
        assert_eq!(pick_on_platform(&devices, Some("Medium Phone"), SimPlatformArg::Ios), None);
        assert_eq!(devices.iter().filter(|d| on_platform(d, Some(SimPlatformArg::Android))).count(), 2);
    }

    use super::*;

    #[test]
    fn consent_pending_is_its_own_exit_code() {
        let f = sim_failure(SimErrorWire::ConsentPending);
        assert_eq!((f.code, f.exit), ("consent-pending", exit::PENDING));
        assert!(f.next_steps.iter().any(|s| s.contains("wait-consent")));
        assert_eq!(sim_failure(SimErrorWire::ConsentDenied { retry_after_secs: 61 }).exit, exit::DENIED);
        assert_eq!(sim_failure(SimErrorWire::AgentControlDisabled).exit, exit::DENIED);
        assert!(sim_failure(SimErrorWire::NoDevice).next_steps.iter().any(|s| s.contains("sim attach")));
    }

    #[test]
    fn a_tap_needs_a_point_or_an_element() {
        let w = Path::new("/w");
        let tap = |x, y, label: Option<&str>, id: Option<&str>| {
            request_for(SimCommand::Tap { x, y, label: label.map(Into::into), id: id.map(Into::into) }, w)
        };
        assert!(matches!(tap(Some(1.0), Some(2.0), None, None).unwrap().0, SimCmdWire::Tap(SimTargetWire::Point(_))));
        assert!(matches!(tap(None, None, Some("OK"), None).unwrap().0, SimCmdWire::Tap(SimTargetWire::Label(_))));
        assert!(matches!(tap(None, None, None, Some("ok")).unwrap().0, SimCmdWire::Tap(SimTargetWire::Id(_))));
        assert_eq!(tap(Some(1.0), None, None, None).unwrap_err().exit, exit::USAGE);
        assert_eq!(tap(None, None, None, None).unwrap_err().exit, exit::USAGE);
    }

    #[test]
    fn an_install_path_is_made_absolute_where_the_caller_stands() {
        let (cmd, floor, _) = request_for(SimCommand::Install { path: "Build/App.app".into() }, Path::new("/w")).unwrap();
        assert_eq!(cmd, SimCmdWire::Install { path: "/w/Build/App.app".into() });
        assert_eq!(floor, INSTALL);
    }

    #[test]
    fn ax_lines_read_like_the_tree() {
        let node = SimAxNodeWire {
            depth: 2,
            role: "Button".into(),
            label: Some("Settings".into()),
            identifier: Some("settings".into()),
            value: None,
            enabled: false,
            frame: [10.0, 20.5, 100.0, 44.0],
        };
        assert_eq!(ax_line(&node, false), "    Button “Settings” #settings (disabled)  [10,20 100×44]");
        assert!(ax_line(&node, true).starts_with("Button"));
    }
}
