//! `wait` and `assert`: read the screen for an element instead of sleeping.
//!
//! `wait` reads until the element is there (or, `--gone`, is not); `assert`
//! reads once and checks. Both name an element by label, identifier or `@eN`
//! ref, and every read renumbers the caller's refs, as on the settle path.

use std::time::{Duration, Instant};

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{SimCheckWire, SimErrorWire, SimReplyWire, SimTargetWire};
use oximux_simulator::DeviceId;
use oximux_simulator::ax::{self, AxNode, Query};
use oximux_simulator::ax_refs::{RefError, TableKey};

use super::ax_lookup::{Shown, units_per_point};
use super::settle::{MAX_WAIT, Read, millis, read_now, slow, stale};
use super::verbs::{Out, live_session};
use crate::shell::simulator::hub::SimulatorHub;

/// `wait` without `--timeout-ms`.
const WAIT_DEFAULT: Duration = Duration::from_secs(10);
/// How often `wait` reads (at least the last read's own time, on a device
/// whose reads are slow).
const POLL: Duration = Duration::from_millis(250);

/// `wait`: read until the element is there (or, `gone`, is not), or time is
/// up. A read that fails (Android's dump does, mid-animation) is no answer
/// yet: the wait goes on, and says so if it runs out.
pub(super) async fn wait(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    aim: SimTargetWire,
    gone: bool,
    timeout_ms: u32,
    refs: &TableKey,
    cx: &mut AsyncApp,
) -> Out {
    check_target(&aim)?;
    let session = live_session(hub, udid, cx).await?;
    let timeout = if timeout_ms == 0 { WAIT_DEFAULT } else { Duration::from_millis(u64::from(timeout_ms)).min(MAX_WAIT) };
    let (started, pt) = (Instant::now(), units_per_point(&session));
    let mut last_error = None;
    loop {
        let asked = Instant::now();
        match read_now(hub, udid, &session, Some(started), false, cx).await {
            // An empty tree is a screen in transition, not an answer either way.
            Ok(read) if !read.nodes.is_empty() => {
                let found = locate(hub, &aim, &read, pt, refs, cx)?;
                if found.is_some() != gone {
                    let node = found.and_then(|n| Shown::new(hub, udid, &session, &read.nodes, cx).ok().map(|s| s.wire(&n, 0)));
                    return Ok(SimReplyWire::Matched { node, elapsed_ms: millis(started.elapsed()) });
                }
            }
            Ok(_) => {}
            Err(e) => last_error = Some(e.to_string()),
        }
        let took = asked.elapsed();
        let left = timeout.saturating_sub(started.elapsed());
        if left.is_zero() {
            let happen = if gone { "go away" } else { "appear" };
            let why = last_error.map(|e| format!(" (last read: {e})")).unwrap_or_default();
            return Err(SimErrorWire::TimedOut(format!("{} did not {happen} within {} ms{why}", target_name(&aim), timeout.as_millis())));
        }
        let pause = if slow(&session) { POLL.max(took) } else { POLL };
        cx.background_executor().timer(pause.min(left)).await;
    }
}

/// `assert`: one fresh read, one check.
pub(super) async fn assert(hub: &Entity<SimulatorHub>, udid: &DeviceId, aim: SimTargetWire, check: SimCheckWire, refs: &TableKey, cx: &mut AsyncApp) -> Out {
    check_target(&aim)?;
    let session = live_session(hub, udid, cx).await?;
    let read = read_now(hub, udid, &session, None, true, cx).await?;
    let found = locate(hub, &aim, &read, units_per_point(&session), refs, cx)?;
    verdict(&check, found.as_ref(), &target_name(&aim)).map_err(SimErrorWire::AssertFailed)?;
    let node = found.and_then(|n| Shown::new(hub, udid, &session, &read.nodes, cx).ok().map(|s| s.wire(&n, 0)));
    Ok(SimReplyWire::Matched { node, elapsed_ms: millis(read.at.elapsed()) })
}

/// Whether `check` holds of `found` (the element `what` names, if on
/// screen); otherwise what was found instead.
fn verdict(check: &SimCheckWire, found: Option<&AxNode>, what: &str) -> Result<(), String> {
    match (check, found) {
        (SimCheckWire::Exists, Some(_)) | (SimCheckWire::Gone, None) => Ok(()),
        (SimCheckWire::Exists | SimCheckWire::Text(_) | SimCheckWire::Enabled, None) => Err(format!("no on-screen element matches {what}")),
        (SimCheckWire::Gone, Some(n)) => Err(format!("{what} is on screen: {}", summary(n))),
        (SimCheckWire::Text(want), Some(n)) => match text_of(n) {
            Some(text) if text == want.trim() => Ok(()),
            Some(text) => Err(format!("{what} reads “{text}”, not “{}”", want.trim())),
            None => Err(format!("{what} has no text (wanted “{}”)", want.trim())),
        },
        (SimCheckWire::Enabled, Some(n)) if n.enabled => Ok(()),
        (SimCheckWire::Enabled, Some(n)) => Err(format!("{what} is disabled: {}", summary(n))),
    }
}

/// A field's text is its value — empty when it is cleared, never its label
/// (the label names the field: “Email”); anything else's is its value or,
/// without one, its label.
fn text_of(n: &AxNode) -> Option<&str> {
    let value = n.value.as_deref().map(str::trim);
    if is_text_entry(&n.role) && value.is_some() {
        return value;
    }
    [value, n.label.as_deref().map(str::trim)].into_iter().flatten().find(|s| !s.is_empty())
}

fn is_text_entry(role: &str) -> bool {
    matches!(role, "TextField" | "SecureTextField" | "SearchField" | "TextView")
}

fn summary(n: &AxNode) -> String {
    match text_of(n).filter(|t| !t.is_empty()) {
        Some(text) => format!("{} “{text}”", n.role),
        None => n.role.clone(),
    }
}

fn check_target(aim: &SimTargetWire) -> Result<(), SimErrorWire> {
    match aim {
        SimTargetWire::Point(_) => Err(SimErrorWire::BadInput("name an element (--label, --id or @eN), not a point".into())),
        SimTargetWire::Label(_) | SimTargetWire::Id(_) | SimTargetWire::Ref(_) => Ok(()),
    }
}

fn target_name(aim: &SimTargetWire) -> String {
    match aim {
        SimTargetWire::Label(label) => format!("“{label}”"),
        SimTargetWire::Id(id) => format!("#{id}"),
        SimTargetWire::Ref(r) => format!("@e{r}"),
        SimTargetWire::Point(p) => format!("({}, {})", p.x, p.y),
    }
}

/// The element `aim` names in `read`, renumbering the caller's refs from
/// this read on the way. A ref is held to `tap @eN`'s rule: an element that
/// can no longer be told from a look-alike is a stale ref, not a guess.
fn locate(hub: &Entity<SimulatorHub>, aim: &SimTargetWire, read: &Read, pt: f64, refs: &TableKey, cx: &mut AsyncApp) -> Result<Option<AxNode>, SimErrorWire> {
    hub.update(cx, |hub, _| {
        let table = hub.ref_table(refs);
        let found = match aim {
            SimTargetWire::Label(label) => ax::find_on_screen(&read.nodes, Query::Label(label)).cloned(),
            SimTargetWire::Id(id) => ax::find_on_screen(&read.nodes, Query::Id(id)).cloned(),
            SimTargetWire::Ref(r) => match table.resolve(*r, &read.nodes, pt) {
                Ok(found) => Some(found.node.clone()),
                Err(RefError::Gone) => None,
                Err(e @ (RefError::Unknown | RefError::Ambiguous)) => return Err(stale(*r, e)),
            },
            SimTargetWire::Point(_) => None,
        };
        table.assign(&read.nodes);
        table.last = Some(read.snapshot());
        Ok(found)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_proto::simulator::SimPointWire;
    use oximux_simulator::geometry::Rect;

    fn node(role: &str, label: Option<&str>, value: Option<&str>, enabled: bool) -> AxNode {
        AxNode {
            label: label.map(Into::into),
            identifier: None,
            value: value.map(Into::into),
            role: role.into(),
            role_description: String::new(),
            enabled,
            frame: Rect::new(0.0, 0.0, 10.0, 10.0),
            children: Vec::new(),
        }
    }

    #[test]
    fn an_assert_says_what_it_found() {
        let field = node("TextField", Some("Email"), Some("a@b.co"), true);
        let off = node("Button", Some("Next"), None, false);
        assert!(verdict(&SimCheckWire::Exists, Some(&field), "x").is_ok());
        assert!(verdict(&SimCheckWire::Gone, None, "x").is_ok());
        assert!(verdict(&SimCheckWire::Text(" a@b.co ".into()), Some(&field), "x").is_ok());
        assert!(verdict(&SimCheckWire::Text("Next".into()), Some(&off), "x").is_ok(), "no value: the label");
        assert_eq!(verdict(&SimCheckWire::Text("z".into()), Some(&field), "“Email”").unwrap_err(), "“Email” reads “a@b.co”, not “z”");
        assert_eq!(verdict(&SimCheckWire::Enabled, Some(&off), "#next").unwrap_err(), "#next is disabled: Button “Next”");
        assert_eq!(verdict(&SimCheckWire::Gone, Some(&off), "@e3").unwrap_err(), "@e3 is on screen: Button “Next”");
        assert!(verdict(&SimCheckWire::Exists, None, "x").unwrap_err().contains("no on-screen element"));
    }

    /// A cleared field reads as empty, never as its label; a button whose
    /// value is empty still reads as its label.
    #[test]
    fn a_cleared_field_reads_empty() {
        let cleared = node("TextField", Some("Email"), Some(""), true);
        assert!(verdict(&SimCheckWire::Text(String::new()), Some(&cleared), "x").is_ok());
        assert_eq!(verdict(&SimCheckWire::Text("Email".into()), Some(&cleared), "x").unwrap_err(), "x reads “”, not “Email”");
        let button = node("Button", Some("Sign in"), Some(""), true);
        assert!(verdict(&SimCheckWire::Text("Sign in".into()), Some(&button), "x").is_ok());
        assert_eq!(summary(&cleared), "TextField");
    }

    #[test]
    fn a_point_is_not_an_element() {
        assert!(matches!(check_target(&SimTargetWire::Point(SimPointWire { x: 1.0, y: 2.0 })), Err(SimErrorWire::BadInput(_))));
        assert!(check_target(&SimTargetWire::Ref(4)).is_ok());
        assert_eq!(target_name(&SimTargetWire::Ref(4)), "@e4");
    }
}
