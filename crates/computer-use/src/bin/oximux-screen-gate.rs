//! The `PreToolUse` hook that decides a chat's screen-control calls.
//!
//! # Why this exists as a separate process
//!
//! The obvious place to enforce is the permission round-trip OxiMux already
//! handles. Measured against the shipping CLI, that round-trip **does not
//! always happen**: a tool call skips it entirely under
//! `--permission-mode bypassPermissions`, and — worse, because it needs no
//! unusual setting — under any `permissions.allow` rule the user has, in the
//! *default* mode. A policy hung there silently stops running.
//!
//! A `PreToolUse` hook fires in every permission mode, receives the full tool
//! input, and can deny with a reason the agent reads. That is the only
//! interception point measured to hold, so this is where the policy lives.
//!
//! # Contract
//!
//! Reads the hook payload as JSON on stdin, writes a decision as JSON on
//! stdout, exits 0. Emitting **nothing** means "no opinion", which is what every
//! non-screen-control tool gets — this binary must never influence a tool it
//! does not own.
//!
//! Spawned once per tool call, so it does no work it can avoid: no daemon, no
//! network, and the grant store is a single locked read.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oximux_computer_use::grants::{GrantTable, Provenance};
use oximux_computer_use::policy::{decide, Decision, PolicyContext};
use oximux_computer_use::session::SessionId;

/// What OxiMux tells the hook about the chat it is serving.
struct Args {
    /// The chat's screen-control session id — its identity in the grant store.
    chat: String,
    /// Path to the shared grant store. Passed rather than derived so the app
    /// and the hook cannot resolve different files.
    grants: PathBuf,
    /// OxiMux's own executable, so the policy can recognise a call aimed at us.
    ///
    /// This process is the gate, not OxiMux, so `current_exe()` answers a
    /// different question than the one being asked. Optional only because an
    /// older command line would otherwise be rejected outright, which fails in
    /// the worse direction — a chat with no gate at all.
    host: Option<PathBuf>,
    /// The chat's worktree, for build provenance.
    worktree: Option<PathBuf>,
    /// When the chat started, as seconds since the epoch. A binary older than
    /// this was not built by it.
    since: Option<SystemTime>,
}

fn main() {
    let Some(args) = parse_args(std::env::args().skip(1)) else {
        // Misconfigured hook. Say nothing: a malformed decision could block
        // unrelated tools, and OxiMux writes this command line itself, so a
        // parse failure is a bug here rather than something to enforce on.
        eprintln!("oximux-screen-gate: expected --chat <id> --grants <path>");
        return;
    };

    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };

    let tool_name = payload["tool_name"].as_str().unwrap_or_default();
    let input = payload.get("tool_input").cloned().unwrap_or(serde_json::Value::Null);

    let session = SessionId::for_agent(&args.chat);
    let grants = GrantTable::at(&args.grants);
    let provenance = args
        .worktree
        .as_deref()
        .zip(args.since)
        .and_then(|(root, since)| Provenance::new(root, since));

    let decision = decide(
        tool_name,
        &input,
        &PolicyContext {
            session: &session,
            grants: &grants,
            provenance: provenance.as_ref(),
            host: args.host.as_deref(),
        },
    );

    // An allowed call is the moment this chat starts actually driving, as
    // opposed to merely holding the right to. Recorded here because this hook is
    // the only thing that sees every call: the provenance path grants without
    // OxiMux ever showing a card, so a mark written at the card would miss the
    // most common case entirely. Cleared at the turn boundary by the chat.
    if matches!(decision, Decision::Allow) {
        grants.begin_driving_turn(&session);
    }

    if let Some(output) = render(&decision) {
        println!("{output}");
    }
}

/// The hook's reply, or `None` to stay silent.
///
/// `Ask` returns no output rather than `permissionDecision: "ask"`: asking is
/// what already happens when the hook says nothing, and staying silent leaves
/// the existing card path — including its suggestions and the grant recorded on
/// approval — exactly as it is.
fn render(decision: &Decision) -> Option<String> {
    let (verdict, reason) = match decision {
        Decision::NotApplicable | Decision::Ask { .. } => return None,
        Decision::Allow => ("allow", String::new()),
        Decision::Refuse { reason } => ("deny", reason.clone()),
    };
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": verdict,
                "permissionDecisionReason": reason,
            }
        })
        .to_string(),
    )
}

fn parse_args(args: impl Iterator<Item = String>) -> Option<Args> {
    let mut chat = None;
    let mut grants = None;
    let mut host = None;
    let mut worktree = None;
    let mut since = None;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let value = args.next();
        match (flag.as_str(), value) {
            ("--chat", Some(v)) => chat = Some(v),
            ("--grants", Some(v)) => grants = Some(PathBuf::from(v)),
            ("--host-exe", Some(v)) => host = Some(PathBuf::from(v)),
            ("--worktree", Some(v)) => worktree = Some(PathBuf::from(v)),
            ("--since", Some(v)) => {
                since = v
                    .parse::<u64>()
                    .ok()
                    .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
            }
            _ => return None,
        }
    }
    Some(Args {
        chat: chat?,
        grants: grants?,
        host,
        worktree,
        since,
    })
}

/// Seconds since the epoch, for building the `--since` argument. Lives here so
/// the writer and the reader of that flag share one definition.
pub fn epoch_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    fn args(list: &[&str]) -> Option<Args> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_the_command_line_oximux_writes() {
        let parsed = args(&[
            "--chat",
            "chat-7",
            "--grants",
            "/tmp/grants.json",
            "--host-exe",
            "/Applications/OxiMux.app/Contents/MacOS/oximux",
            "--worktree",
            "/repo",
            "--since",
            "1700000000",
        ])
        .expect("valid");
        assert_eq!(parsed.chat, "chat-7");
        assert_eq!(parsed.grants, Path::new("/tmp/grants.json"));
        assert_eq!(
            parsed.host.as_deref(),
            Some(Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"))
        );
        assert_eq!(parsed.worktree.as_deref(), Some(Path::new("/repo")));
        assert_eq!(parsed.since.map(epoch_seconds), Some(1_700_000_000));
    }

    #[test]
    fn provenance_arguments_are_optional() {
        // A chat whose worktree cannot be resolved still gets a policy; it just
        // asks about every target instead of trusting its own builds.
        let parsed = args(&["--chat", "c", "--grants", "/tmp/g.json"]).expect("valid");
        assert!(parsed.worktree.is_none());
        assert!(parsed.since.is_none());
        // Likewise the host: an older command line must still produce a gate,
        // because rejecting it would leave the chat with none at all.
        assert!(parsed.host.is_none());
    }

    #[test]
    fn a_missing_required_flag_is_rejected() {
        assert!(args(&["--chat", "c"]).is_none());
        assert!(args(&["--grants", "/tmp/g.json"]).is_none());
        assert!(args(&["--chat"]).is_none());
        assert!(args(&["--unknown", "x", "--chat", "c", "--grants", "/g"]).is_none());
    }

    #[test]
    fn a_refusal_renders_the_shape_the_cli_expects() {
        // Field names are the contract with Claude Code; a typo here is a hook
        // that silently allows everything.
        let out = render(&Decision::Refuse {
            reason: "no target process".into(),
        })
        .expect("some output");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecisionReason"],
            "no target process"
        );
    }

    #[test]
    fn an_allowance_renders_as_allow() {
        let out = render(&Decision::Allow).expect("some output");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn tools_this_binary_does_not_own_get_no_opinion() {
        // The most important negative. Any output at all for an unrelated tool
        // would put this hook in the path of every tool the agent has.
        assert!(render(&Decision::NotApplicable).is_none());
    }

    #[test]
    fn ask_stays_silent_so_the_existing_card_path_runs() {
        // Saying nothing is already "ask the user", and it leaves the card's
        // suggestions and the grant-on-approval intact.
        assert!(render(&Decision::Ask { pid: Some(42) }).is_none());
        assert!(render(&Decision::Ask { pid: None }).is_none());
    }
}
