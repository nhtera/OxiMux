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
    SimButtonWire, SimCmdWire, SimConsentWire, SimDeviceWire, SimErrorWire, SimOrientationWire, SimPointWire,
    SimReplyWire, SimRequestWire, SimStatusWire, SimTargetWire,
};
use serde_json::{Value, json};

use super::sim_render::{ax_json, ax_line};
use super::sim_verify;
use crate::cli::{SimButtonArg, SimCommand, SimOrientationArg, SimPlatformArg, TapArg, exit};
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

pub(super) type Outcome = Result<(Value, String), Failure>;

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
            let SimReplyWire::Devices(devices) = call(client, &worktree, SimCmdWire::Devices, LISTING).await? else {
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
            let SimReplyWire::Devices(devices) = call(client, &worktree, SimCmdWire::Devices, LISTING).await? else {
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
        SimCommand::Ax { max, interactive: true, .. } => sim_verify::ax_refs(client, &worktree, max).await,
        SimCommand::Wait(args) => sim_verify::wait(client, &worktree, args).await,
        SimCommand::Assert(args) => sim_verify::assert(client, &worktree, args).await,
        SimCommand::Type { text, paste, settle } if text.len() > 1 => sim_verify::type_into(client, &worktree, text, paste, &settle).await,
        SimCommand::Ax { flat, max, interactive: false } => {
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
            let settle = other.settle_args().cloned().unwrap_or_default();
            // The verb already names its element: a report need not repeat it.
            let named = matches!(other, SimCommand::Tap { label: Some(_), .. } | SimCommand::Tap { id: Some(_), .. });
            let (cmd, floor, said) = request_for(other, &cwd)?;
            let (cmd, floor) = sim_verify::wrap(cmd, floor, &settle);
            let reply = call(client, &worktree, cmd, floor).await?;
            match reply {
                reply @ SimReplyWire::Attached(_) => attached(reply),
                reply @ (SimReplyWire::Done | SimReplyWire::Settled(_)) => sim_verify::acted(reply, &said, named),
                other => Err(unexpected_reply("Simulator", &Response::Simulator(Ok(other)))),
            }
        }
    }
}

/// How long a verb may take on the host: a parked device wakes in seconds, a
/// shut-down one boots for up to a minute, an install copies a whole app.
const QUICK: Duration = Duration::ZERO;
pub(super) const SLOW: Duration = Duration::from_secs(90);
/// Waking (≤ 60 s) plus a relaunch's terminate and launch (≤ 60 s each).
pub(super) const APP: Duration = Duration::from_secs(200);
/// Waking (≤ 60 s), on a real device the user's yes (≤ 90 s), then the copy
/// (≤ 180 s).
const INSTALL: Duration = Duration::from_secs(360);
/// A device listing: right after the app starts it first waits for the Xcode
/// check (≤ 15 s), then lists (≤ 30 s).
const LISTING: Duration = Duration::from_secs(45);

/// The request for a verb without a reply of its own, its time floor, and what
/// to say when it is done.
fn request_for(command: SimCommand, cwd: &Path) -> Result<(SimCmdWire, Duration, String), Failure> {
    if let Some(request) = super::sim_state::request(&command, cwd) {
        return request;
    }
    Ok(match command {
        SimCommand::Attach { device, .. } => (SimCmdWire::Attach { device }, LISTING + Duration::from_secs(15), String::new()),
        SimCommand::Detach => (SimCmdWire::Detach, QUICK, "detached".into()),
        SimCommand::Tap { x, y, label, id, .. } => {
            let (target, said) = match (x, y, label, id) {
                (_, _, Some(label), _) => (SimTargetWire::Label(label.clone()), format!("tapped “{label}”")),
                (_, _, _, Some(id)) => (SimTargetWire::Id(id.clone()), format!("tapped #{id}")),
                (Some(TapArg::Ref(r)), None, None, None) => (SimTargetWire::Ref(r), format!("tapped @e{r}")),
                (Some(TapArg::X(x)), Some(y), None, None) => (SimTargetWire::Point(SimPointWire { x, y }), format!("tapped ({x}, {y})")),
                _ => return Err(usage("`sim tap` wants X Y (points), an @eN ref, --label TEXT, or --id AXID")),
            };
            (SimCmdWire::Tap(target), SLOW, said)
        }
        SimCommand::Swipe { x1, y1, x2, y2, duration, .. } => (
            SimCmdWire::Swipe { from: SimPointWire { x: x1, y: y1 }, to: SimPointWire { x: x2, y: y2 }, duration_ms: duration },
            SLOW,
            format!("swiped ({x1}, {y1}) → ({x2}, {y2})"),
        ),
        SimCommand::Type { text, paste, .. } => {
            // `type @eN TEXT` (two words) is `sim_verify::type_into`'s.
            let [text] = <[String; 1]>::try_from(text).map_err(|_| usage("`sim type` takes TEXT, or an @eN ref then TEXT"))?;
            let said = format!("typed {} characters", text.chars().count());
            (SimCmdWire::Type { text, paste }, SLOW, said)
        }
        SimCommand::Button { button, .. } => {
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
        SimCommand::Rotate { to, .. } => {
            let wire = match to {
                SimOrientationArg::Portrait => SimOrientationWire::Portrait,
                SimOrientationArg::LandscapeLeft => SimOrientationWire::LandscapeLeft,
                SimOrientationArg::LandscapeRight => SimOrientationWire::LandscapeRight,
                SimOrientationArg::UpsideDown => SimOrientationWire::UpsideDown,
            };
            (SimCmdWire::Rotate(wire), SLOW, "rotated".into())
        }
        SimCommand::Launch { bundle_id, relaunch, .. } => {
            let said = format!("launched {bundle_id}");
            (SimCmdWire::Launch { bundle_id, relaunch }, APP, said)
        }
        SimCommand::OpenUrl { url, .. } => {
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
        | SimCommand::Wait(_)
        | SimCommand::Assert(_)
        | SimCommand::Devices { .. } => {
            unreachable!("answered with a reply of their own")
        }
        SimCommand::Location(_)
        | SimCommand::Appearance { .. }
        | SimCommand::Privacy(_)
        | SimCommand::Push(_)
        | SimCommand::StatusBar { .. }
        | SimCommand::Erase { .. } => unreachable!("built by `sim_state::request` above"),
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

/// A real device: an Android phone (`adb:`) or an iPhone (`iosdev:`).
fn is_physical(device: &SimDeviceWire) -> bool {
    device.udid.starts_with("adb:") || device.udid.starts_with("iosdev:")
}

/// The device `wanted` names (id, else name, case-insensitive) on
/// `platform` — or, without a name, its booted one, else its first. Never a
/// real device unless named: someone's phone is not picked for them (and the
/// desktop refuses an agent's attach of one the user has not allowed).
fn pick_on_platform(devices: &[SimDeviceWire], wanted: Option<&str>, platform: SimPlatformArg) -> Option<String> {
    let mut candidates = devices.iter().filter(|d| on_platform(d, Some(platform)));
    let found = match wanted.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => candidates.find(|d| d.udid == w || d.name.eq_ignore_ascii_case(w)),
        None => {
            let all: Vec<&SimDeviceWire> = candidates.filter(|d| !is_physical(d)).collect();
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

pub(super) async fn call(client: &Client, worktree: &Path, cmd: SimCmdWire, floor: Duration) -> Result<SimReplyWire, Failure> {
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
            .with_steps(["tell the user the Mobile Emulator panel in OxiMux is asking, then wait again".into()]));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Write the PNG to `out`, or a fresh file under `$TMPDIR/oximux-sim/`: a
/// screen can show anything, so that folder and file are the user's alone
/// (0700 / 0600 on Unix), and the file is created new, never reused.
fn write_screenshot(out: Option<PathBuf>, png: &[u8]) -> Result<PathBuf, Failure> {
    let failed = |path: &Path, e: std::io::Error| Failure::new("write", exit::ERROR, format!("cannot write {}: {e}", path.display()));
    if let Some(path) = out {
        std::fs::write(&path, png).map_err(|e| failed(&path, e))?;
        return Ok(path);
    }
    let dir = std::env::temp_dir().join(SCREENSHOT_DIR);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|e| Failure::new("write", exit::ERROR, format!("cannot create {}: {e}", dir.display())))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    for attempt in 0..100 {
        let suffix = if attempt == 0 { String::new() } else { format!("-{attempt}") };
        let path = dir.join(format!("screenshot-{stamp}-{}{suffix}.png", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, png).map_err(|e| failed(&path, e))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(failed(&path, e)),
        }
    }
    Err(Failure::new("write", exit::ERROR, format!("no free screenshot name in {}", dir.display())))
}

fn sim_failure(e: SimErrorWire) -> Failure {
    let message = e.to_string();
    match e {
        SimErrorWire::ConsentPending => Failure::new("consent-pending", exit::PENDING, message).with_steps([
            "the user is being asked in OxiMux's Mobile Emulator panel; tell them, then run `oximux sim wait-consent`".into(),
            "retry this verb once it exits 0".into(),
        ]),
        SimErrorWire::ConsentDenied { retry_after_secs } => Failure::new("consent-denied", exit::DENIED, message)
            .with_steps([format!("do not retry for {} min; tell the user what you needed the simulator for", retry_after_secs.div_ceil(60))])
            .with_data(json!({ "retry_after_secs": retry_after_secs })),
        SimErrorWire::AgentControlDisabled => Failure::new("agent-control-off", exit::DENIED, message)
            .with_steps(["the user can turn it on in OxiMux Settings › Mobile Emulator".into()]),
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
        SimErrorWire::StaleRef(_) => Failure::new("stale-ref", exit::ERROR, message)
            .with_steps(["run `oximux sim ax -i` and use the refs it prints".into()]),
        SimErrorWire::AssertFailed(_) => Failure::new("assert-failed", exit::ERROR, message),
        SimErrorWire::TimedOut(_) => Failure::new("timeout", exit::TIMEOUT, message)
            .with_steps(["`oximux sim ax -i` shows what is on screen now".into()]),
    }
}

pub(super) fn usage(message: &str) -> Failure {
    Failure::new("usage", exit::USAGE, message.to_string())
}

pub(super) fn unexpected(verb: &str) -> Failure {
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

    /// Without a name, a real device is never the pick (even a booted one);
    /// named, it is resolved and the desktop decides.
    #[test]
    fn a_real_device_is_never_picked_unnamed() {
        let devices = [
            device("adb:R58M123", "Galaxy S24", "Booted"),
            device("avd:Pixel", "Pixel", "Shutdown"),
            device("iosdev:00008110-001A2C3E0A88401E", "iPhone", "Booted"),
            device("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3", "iPhone 17", "Shutdown"),
        ];
        assert_eq!(pick_on_platform(&devices, None, SimPlatformArg::Android).as_deref(), Some("avd:Pixel"));
        assert_eq!(pick_on_platform(&devices, None, SimPlatformArg::Ios).as_deref(), Some("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3"));
        assert_eq!(pick_on_platform(&devices[..1], None, SimPlatformArg::Android), None);
        assert_eq!(pick_on_platform(&devices, Some("galaxy s24"), SimPlatformArg::Android).as_deref(), Some("adb:R58M123"));
        assert!(on_platform(&devices[2], Some(SimPlatformArg::Ios)), "an iPhone is iOS");
    }

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
            let settle = Default::default();
            request_for(SimCommand::Tap { x, y, label: label.map(Into::into), id: id.map(Into::into), settle }, w)
        };
        assert!(matches!(tap(Some(TapArg::X(1.0)), Some(2.0), None, None).unwrap().0, SimCmdWire::Tap(SimTargetWire::Point(_))));
        assert!(matches!(tap(None, None, Some("OK"), None).unwrap().0, SimCmdWire::Tap(SimTargetWire::Label(_))));
        assert!(matches!(tap(None, None, None, Some("ok")).unwrap().0, SimCmdWire::Tap(SimTargetWire::Id(_))));
        assert_eq!(tap(Some(TapArg::Ref(7)), None, None, None).unwrap().0, SimCmdWire::Tap(SimTargetWire::Ref(7)));
        assert_eq!(tap(Some(TapArg::Ref(7)), Some(2.0), None, None).unwrap_err().exit, exit::USAGE);
        assert_eq!(tap(Some(TapArg::X(1.0)), None, None, None).unwrap_err().exit, exit::USAGE);
        assert_eq!(tap(None, None, None, None).unwrap_err().exit, exit::USAGE);
    }

    /// A default screenshot is a new file each time, readable by its owner
    /// only.
    #[test]
    fn default_screenshots_are_private_and_never_reused() {
        let (a, b) = (write_screenshot(None, b"one").unwrap(), write_screenshot(None, b"two").unwrap());
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        }
        for path in [a, b] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn an_install_path_is_made_absolute_where_the_caller_stands() {
        let (cmd, floor, _) = request_for(SimCommand::Install { path: "Build/App.app".into() }, Path::new("/w")).unwrap();
        // Joined with the platform's separator (`/w\\Build/App.app` on Windows).
        let joined = Path::new("/w").join("Build/App.app").to_string_lossy().into_owned();
        assert_eq!(cmd, SimCmdWire::Install { path: joined });
        assert_eq!(floor, INSTALL);
    }
}
