//! `oximux sim`'s verify loop (v27): `--settle` on an action, `ax -i` and
//! `@eN` refs, `wait` and `assert`.
//!
//! A command that uses one of them is wrapped for its caller
//! ([`SimCmdWire::AsCaller`]) when it runs inside an OxiMux agent session, so
//! two agents sharing a worktree keep refs of their own. Outside a session
//! the worktree's device has one shared table. (A parent pid is no caller id:
//! an agent's shell tool runs each command in a fresh shell.)

use std::path::Path;
use std::time::Duration;

use oximux_remote_local::SESSION_ENV_VAR;
use oximux_remote_proto::simulator::{SimCmdWire, SimReplyWire, SimTargetWire};
use serde_json::json;

use super::sim::{Outcome, SLOW, call, unexpected, usage};
use super::sim_render as render;
use crate::cli::{AssertArgs, SettleArgs, WaitArgs};
use crate::cli_sim::parse_ref;
use crate::client::Client;

/// The longest a settle waits when the command does not say (launch and
/// open-url's default; the host picks the class's own).
const DEFAULT_SETTLE: Duration = Duration::from_millis(8000);
/// Past a wait's own bound: the reply's trip, and a margin.
const MARGIN: Duration = Duration::from_secs(2);

/// This caller, as the host's ref tables know it: a hash of the agent
/// session it runs in.
pub fn caller() -> Option<u64> {
    caller_from(std::env::var(SESSION_ENV_VAR).ok().as_deref())
}

fn caller_from(session: Option<&str>) -> Option<u64> {
    let session = session.map(str::trim).filter(|s| !s.is_empty())?;
    // FNV-1a: stable across builds and platforms (the host compares the
    // numbers only; nothing is reversed).
    Some(session.bytes().fold(0xcbf2_9ce4_8422_2325, |h: u64, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)))
}

/// `cmd` on behalf of this caller, when it is a v27 verb.
pub fn as_caller(cmd: SimCmdWire) -> SimCmdWire {
    match caller() {
        Some(caller) if cmd.uses_verify() => SimCmdWire::AsCaller { caller, cmd: Box::new(cmd) },
        _ => cmd,
    }
}

/// `cmd` settled (when asked) and on this caller's behalf, and the time the
/// host may take for it.
pub fn wrap(cmd: SimCmdWire, floor: Duration, settle: &SettleArgs) -> (SimCmdWire, Duration) {
    let (cmd, floor) = if settle.settle {
        let cap = settle.settle_timeout.map_or(DEFAULT_SETTLE, |ms| Duration::from_millis(u64::from(ms)));
        let timeout_ms = settle.settle_timeout.unwrap_or(0);
        (SimCmdWire::Settled { action: Box::new(cmd), timeout_ms }, floor + cap + MARGIN)
    } else {
        (cmd, floor)
    };
    (as_caller(cmd), floor)
}

/// An action's reply: a settle report, or plain done. `named`: the verb
/// already names its element.
pub fn acted(reply: SimReplyWire, said: &str, named: bool) -> Outcome {
    match reply {
        SimReplyWire::Settled(report) => Ok((render::settle_json(&report), render::settle_human(said, &report, named))),
        SimReplyWire::Done => Ok((json!({ "ok": true }), said.to_owned())),
        _ => Err(unexpected("an action")),
    }
}

/// `ax -i`.
pub async fn ax_refs(client: &Client, worktree: &Path, max: u32) -> Outcome {
    match call(client, worktree, as_caller(SimCmdWire::AxRefs { max }), SLOW).await? {
        SimReplyWire::AxRefs(nodes) => Ok((render::refs_json(&nodes), render::refs_human(&nodes))),
        _ => Err(unexpected("AxRefs")),
    }
}

/// `wait`: exit 0 once it holds, 4 (`timeout`) when time runs out.
pub async fn wait(client: &Client, worktree: &Path, args: WaitArgs) -> Outcome {
    let target = args.element.target();
    let what = format!("{} {}", name(&target), if args.gone { "went" } else { "appeared" });
    let cmd = SimCmdWire::Wait { target, gone: args.gone, timeout_ms: args.timeout_ms };
    // Waking the device first may take its own while.
    let floor = SLOW + Duration::from_millis(u64::from(args.timeout_ms)) + MARGIN;
    matched(call(client, worktree, as_caller(cmd), floor).await?, &what)
}

/// `assert`: exit 0 if it holds, 1 (`assert-failed`) otherwise.
pub async fn assert(client: &Client, worktree: &Path, args: AssertArgs) -> Outcome {
    let target = args.element.target();
    let what = format!("{} holds", name(&target));
    let cmd = SimCmdWire::Assert { target, check: args.check() };
    matched(call(client, worktree, as_caller(cmd), SLOW).await?, &what)
}

fn matched(reply: SimReplyWire, what: &str) -> Outcome {
    match reply {
        SimReplyWire::Matched { node, elapsed_ms } => {
            Ok((render::matched_json(node.as_ref(), elapsed_ms), render::matched_human(what, node.as_ref(), elapsed_ms)))
        }
        _ => Err(unexpected("Matched")),
    }
}

/// `type @eN TEXT`: tap the element (by its ref), then type — settled, when
/// asked, after the typing.
pub async fn type_into(client: &Client, worktree: &Path, words: Vec<String>, paste: bool, settle: &SettleArgs) -> Outcome {
    let [aim, text] = <[String; 2]>::try_from(words).map_err(|_| usage("`sim type` takes TEXT, or an @eN ref then TEXT"))?;
    let r = parse_ref(&aim).ok_or_else(|| usage("`sim type` takes TEXT, or an @eN ref then TEXT (see `oximux sim ax -i`)"))?;
    let tapped = call(client, worktree, as_caller(SimCmdWire::Tap(SimTargetWire::Ref(r))), SLOW).await?;
    let (tap_json, tap_human) = acted(tapped, &format!("tapped @e{r}"), false)?;
    let said = format!("typed {} characters", text.chars().count());
    let (cmd, floor) = wrap(SimCmdWire::Type { text, paste }, SLOW, settle);
    let (mut data, typed) = acted(call(client, worktree, cmd, floor).await?, &said, false)?;
    data["tap"] = tap_json;
    Ok((data, format!("{tap_human}\n{typed}")))
}

fn name(target: &SimTargetWire) -> String {
    match target {
        SimTargetWire::Label(label) => format!("“{label}”"),
        SimTargetWire::Id(id) => format!("#{id}"),
        SimTargetWire::Ref(r) => format!("@e{r}"),
        SimTargetWire::Point(p) => format!("({}, {})", p.x, p.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_is_its_session_hashed_and_nobody_without_one() {
        assert_eq!(caller_from(None), None);
        assert_eq!(caller_from(Some("  ")), None);
        let a = caller_from(Some("sess-1")).unwrap();
        assert_eq!(caller_from(Some(" sess-1 ")), Some(a));
        assert_ne!(caller_from(Some("sess-2")), Some(a));
        // Pinned: the host keys tables by it, across CLI builds.
        assert_eq!(caller_from(Some("a")), Some(0xaf63_dc4c_8601_ec8c));
    }

    #[test]
    fn a_settle_wraps_the_action_and_lengthens_the_wait() {
        let tap = SimCmdWire::Tap(SimTargetWire::Label("OK".into()));
        let (cmd, floor) = wrap(tap.clone(), SLOW, &SettleArgs::default());
        assert_eq!((cmd, floor), (tap.clone(), SLOW), "nothing asked: the v25 verb, unwrapped");
        let (cmd, floor) = wrap(tap.clone(), SLOW, &SettleArgs { settle: true, settle_timeout: Some(1500) });
        let SimCmdWire::Settled { action, timeout_ms } = (match cmd {
            SimCmdWire::AsCaller { cmd, .. } => *cmd,
            other => other,
        }) else {
            panic!("not settled");
        };
        assert_eq!((*action, timeout_ms, floor), (tap, 1500, SLOW + Duration::from_millis(1500) + MARGIN));
    }
}
