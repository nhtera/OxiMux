//! `oximux sim flow start|stop|status` and a run's one approval, for the
//! worktree's agent: its recording is the verbs it sends (see
//! [`record`]); the panel's recording is out of its reach.

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{SimCmdWire, SimErrorWire, SimFlowCmdWire, SimFlowWire, SimReplyWire, SimSelectorWire, SimTargetWire, render_step};
use oximux_simulator::ax::{self, Query};
use oximux_simulator::ax_refs::TableKey;
use oximux_simulator::registry::WorktreeKey;
use oximux_simulator::{DeviceId, Source, agent};

use super::Target;
use super::device_state::confirm;
use super::verbs::Out;
use crate::shell::simulator::hub::{AskKind, HubEvent, RecSource, SimulatorHub};

pub(super) async fn run(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, cmd: SimFlowCmdWire, target: &Target, cx: &mut AsyncApp) -> Out {
    let key = (WorktreeKey::from_path(&target.worktree), udid.clone(), RecSource::Agent);
    match cmd {
        SimFlowCmdWire::Start => hub.update(cx, |hub, cx| {
            let screen = screen_points(hub, udid);
            if hub.flows_mut().start(key.clone(), platform(udid), screen) {
                cx.emit(HubEvent::Flow);
            }
            Ok(reply(hub.flows().get(&key).map(|r| r.wire(true, None))))
        }),
        SimFlowCmdWire::Status => Ok(reply(hub.read_with(cx, |hub, _| hub.flows().get(&key).map(|r| r.wire(true, None))))),
        SimFlowCmdWire::Stop { save } => {
            let stopped = hub.update(cx, |hub, cx| {
                let stopped = hub.flows_mut().stop(&key);
                if stopped.is_some() {
                    cx.emit(HubEvent::Flow);
                }
                stopped
            });
            let recording = stopped.ok_or_else(|| SimErrorWire::Refused("nothing is recording; start with `oximux sim flow start`".into()))?;
            let text = recording.text();
            let saved = if save {
                let worktree = key.0.clone();
                let path = cx.background_executor().spawn(async move { crate::shell::simulator::hub::save_flow(&worktree, &text) }).await;
                Some(path.map_err(SimErrorWire::Failed)?.to_string_lossy().into_owned())
            } else {
                None
            };
            Ok(SimReplyWire::Flow(recording.wire(false, saved)))
        }
        SimFlowCmdWire::ApproveRun { lines } => {
            if lines.is_empty() {
                return Ok(SimReplyWire::Done);
            }
            confirm(hub, udid, name, AskKind::FlowRun { lines }, target, cx).await?;
            Ok(SimReplyWire::Done)
        }
    }
}

fn reply(wire: Option<SimFlowWire>) -> SimReplyWire {
    SimReplyWire::Flow(wire.unwrap_or(SimFlowWire { recording: false, steps: String::new(), count: 0, warnings: Vec::new(), saved: None }))
}

/// "ios" or "android", for a flow's header.
pub(crate) fn platform(udid: &DeviceId) -> &'static str {
    if udid.source() == Source::Adb { "android" } else { "ios" }
}

/// The device's screen in points, the way it is turned now.
pub(crate) fn screen_points(hub: &SimulatorHub, udid: &DeviceId) -> Option<(f64, f64)> {
    let session = hub.session(udid)?;
    let (w, h) = session.framebuffer_size()?;
    let scale = hub.scale(udid).or_else(|| session.point_scale())?;
    let size = agent::display_points(session.orientation(), oximux_simulator::geometry::Size::new(f64::from(w), f64::from(h)), scale);
    Some((size.w, size.h))
}

/// What an agent's recording keeps of a verb about to run: its step lines,
/// with an `@eN` ref (or a plain label) turned into the exact selector of
/// the element it names on the caller's last read — or why it cannot be
/// written down. `None` when nothing records this worktree's agent.
pub(super) fn plan(hub: &Entity<SimulatorHub>, cmd: &SimCmdWire, settle: Option<u32>, refs: &TableKey, cx: &mut AsyncApp) -> Option<Result<Vec<String>, String>> {
    let key = (refs.0.clone(), refs.1.clone(), RecSource::Agent);
    hub.update(cx, |hub, _| {
        hub.flows().get(&key)?;
        let named = name_targets(hub, cmd, refs);
        Some(named.and_then(|cmd| {
            let cmd = match settle {
                Some(timeout_ms) => SimCmdWire::Settled { action: Box::new(cmd), timeout_ms },
                None => cmd,
            };
            render_step(&cmd)
        }))
    })
}

/// Keep `planned` in the agent's recording once its verb succeeded.
pub(super) fn record(hub: &Entity<SimulatorHub>, refs: &TableKey, planned: Result<Vec<String>, String>, cx: &mut AsyncApp) {
    let key = (refs.0.clone(), refs.1.clone(), RecSource::Agent);
    hub.update(cx, |hub, cx| {
        let Some(recording) = hub.flows_mut().get_mut(&key) else { return };
        match planned {
            Ok(lines) if lines.is_empty() => return,
            Ok(lines) => recording.push(lines),
            Err(why) => recording.warn(format!("not recorded: {why}")),
        }
        cx.emit(HubEvent::Flow);
    });
}

/// `cmd` with each target named by what it is, not by a ref.
fn name_targets(hub: &mut SimulatorHub, cmd: &SimCmdWire, refs: &TableKey) -> Result<SimCmdWire, String> {
    let mut name = |aim: &SimTargetWire| -> Result<SimTargetWire, String> {
        let table = hub.ref_table(refs);
        let Some(last) = table.last.clone() else {
            return match aim {
                SimTargetWire::Ref(r) => Err(format!("@e{r} names nothing this recording can see")),
                other => Ok(other.clone()),
            };
        };
        let node = match aim {
            SimTargetWire::Ref(r) => table.resolve(*r, &last.nodes, 1.0).map(|f| f.node).map_err(|_| format!("@e{r} no longer names one element"))?,
            // A label may have matched part of another: named exactly,
            // when the last read shows which element it was.
            SimTargetWire::Label(label) => match ax::find(&last.nodes, Query::Label(label)) {
                Some(node) => node,
                None => return Ok(aim.clone()),
            },
            other => return Ok(other.clone()),
        };
        let sel = ax::exact_selector(&last.nodes, node).ok_or("the element has neither a label nor a role")?;
        Ok(SimTargetWire::Exact(SimSelectorWire { label: sel.label, id: sel.id, role: sel.role, nth: u32::try_from(sel.nth).unwrap_or(u32::MAX) }))
    };
    Ok(match cmd {
        SimCmdWire::Tap(aim) => SimCmdWire::Tap(name(aim)?),
        SimCmdWire::TypeInto { target, text, paste } => SimCmdWire::TypeInto { target: name(target)?, text: text.clone(), paste: *paste },
        SimCmdWire::Wait { target, gone, timeout_ms } => SimCmdWire::Wait { target: name(target)?, gone: *gone, timeout_ms: *timeout_ms },
        SimCmdWire::Assert { target, check } => SimCmdWire::Assert { target: name(target)?, check: check.clone() },
        other => other.clone(),
    })
}
