//! `oximux agent hooks` — install, remove, and inspect the status hooks that
//! let an agent CLI say what it is doing.
//!
//! Entirely offline. Every verb here reads or writes files in the agents' own
//! config directories on this machine and never contacts a host, which is the
//! point: a `status` that only answered while the app was up would be useless
//! precisely when it is reached for — a hook misbehaving, an agent not
//! reporting, the app not starting.

use std::path::PathBuf;

use oximux_agent_hooks::agent_hook_dialects::{self, DIALECTS, HookDialect};
use oximux_agent_hooks::agent_hooks_global::{Applied, apply};
use oximux_agent_hooks::inspect::{self, HookState};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::output::Failure;

/// The dialects a verb should act on: one named agent, or the whole table.
fn selected(agent: Option<&str>) -> Result<Vec<&'static HookDialect>, Failure> {
    let Some(slug) = agent else {
        return Ok(DIALECTS.iter().collect());
    };
    match agent_hook_dialects::dialect_for_slug(slug) {
        Some(dialect) => Ok(vec![dialect]),
        None => Err(Failure::new(
            "usage",
            exit::USAGE,
            format!(
                "unknown agent {slug:?} — must be {}",
                agent_hook_dialects::known_slugs()
            ),
        )),
    }
}

/// The binary an installed hook calls back into: this one.
///
/// Deliberately `current_exe` rather than a hunt for the desktop app. Both
/// binaries answer `agent-status`, and the copy on `PATH` is the stable one —
/// an app bundle is replaced wholesale by every update, which is why the app
/// refreshes these paths on each boot. On a headless host there is no bundle to
/// find at all, and this is the only binary there is.
fn hook_binary() -> Result<PathBuf, Failure> {
    std::env::current_exe().map_err(|err| {
        Failure::new(
            "io",
            exit::ERROR,
            format!("cannot resolve this binary's own path, so no hook can call back into it: {err}"),
        )
    })
}

/// `agent hooks status`.
pub fn status(agent: Option<&str>) -> Result<(Value, String), Failure> {
    let rows: Vec<_> = selected(agent)?
        .into_iter()
        .map(inspect::status_of)
        .collect();

    let data = json!({
        "hooks": rows.iter().map(|r| json!({
            "agent": r.slug,
            "name": r.agent,
            "state": r.state.label(),
            "installed": r.state.is_installed(),
            "foreign": r.state.foreign(),
            // Always present, even when nothing was found there: knowing where
            // OxiMux looked is most of the value when the answer is "nothing".
            "path": r.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        })).collect::<Vec<_>>(),
    });

    // Both columns are sized from the rows actually being printed. A fixed
    // width silently breaks the moment a state grows a count on the end
    // ("on (+19 yours)"), and a listing whose paths do not line up is harder
    // to scan than one with no alignment at all.
    let states: Vec<String> = rows.iter().map(|r| describe(&r.state)).collect();
    let slug_w = rows.iter().map(|r| r.slug.len()).max().unwrap_or(0);
    let state_w = states.iter().map(String::len).max().unwrap_or(0);
    let mut human = String::new();
    for (row, state) in rows.iter().zip(&states) {
        let path = row
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(no config directory on this platform)".into());
        human.push_str(&format!(
            "{:<slug_w$}  {state:<state_w$}  {path}\n",
            row.slug,
        ));
    }
    if rows.iter().all(|r| r.state == HookState::AgentAbsent) {
        human.push_str("\nNo agent config directories found — OxiMux adds to an agent's own\nhome and never creates one, so run an agent once first.\n");
    }
    Ok((data, human))
}

/// The state as a human reads it, with the counts that change what to do next.
fn describe(state: &HookState) -> String {
    match state {
        HookState::AgentAbsent => "not installed".into(),
        HookState::NoFile => "no hooks file".into(),
        HookState::Absent { foreign: 0 } => "off".into(),
        HookState::Absent { foreign } => format!("off (+{foreign} yours)"),
        HookState::Installed { foreign: 0, .. } => "on".into(),
        HookState::Installed { foreign, .. } => format!("on (+{foreign} yours)"),
        HookState::Unreadable(_) => "unreadable".into(),
    }
}

/// `agent hooks on` / `agent hooks off`.
///
/// Best-effort per agent, like the app's own sync: one file that cannot be
/// written is reported and the rest still get theirs. The exit code is still 0
/// — the request was carried out everywhere it could be, and a machine that
/// happens not to have Gemini installed is not an error.
pub fn set(on: bool, agent: Option<&str>) -> Result<(Value, String), Failure> {
    let dialects = selected(agent)?;
    let exe = hook_binary()?;

    let mut results = Vec::new();
    for dialect in dialects {
        let outcome = apply(on, dialect, &exe);
        results.push((dialect, outcome));
    }

    let data = json!({
        "hooks": results.iter().map(|(d, outcome)| json!({
            "agent": d.slug,
            "name": d.agent,
            "outcome": outcome_slug(outcome),
            "error": match outcome { Applied::Failed(err) => Some(err.clone()), _ => None },
            "path": d.path().map(|p| p.to_string_lossy().into_owned()),
        })).collect::<Vec<_>>(),
    });

    let width = results.iter().map(|(d, _)| d.slug.len()).max().unwrap_or(0);
    let mut human = String::new();
    for (dialect, outcome) in &results {
        human.push_str(&format!(
            "{:<width$}  {}\n",
            dialect.slug,
            outcome_line(outcome, dialect),
            width = width
        ));
    }
    if results.iter().any(|(_, o)| matches!(o, Applied::Changed)) && on {
        human.push_str(
            "\nAn agent that keeps a trusted-hash ledger will ask you to approve the\n\
             new hook in its own prompt before it runs. Until then nothing changes.\n",
        );
    }
    Ok((data, human))
}

fn outcome_slug(outcome: &Applied) -> &'static str {
    match outcome {
        Applied::Changed => "changed",
        Applied::Removed => "removed",
        Applied::Unchanged => "unchanged",
        Applied::AgentAbsent => "agent-absent",
        Applied::KeptForeign => "kept-foreign",
        Applied::Failed(_) => "failed",
    }
}

fn outcome_line(outcome: &Applied, dialect: &HookDialect) -> String {
    match outcome {
        Applied::Changed => "installed".into(),
        Applied::Removed => "removed".into(),
        Applied::Unchanged => "already in that state".into(),
        Applied::AgentAbsent => "not installed on this machine — skipped".into(),
        Applied::KeptForeign => format!(
            "left alone: {} holds hooks OxiMux did not write",
            dialect
                .path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "the file".into())
        ),
        Applied::Failed(err) => format!("failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_agent_is_a_usage_error_that_names_the_real_ones() {
        let Err(err) = selected(Some("clyde")) else {
            panic!("an unknown agent must be refused");
        };
        assert_eq!(err.exit, exit::USAGE);
        assert!(err.message.contains("clyde"), "{}", err.message);
        assert!(err.message.contains("claude"), "{}", err.message);
    }

    #[test]
    fn no_agent_flag_selects_the_whole_table() {
        assert_eq!(selected(None).unwrap().len(), DIALECTS.len());
    }

    #[test]
    fn status_reports_every_dialect_with_a_state_and_a_path_key() {
        // The JSON contract agents drive this CLI by: one row per dialect, and
        // `path` present (possibly null) on every one of them.
        let (data, human) = status(None).expect("status never fails");
        let rows = data["hooks"].as_array().expect("array");
        assert_eq!(rows.len(), DIALECTS.len());
        for row in rows {
            assert!(row["agent"].is_string());
            assert!(row["state"].is_string());
            assert!(row["installed"].is_boolean());
            assert!(row.get("path").is_some(), "path key must always be present");
        }
        for d in DIALECTS {
            assert!(human.contains(d.slug), "{} missing from the listing", d.slug);
        }
    }

    #[test]
    fn every_state_renders_a_distinct_line() {
        // `off` and `off (+2 yours)` must not collapse: the second says a
        // removal will leave something behind, which changes what to do next.
        let lines = [
            describe(&HookState::AgentAbsent),
            describe(&HookState::NoFile),
            describe(&HookState::Absent { foreign: 0 }),
            describe(&HookState::Absent { foreign: 2 }),
            describe(&HookState::Installed { ours: 4, foreign: 0 }),
            describe(&HookState::Installed { ours: 4, foreign: 2 }),
            describe(&HookState::Unreadable("x".into())),
        ];
        let unique: std::collections::HashSet<_> = lines.iter().collect();
        assert_eq!(unique.len(), lines.len(), "{lines:?}");
    }

    #[test]
    fn every_outcome_has_a_slug_and_a_line() {
        let dialect = agent_hook_dialects::dialect_for_slug("claude").unwrap();
        for outcome in [
            Applied::Changed,
            Applied::Removed,
            Applied::Unchanged,
            Applied::AgentAbsent,
            Applied::KeptForeign,
            Applied::Failed("disk full".into()),
        ] {
            assert!(!outcome_slug(&outcome).is_empty());
            assert!(!outcome_line(&outcome, dialect).is_empty());
        }
        assert!(
            outcome_line(&Applied::Failed("disk full".into()), dialect).contains("disk full"),
            "a failure must carry its cause"
        );
    }
}
