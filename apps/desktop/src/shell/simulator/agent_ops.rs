//! Agent control: the desktop half of `oximux sim …`.
//!
//! [`run`] resolves the path a request names to a worktree this app knows,
//! then either reports (`status`, `devices`), changes the attachment
//! (`attach`, `detach`), or — for a control verb — checks the kill switch and
//! the user's consent for the attached device before carrying the verb out
//! (see [`verbs`]).
//!
//! Only state reads and writes hop onto the UI thread; everything that waits
//! on the device (a listing, a screenshot, the AX tree, an install) runs on
//! the background executor, so a slow verb never stalls the app or another
//! agent's request.
//!
//! The window a request belongs to is the one that knows its worktree —
//! preferring one where it is the active tab's — never simply the first
//! window: that is where a consent question is asked.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, AsyncApp, Context, Entity, Window};
use oximux_remote_proto::simulator::{
    SimCmdWire, SimConsentWire, SimDeviceWire, SimErrorWire, SimReplyWire, SimStatusWire,
};
use oximux_simulator::consent::{State, Verdict};
use oximux_simulator::registry::WorktreeKey;
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::{DeviceId, DeviceInfo, DeviceState};

use super::auto_open::Trigger;
use super::hub::{SimulatorHub, hub};
use crate::platform::window_registry;
use crate::workspace_root::WorkspaceRoot;

mod android;
mod verbs;

/// `simctl` listing timeout.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// One worktree a window knows (a rail row, or a project's own root).
#[derive(Clone, Debug)]
struct Known {
    path: String,
    /// The window's persistence id.
    window: String,
    /// It is that window's active tab's worktree.
    active: bool,
    /// The window's place in the stacking order (0 = frontmost): the
    /// deliberate pick among windows that tie otherwise.
    front: usize,
    project_id: String,
    workspace_id: String,
    /// "project / worktree", for the consent question.
    label: String,
}

/// The worktree a request resolved to.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    /// As the app stores it (the key the panel and hub use).
    pub worktree: PathBuf,
    window: String,
    project_id: String,
    workspace_id: String,
    label: String,
}

/// Carry out one `oximux sim` verb for the worktree containing `path`.
pub async fn run(path: String, cmd: SimCmdWire, cx: &mut AsyncApp) -> Result<SimReplyWire, SimErrorWire> {
    let hub = cx
        .update(|cx| hub(cx))
        .ok_or_else(|| SimErrorWire::Unavailable("the iOS Simulator panel needs an Apple silicon Mac".into()))?;
    if !cx.update(|cx| super::panel::settings(cx).enabled) {
        return Err(SimErrorWire::Unavailable("the iOS Simulator is turned off in OxiMux Settings".into()));
    }
    let known = cx.update(known_worktrees);
    // Canonicalizing touches the filesystem: off the UI thread.
    let target = cx.background_executor().spawn(async move { resolve(&path, known) }).await.ok_or(SimErrorWire::NoDevice)?;
    // An agent using the simulator counts as using the feature: the device
    // watcher may poll from now on.
    hub.update(cx, |hub, cx| hub.mark_used(cx));
    match cmd {
        SimCmdWire::Status => status(&hub, &target, cx),
        SimCmdWire::Devices => list_devices(&hub, cx).await.map(|d| SimReplyWire::Devices(d.iter().map(device_wire).collect())),
        SimCmdWire::Attach { device } => attach(&hub, &target, device, cx).await,
        SimCmdWire::Detach => {
            hub.update(cx, |hub, cx| hub.detach(&target.worktree, cx));
            Ok(SimReplyWire::Done)
        }
        control => {
            let (udid, name) = authorize(&hub, &target, cx)?;
            hub.update(cx, |hub, cx| hub.agent_verb_started(&udid, cx));
            let worktree = target.worktree.clone();
            with_root(cx, &target, move |root, _, cx| root.reveal_simulator_for(&worktree, Trigger::Verb, cx));
            let out = verbs::run(&hub, &udid, &name, control, &target, cx).await;
            hub.update(cx, |hub, cx| hub.agent_verb_finished(&udid, cx));
            out
        }
    }
}

/// Every worktree any window knows.
fn known_worktrees(cx: &mut App) -> Vec<Known> {
    let mut out = Vec::new();
    for (window, root) in window_registry::all_windows(cx) {
        let front = window_registry::front_rank(cx, &window);
        let root = root.read(cx);
        let active = root.active_worktree.as_ref().map(|p| p.to_string_lossy().into_owned());
        let projects = &root.app_state.recent_projects;
        let project_name =
            |id: &str| projects.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_default();
        let mut push = |path: &str, project_id: &str, workspace_id: String, label: String| {
            out.push(Known {
                path: path.to_owned(),
                window: window.clone(),
                active: active.as_deref() == Some(path),
                front,
                project_id: project_id.to_owned(),
                workspace_id,
                label,
            });
        };
        for project in projects {
            push(&project.root_path, &project.id, format!("primary:{}", project.id), project.name.clone());
        }
        for (project_id, rows) in &root.rail_workspaces_by_project {
            for row in rows {
                // A project's own checkout is the project, by name; a
                // worktree is "project / worktree".
                let root_row = projects.iter().any(|p| p.id == *project_id && p.root_path == row.worktree_path);
                let label = if root_row {
                    project_name(project_id)
                } else {
                    format!("{} / {}", project_name(project_id), row.name)
                };
                push(&row.worktree_path, project_id, row.id.clone(), label);
            }
        }
    }
    out
}

/// The deepest known worktree containing `path`, compared canonically (so a
/// symlinked or `/tmp` path still matches); a window where it is active wins a
/// tie, then the frontmost. `None` when the path is in no worktree the app
/// knows.
fn resolve(path: &str, known: Vec<Known>) -> Option<Target> {
    let path = std::fs::canonicalize(path).ok()?;
    known
        .into_iter()
        .filter_map(|k| {
            let root = std::fs::canonicalize(&k.path).ok()?;
            path.starts_with(&root).then(|| (root.components().count(), k))
        })
        .max_by_key(|(depth, k)| (*depth, k.active, std::cmp::Reverse(k.front)))
        .map(|(_, k)| Target {
            worktree: PathBuf::from(k.path),
            window: k.window,
            project_id: k.project_id,
            workspace_id: k.workspace_id,
            label: k.label,
        })
}

/// Run `f` on the root (and window) that owns `target`.
fn with_root(cx: &mut AsyncApp, target: &Target, f: impl FnOnce(&mut WorkspaceRoot, &mut Window, &mut Context<WorkspaceRoot>)) {
    let key = target.window.clone();
    cx.update(|cx| {
        let Some(handle) = window_registry::window_handle(cx, &key) else { return };
        let Some((_, root)) = window_registry::all_windows(cx).into_iter().find(|(k, _)| *k == key) else { return };
        let _ = handle.update(cx, |_, window, cx| root.update(cx, |root, cx| f(root, window, cx)));
    });
}

/// The kill switch, the attached device, then the user's consent for it.
/// Returns the device and its name.
fn authorize(hub: &Entity<SimulatorHub>, target: &Target, cx: &mut AsyncApp) -> Result<(DeviceId, String), SimErrorWire> {
    let checked = cx
        .update(|cx| {
            if !super::panel::settings(cx).agent_control {
                return Err(SimErrorWire::AgentControlDisabled);
            }
            hub.update(cx, |hub, cx| {
                let udid = hub.device_for(&target.worktree).ok_or(SimErrorWire::NoDevice)?;
                // Host-resolved, never caller text: it is shown to the user.
                let name = device_name(hub, &udid);
                let (verdict, raised) = hub.consent_check(&udid, &target.worktree, &name, cx);
                Ok((udid, name, verdict, raised))
            })
    })?;
    let (udid, name, verdict, raised) = checked;
    if raised {
        let (target_for_root, name_for_root) = (target.clone(), name.clone());
        with_root(cx, target, move |root, window, cx| {
            root.ask_simulator_consent(
                &target_for_root.worktree,
                &target_for_root.label,
                &name_for_root,
                (target_for_root.project_id.clone(), target_for_root.workspace_id.clone()),
                window,
                cx,
            );
        });
    }
    match verdict {
        Verdict::Allowed => Ok((udid, name)),
        Verdict::Pending => Err(SimErrorWire::ConsentPending),
        Verdict::Denied { retry_after } => Err(SimErrorWire::ConsentDenied { retry_after_secs: retry_after.as_secs().max(1) }),
    }
}

fn device_name(hub: &SimulatorHub, udid: &DeviceId) -> String {
    hub.devices().iter().find(|d| &d.udid == udid).map(|d| d.name.clone()).unwrap_or_else(|| "this simulator".into())
}

fn status(hub: &Entity<SimulatorHub>, target: &Target, cx: &mut AsyncApp) -> Result<SimReplyWire, SimErrorWire> {
    cx.update(|cx| {
        let agent_control = super::panel::settings(cx).agent_control;
        hub.update(cx, |hub, _| {
            let android = hub.android_sdk().is_some();
            let (available, reason, xcode) = match hub.availability() {
                None if android => (true, None, None),
                None => (false, Some("still checking for Xcode; try again in a moment".to_owned()), None),
                Some(a) => {
                    let xcode = match &a.xcode {
                        oximux_simulator::availability::Xcode::Found { version, .. } => version.clone(),
                        _ => None,
                    };
                    // Android devices work without the iOS side.
                    let reason = a.blocking_reason().filter(|_| !android);
                    (a.is_ready() || android, reason, xcode)
                }
            };
            let udid = hub.device_for(&target.worktree);
            let device = udid.as_ref().map(|udid| match hub.devices().iter().find(|d| &d.udid == udid) {
                Some(info) => device_wire(info),
                None => SimDeviceWire { udid: udid.to_string(), name: "unknown".into(), runtime: String::new(), state: String::new() },
            });
            let streaming = udid.as_ref().is_some_and(|u| matches!(hub.phase(u), oximux_simulator::registry::Phase::Live { .. }));
            let consent = match &udid {
                None => SimConsentWire::NotAsked,
                Some(udid) => match hub.consent_state(udid, &target.worktree) {
                    State::NotAsked => SimConsentWire::NotAsked,
                    State::Pending => SimConsentWire::Pending,
                    State::Allowed => SimConsentWire::Allowed,
                    State::Denied { retry_after } => SimConsentWire::Denied { retry_after_secs: retry_after.as_secs().max(1) },
                },
            };
            Ok(SimReplyWire::Status(SimStatusWire {
                available,
                reason,
                xcode,
                worktree: target.worktree.to_string_lossy().into_owned(),
                device,
                streaming,
                consent,
                agent_control,
            }))
        })
    })
}

/// A fresh device listing: iOS simulators (never without a resolvable Xcode,
/// which would pop the command-line-tools dialog) and Android devices (with
/// an SDK).
async fn list_devices(hub: &Entity<SimulatorHub>, cx: &mut AsyncApp) -> Result<Vec<DeviceInfo>, SimErrorWire> {
    let (xcode_ok, sdk) = hub.read_with(cx, |hub, _| (hub.xcode_ok(), hub.android_sdk().cloned()));
    if !xcode_ok && sdk.is_none() {
        return Err(SimErrorWire::Unavailable(
            "neither Xcode nor an Android SDK was found (or they are still being checked); see `oximux sim status`".into(),
        ));
    }
    cx.background_executor()
        .spawn(async move { super::hub::list_all(&SystemRunner, xcode_ok, sdk.as_ref(), LIST_TIMEOUT) })
        .await
        .map_err(|e| SimErrorWire::Failed(format!("could not list simulators: {e}")))
}

async fn attach(hub: &Entity<SimulatorHub>, target: &Target, device: Option<String>, cx: &mut AsyncApp) -> Result<SimReplyWire, SimErrorWire> {
    let devices = list_devices(hub, cx).await?;
    let wanted = match device.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        None => None,
        Some(name) => Some(
            pick_device(&devices, name)
                .ok_or_else(|| SimErrorWire::NotFound(format!("no simulator named “{name}” (see `oximux sim devices`)")))?,
        ),
    };
    let preferred = cx.update(|cx| super::panel::settings(cx).default_device.map(DeviceId));
    let info = hub
        .update(cx, |hub, cx| hub.attach_for_agent(&target.worktree, Ok(devices), wanted.as_ref(), preferred.as_ref(), cx))
        .map_err(SimErrorWire::Failed)?;
    // Show it where the user is looking at this worktree.
    let worktree = target.worktree.clone();
    with_root(cx, target, move |root, _, cx| root.reveal_simulator_for(&worktree, Trigger::Attach, cx));
    Ok(SimReplyWire::Attached(device_wire(&info)))
}

/// A device by udid, else by name (case-insensitive). Names repeat across
/// runtimes: a booted one wins, then the newest runtime.
fn pick_device(devices: &[DeviceInfo], wanted: &str) -> Option<DeviceId> {
    if let Some(d) = devices.iter().find(|d| d.udid.as_str().eq_ignore_ascii_case(wanted)) {
        return Some(d.udid.clone());
    }
    devices
        .iter()
        .filter(|d| d.is_available && d.name.eq_ignore_ascii_case(wanted))
        .max_by(|a, b| {
            (a.state == DeviceState::Booted)
                .cmp(&(b.state == DeviceState::Booted))
                .then_with(|| version_key(&a.os_version).cmp(&version_key(&b.os_version)))
        })
        .map(|d| d.udid.clone())
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn device_wire(d: &DeviceInfo) -> SimDeviceWire {
    // `com.apple.CoreSimulator.SimRuntime.iOS-26-3` → `iOS 26.3`.
    let platform = d.runtime.rsplit('.').next().and_then(|r| r.split('-').next()).unwrap_or("iOS");
    let state = match &d.state {
        DeviceState::Shutdown => "Shutdown",
        DeviceState::Booting => "Booting",
        DeviceState::Booted => "Booted",
        DeviceState::ShuttingDown => "Shutting Down",
        DeviceState::Creating => "Creating",
        DeviceState::Other(s) => s.as_str(),
    };
    SimDeviceWire {
        udid: d.udid.to_string(),
        name: d.name.clone(),
        // Android's runtime is already "Android 16" / "Android API 37.1".
        runtime: match d.udid.platform() {
            oximux_simulator::Platform::Android => d.runtime.clone(),
            oximux_simulator::Platform::Ios => format!("{platform} {}", d.os_version),
        },
        state: state.to_owned(),
    }
}

/// Whether `a` and `b` name the same worktree.
pub(crate) fn same_worktree(a: &Path, b: &Path) -> bool {
    WorktreeKey::from_path(a) == WorktreeKey::from_path(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_simulator::DeviceKind;

    fn info(udid: &str, name: &str, os: &str, state: DeviceState) -> DeviceInfo {
        DeviceInfo {
            udid: DeviceId(udid.into()),
            name: name.into(),
            runtime: format!("com.apple.CoreSimulator.SimRuntime.iOS-{}", os.replace('.', "-")),
            os_version: os.into(),
            state,
            kind: DeviceKind::Phone,
            is_available: true,
        }
    }

    #[test]
    fn a_device_is_found_by_udid_or_name() {
        let devices = [
            info("A", "iPhone 17 Pro", "26.0", DeviceState::Shutdown),
            info("B", "iPhone 17 Pro", "26.3", DeviceState::Shutdown),
            info("C", "iPhone 17 Pro", "26.1", DeviceState::Booted),
            info("D", "iPad Air", "26.3", DeviceState::Shutdown),
        ];
        assert_eq!(pick_device(&devices, "a"), Some(DeviceId("A".into())), "udid, any case");
        assert_eq!(pick_device(&devices, "iphone 17 pro"), Some(DeviceId("C".into())), "the booted one wins");
        assert_eq!(pick_device(&devices[..2], "iPhone 17 Pro"), Some(DeviceId("B".into())), "then the newest runtime");
        assert_eq!(pick_device(&devices, "iPhone 99"), None);
    }

    #[test]
    fn devices_read_as_platform_and_version() {
        let wire = device_wire(&info("A", "iPhone 17 Pro", "26.3", DeviceState::ShuttingDown));
        assert_eq!((wire.runtime.as_str(), wire.state.as_str()), ("iOS 26.3", "Shutting Down"));
    }

    fn known(path: &str, window: &str, active: bool) -> Known {
        Known {
            path: path.into(),
            window: window.into(),
            active,
            front: 0,
            project_id: "p".into(),
            workspace_id: path.into(),
            label: path.into(),
        }
    }

    #[test]
    fn a_path_resolves_to_the_deepest_known_worktree() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("proj");
        let nested = project.join(".worktrees/feat");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let list = vec![known(&s(&project), "main", false), known(&s(&nested), "main", false)];
        let target = resolve(&s(&nested.join("src")), list.clone()).unwrap();
        assert_eq!(target.worktree, nested, "the worktree, not the project around it");
        assert_eq!(resolve(&s(&project), list.clone()).unwrap().worktree, project);
        assert!(resolve(&s(root.path()), list).is_none(), "outside every worktree");
    }

    #[test]
    fn a_window_showing_the_worktree_wins_a_tie() {
        let root = tempfile::tempdir().unwrap();
        let s = root.path().to_string_lossy().into_owned();
        let list = vec![known(&s, "w-1", false), known(&s, "w-2", true), known(&s, "w-3", false)];
        assert_eq!(resolve(&s, list).unwrap().window, "w-2");
    }

    /// Known to several windows and active in none: the frontmost takes it,
    /// never whichever the registry lists first (P8 review L6).
    #[test]
    fn otherwise_the_frontmost_window_wins() {
        let root = tempfile::tempdir().unwrap();
        let s = root.path().to_string_lossy().into_owned();
        let at = |window: &str, front: usize| Known { front, ..known(&s, window, false) };
        let list = vec![at("w-1", 2), at("w-2", 0), at("w-3", usize::MAX)];
        assert_eq!(resolve(&s, list).unwrap().window, "w-2");
    }
}
