//! `oximux sim flow start|stop|status` and a run's one approval, for the
//! worktree's agent: its recording is the verbs it sends (see
//! [`record`]); the panel's recording is out of its reach.

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{SimCmdWire, SimErrorWire, SimFlowCmdWire, SimFlowWire, SimReplyWire, SimSelectorWire, SimTargetWire};
use oximux_simulator::ax::{self, AxNode, Query};
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
        SimFlowCmdWire::Start => {
            // The scale a point step needs: read now if no verb has yet.
            if let Some(session) = hub.read_with(cx, |hub, _| hub.session(udid)) {
                let _ = super::ax_lookup::scale(hub, udid, &session, cx).await;
            }
            hub.update(cx, |hub, cx| {
            let screen = screen_points(hub, udid);
            if hub.flows_mut().start(key.clone(), platform(udid), screen) {
                cx.emit(HubEvent::Flow);
            }
            Ok(reply(hub.flows().get(&key).map(|r| r.wire(true, None))))
            })
        }
        SimFlowCmdWire::Status => Ok(reply(hub.read_with(cx, |hub, _| hub.flows().get(&key).map(|r| r.wire(true, None))))),
        SimFlowCmdWire::Stop { save } => {
            // Stopped first, so what is saved is what is returned; a failed
            // save puts it back.
            let stopped = hub.update(cx, |hub, cx| {
                let stopped = hub.flows_mut().stop(&key);
                cx.emit(HubEvent::Flow);
                stopped
            });
            let recording = stopped.ok_or_else(|| SimErrorWire::Refused("nothing is recording; start with `oximux sim flow start`".into()))?;
            let saved = if save {
                let (worktree, text) = (key.0.clone(), recording.text());
                match cx.background_executor().spawn(async move { crate::shell::simulator::hub::save_flow(&worktree, &text) }).await {
                    Ok(path) => Some(path.to_string_lossy().into_owned()),
                    Err(why) => {
                        hub.update(cx, |hub, cx| {
                            hub.flows_mut().restore(key, recording);
                            cx.emit(HubEvent::Flow);
                        });
                        return Err(SimErrorWire::Failed(format!("{why}; still recording — retry, or `flow stop --out FILE`")));
                    }
                }
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

/// A verb as an agent's recording keeps it: which recording it is for, its
/// targets named, and whether its own target is a password field (see
/// `Recording::agent`).
pub(super) type Planned = (u64, Result<(SimCmdWire, Option<Option<bool>>), String>);

/// How old a read may be to say what a looked-up field is.
const FIELD_READ_AGE: std::time::Duration = std::time::Duration::from_secs(10);

/// What an agent's recording keeps of a verb about to run: an `@eN` ref
/// turned into the exact selector of the element it names on the caller's
/// last read (where the ref came from) — or why it cannot be written down.
/// `None` when nothing records this worktree's agent.
pub(super) fn plan(hub: &Entity<SimulatorHub>, cmd: &SimCmdWire, settle: Option<u32>, refs: &TableKey, cx: &mut AsyncApp) -> Option<Planned> {
    let key = (refs.0.clone(), refs.1.clone(), RecSource::Agent);
    hub.update(cx, |hub, _| {
        let id = hub.flows().get(&key)?.id();
        Some((
            id,
            name_targets(hub, cmd, refs).map(|(cmd, field)| {
                let cmd = match settle {
                    Some(timeout_ms) => SimCmdWire::Settled { action: Box::new(cmd), timeout_ms },
                    None => cmd,
                };
                (cmd, field)
            }),
        ))
    })
}

/// Keep `planned` in the agent's recording once its verb succeeded.
pub(super) fn record(hub: &Entity<SimulatorHub>, refs: &TableKey, planned: Planned, cx: &mut AsyncApp) {
    let key = (refs.0.clone(), refs.1.clone(), RecSource::Agent);
    hub.update(cx, |hub, cx| {
        let screen = screen_points(hub, &key.1);
        let (id, planned) = planned;
        // Only into the recording it was planned for (not one started since).
        let Some(recording) = hub.flows_mut().get_mut(&key).filter(|r| r.id() == id) else { return };
        recording.fill_screen(screen);
        match planned {
            Ok((cmd, field)) => recording.agent(cmd, field),
            Err(why) => recording.warn(format!("not recorded: {why}")),
        }
        cx.emit(HubEvent::Flow);
    });
}

/// `cmd` with a ref named by what its element is, and whether the verb's
/// own target is a password field (`Some(None)`: not known). A label or an
/// identifier stays as the agent wrote it: it is looked up only to say
/// whether typing there is a secret.
fn name_targets(hub: &mut SimulatorHub, cmd: &SimCmdWire, refs: &TableKey) -> Result<(SimCmdWire, Option<Option<bool>>), String> {
    let mut name = |aim: &SimTargetWire| -> Result<(SimTargetWire, Option<bool>), String> {
        let table = hub.ref_table(refs);
        let last = table.last.clone();
        let nodes = last.as_ref().map(|l| l.nodes.as_slice()).unwrap_or_default();
        // A label looked up in an old read may be another screen's: only a
        // recent one says what the field is.
        let recent = last.as_ref().is_some_and(|l| l.taken_at.elapsed() < FIELD_READ_AGE);
        let secure = |node: Option<&AxNode>| node.filter(|_| recent).and_then(ax::field_kind);
        Ok(match aim {
            SimTargetWire::Ref(r) => {
                let node = table.resolve(*r, nodes, 1.0).map(|f| f.node).map_err(|_| format!("@e{r} no longer names one element"))?;
                let sel = ax::exact_selector(nodes, node).ok_or("the element has neither a label nor a role")?;
                let nth = u32::try_from(sel.nth).unwrap_or(u32::MAX);
                (SimTargetWire::Exact(SimSelectorWire { label: sel.label, id: sel.id, role: sel.role, nth }), ax::field_kind(node))
            }
            SimTargetWire::Label(label) => (aim.clone(), secure(ax::find(nodes, Query::Label(label)))),
            SimTargetWire::Id(id) => (aim.clone(), secure(ax::find(nodes, Query::Id(id)))),
            SimTargetWire::Exact(sel) => (aim.clone(), secure(ax::find_exact(nodes, &super::ax_lookup::exact_of(sel)))),
            SimTargetWire::Point(_) => (aim.clone(), None),
        })
    };
    Ok(match cmd {
        SimCmdWire::Tap(aim) => {
            let (aim, field) = name(aim)?;
            (SimCmdWire::Tap(aim), Some(field))
        }
        SimCmdWire::TypeInto { target, text, paste } => {
            let (target, field) = name(target)?;
            (SimCmdWire::TypeInto { target, text: text.clone(), paste: *paste }, Some(field))
        }
        SimCmdWire::Wait { target, gone, timeout_ms } => (SimCmdWire::Wait { target: name(target)?.0, gone: *gone, timeout_ms: *timeout_ms }, None),
        SimCmdWire::Assert { target, check } => (SimCmdWire::Assert { target: name(target)?.0, check: check.clone() }, None),
        other => (other.clone(), None),
    })
}
