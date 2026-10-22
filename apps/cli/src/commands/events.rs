//! `oximux events` — stream session changes as they happen, over the host's
//! session-list subscription (the same one the phone uses, so nothing polls).
//!
//! The host pushes the whole list on every change; this diffs consecutive
//! lists into one line per session added, removed, or changed. `last_seq` is
//! left out of the diff: it moves on every agent event, and a line per token
//! would bury the changes a caller is waiting for.
//!
//! v1 reports lifecycle and the permission flag only — the summary carries no
//! turn status, so `--until idle` and friends wait on a status frame the host
//! does not send yet. `oximux wait` covers one session in the meantime.

use std::collections::BTreeMap;
use std::io::Write as _;

use oximux_remote_proto::messages::SessionSummary;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::{EventsUntil, exit};
use crate::client::{Client, is_push, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// What changed about one session between two lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Removed,
    Changed,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Added => "added",
            Kind::Removed => "removed",
            Kind::Changed => "changed",
        }
    }
}

/// The sessions being watched, by id — every one, or just `--session`.
type Watched = BTreeMap<String, SessionSummary>;

fn watched(rows: Vec<SessionSummary>, only: Option<&str>) -> Watched {
    rows.into_iter()
        .filter(|s| only.is_none_or(|id| s.session_id == id))
        .map(|s| (s.session_id.clone(), s))
        .collect()
}

/// The changes from `before` to `after`: additions and changes in id order,
/// then removals.
pub fn diff(before: &Watched, after: &Watched) -> Vec<(Kind, SessionSummary)> {
    let mut out = Vec::new();
    for (id, now) in after {
        match before.get(id) {
            None => out.push((Kind::Added, now.clone())),
            Some(was)
                if was.title != now.title
                    || was.model != now.model
                    || was.awaiting_permission != now.awaiting_permission =>
            {
                out.push((Kind::Changed, now.clone()));
            }
            Some(_) => {}
        }
    }
    for (id, was) in before {
        if !after.contains_key(id) {
            out.push((Kind::Removed, was.clone()));
        }
    }
    out
}

/// Print one change line. `false` once stdout is gone (`events | head -1`),
/// which ends the stream instead of panicking on the broken pipe.
fn emit(kind: Kind, s: &SessionSummary, json_mode: bool) -> bool {
    let now = chrono::Local::now();
    let mut out = std::io::stdout().lock();
    let written = if json_mode {
        writeln!(
            out,
            "{}",
            json!({
                "ts": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "kind": kind.as_str(),
                "session": s.session_id,
                "title": s.title,
                "model": s.model,
                "awaiting_permission": s.awaiting_permission,
            })
        )
    } else {
        let flag = if s.awaiting_permission { "  [awaiting permission]" } else { "" };
        writeln!(
            out,
            "{}  {:<7}  {}  {}{flag}",
            now.format("%H:%M:%S"),
            kind.as_str(),
            s.session_id,
            s.title
        )
    };
    written.and_then(|()| out.flush()).is_ok()
}

/// What one list means for the stream.
enum Step {
    Continue,
    Done(Result<(Value, String), Failure>),
}

/// Diff `rows` against `current`, print the changes, and say whether the
/// stream is over: `--until` met, stdout closed, or the one watched session
/// gone (it cannot come back, so waiting on would never end).
fn apply(
    current: &mut Watched,
    rows: Vec<SessionSummary>,
    only: Option<&str>,
    until: Option<EventsUntil>,
    json_mode: bool,
) -> Step {
    let next = watched(rows, only);
    for (kind, s) in diff(current, &next) {
        if !emit(kind, &s, json_mode) {
            // Whoever read the stream has stopped (`events | head -1`). No
            // result line either: it would only hit the same closed pipe.
            std::process::exit(exit::OK.into());
        }
        if kind == Kind::Removed && only.is_some() {
            let id = s.session_id.clone();
            return Step::Done(Err(Failure::new(
                "session-gone",
                exit::ERROR,
                format!("session {id} ended"),
            )
            .with_data(json!({ "session": id }))));
        }
    }
    *current = next;
    // Checked against the state already in force too: waiting for a state
    // the session is in now returns at once, as `oximux wait` does.
    match met(until, current) {
        Some(session) => Step::Done(Ok(reached(session))),
        None => Step::Continue,
    }
}

/// The first watched session waiting on a person, if `--until` asks for one.
fn met(until: Option<EventsUntil>, sessions: &Watched) -> Option<String> {
    match until? {
        EventsUntil::AwaitingPermission => {
            sessions.values().find(|s| s.awaiting_permission).map(|s| s.session_id.clone())
        }
    }
}

fn reached(session: String) -> (Value, String) {
    (
        json!({ "until": "awaiting-permission", "session": session }),
        format!("{session} is waiting for permission"),
    )
}

pub async fn run(
    client: &Client,
    only: Option<&str>,
    until: Option<EventsUntil>,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    // A change the host pushes before its snapshot reply is kept, not dropped,
    // and applied after the snapshot so nothing between the two is missed.
    let mut early = Vec::new();
    let reply = client
        .call_streaming(Request::SubscribeSessions, |push| {
            if let Response::SessionsChanged(rows) = push {
                early.push(rows);
            }
        })
        .await?;
    let initial = match reply {
        Response::Sessions(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("SubscribeSessions", &other)),
    };

    // The sessions already open come first, as `added`, so a consumer starts
    // from the same picture as `oximux ls` without a second call.
    let mut current = Watched::new();
    for rows in std::iter::once(initial).chain(early) {
        if let Step::Done(outcome) = apply(&mut current, rows, only, until, json_mode) {
            return outcome;
        }
    }

    // One Ctrl+C listener for the whole stream, so a press between two frames
    // is not lost to a listener that was not yet re-created.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let frame = tokio::select! {
            frame = client.recv_frame() => frame?,
            _ = &mut ctrl_c => {
                return Err(Failure::new("interrupted", exit::INTERRUPTED, "stopped (Ctrl+C)"));
            }
        };
        let Response::SessionsChanged(rows) = frame else {
            if !is_push(&frame) {
                return Err(unexpected_reply("SubscribeSessions", &frame));
            }
            continue;
        };
        if let Step::Done(outcome) = apply(&mut current, rows, only, until, json_mode) {
            return outcome;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str, awaiting: bool) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            title: title.into(),
            model: None,
            last_seq: 0,
            awaiting_permission: awaiting,
        }
    }

    fn kinds(changes: &[(Kind, SessionSummary)]) -> Vec<(Kind, &str)> {
        changes.iter().map(|(k, s)| (*k, s.session_id.as_str())).collect()
    }

    #[test]
    fn a_diff_reports_added_changed_and_removed_sessions() {
        let before = watched(vec![row("a", "one", false), row("b", "two", false)], None);
        let after = watched(vec![row("a", "one", true), row("c", "three", false)], None);
        assert_eq!(
            kinds(&diff(&before, &after)),
            [(Kind::Changed, "a"), (Kind::Added, "c"), (Kind::Removed, "b")]
        );
    }

    #[test]
    fn progress_alone_is_not_a_change() {
        let before = watched(vec![row("a", "one", false)], None);
        let mut moved = row("a", "one", false);
        moved.last_seq = 99;
        assert!(diff(&before, &watched(vec![moved], None)).is_empty());
    }

    #[test]
    fn the_watched_session_ending_ends_the_stream() {
        let mut current = watched(vec![row("b", "two", false)], Some("b"));
        let Step::Done(Err(failure)) = apply(&mut current, vec![row("a", "one", false)], Some("b"), None, true)
        else {
            panic!("a watched session that is gone cannot be waited on");
        };
        assert_eq!((failure.code, failure.exit), ("session-gone", exit::ERROR));
        // Watching everything, one session ending is just a line.
        let mut all = watched(vec![row("b", "two", false)], None);
        assert!(matches!(apply(&mut all, vec![], None, None, true), Step::Continue));
    }

    #[test]
    fn a_session_filter_watches_only_that_session() {
        let rows = vec![row("a", "one", true), row("b", "two", false)];
        let only_b = watched(rows.clone(), Some("b"));
        assert_eq!(only_b.keys().collect::<Vec<_>>(), ["b"]);
        // `a` waits for permission, but it is not the one being watched.
        assert_eq!(met(Some(EventsUntil::AwaitingPermission), &only_b), None);
        assert_eq!(
            met(Some(EventsUntil::AwaitingPermission), &watched(rows, None)),
            Some("a".into())
        );
        assert_eq!(met(None, &watched(vec![row("a", "one", true)], None)), None);
    }
}
