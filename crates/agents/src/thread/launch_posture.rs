//! Spawn posture: what a child agent may do, fixed when it is spawned.
//!
//! A reviewer must never write and never prompt; a verifier may build and run
//! tests but not edit sources. The posture is chosen at spawn, stored on the
//! child's lineage row, and reapplied on every respawn, so neither the agent
//! nor a later mode change can widen it. Mapping a posture onto each adapter's
//! own flags is `resolve` (see the P1 spike results for the Claude, Codex,
//! Pi and omp mappings).
//!
//! Named `SpawnPosture`, not `Posture`: the Codex, Pi and omp adapters already
//! own that word for their per-session permission settings.
//!
//! The mapping, from the P1 spike (S5, 2026-10-24, Claude 2.1.310):
//!
//! | Adapter | Review | Verify |
//! |---|---|---|
//! | Claude | `--tools Read,Grep,Glob`, `dontAsk`, `--permission-prompts none`, `--strict-mcp-config` | the same plus `Bash`, with the configured commands as `permissions.allow` in `--settings` |
//! | Codex | `("never", "read-only")` | `("never", "workspace-write")` (network stays off) |
//! | Pi | `--tools read,find,grep,ls` | unenforceable (no bash, so no tests) |
//! | ACP, omp | unenforceable | unenforceable |
//!
//! `--tools` scopes only Claude's built-in tools; an allowed MCP tool would
//! still run, which is why `--strict-mcp-config` is part of the posture and why
//! a posture-locked spec carries no MCP allow rules. An adapter that cannot
//! enforce a posture fails the launch with [`PostureUnenforceable`] — never a
//! silent downgrade to `Default`.

use super::connect::ConnectSpec;
use super::pi::posture::{PiPosture, TOOLS_READ_ONLY};
use super::Transport;

/// What a spawned agent is allowed to do, by rank: `Review < Verify < Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum SpawnPosture {
    /// Reads only. Never writes, never runs commands, never prompts.
    Review,
    /// No source edits; may build and run tests. Never prompts.
    Verify,
    /// Whatever the user configured for that agent.
    #[default]
    Default,
}

impl SpawnPosture {
    pub fn as_text(self) -> &'static str {
        match self {
            SpawnPosture::Review => "review",
            SpawnPosture::Verify => "verify",
            SpawnPosture::Default => "default",
        }
    }

    /// An unrecognised stored value reads as the *narrowest* posture: a row
    /// this build cannot interpret must not hand its child more power.
    pub fn from_text(text: &str) -> Self {
        match text {
            "default" => SpawnPosture::Default,
            "verify" => SpawnPosture::Verify,
            _ => SpawnPosture::Review,
        }
    }

    /// Whether an agent in this posture may spawn a child in `child`. The
    /// ceiling: an agent never hands a child more than it has itself.
    pub fn permits_child(self, child: SpawnPosture) -> bool {
        child <= self
    }
}

/// A CLI's `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CliVersion(pub u32, pub u32, pub u32);

impl CliVersion {
    /// Parse the first `x.y.z` in a `--version` line (`2.1.310 (Claude Code)`).
    pub fn parse(text: &str) -> Option<Self> {
        text.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|tok| {
            let mut it = tok.split('.').map(str::parse::<u32>);
            match (it.next(), it.next(), it.next()) {
                (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) => Some(CliVersion(a, b, c)),
                _ => None,
            }
        })
    }
}

impl std::fmt::Display for CliVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The oldest Claude CLI whose posture flags and input-queue frames were
/// verified live (spike S5). Lower it only after re-running the spike on the
/// lower version: the flags exist from 2.1.259, but presence is not behaviour.
pub const CLAUDE_MIN_ORCH: CliVersion = CliVersion(2, 1, 310);

/// Claude's built-in tools a reviewer keeps.
pub const CLAUDE_REVIEW_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

/// Why a posture could not be applied. The launch fails; nothing runs wider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostureUnenforceable {
    /// This adapter has no way to hold the posture.
    Adapter { transport: Transport, posture: SpawnPosture },
    /// The installed CLI predates the verified flags.
    CliVersion { found: Option<CliVersion>, need: CliVersion },
    /// The posture's settings clashed with another writer's.
    Spec(String),
}

impl std::fmt::Display for PostureUnenforceable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostureUnenforceable::Adapter { transport, posture } => write!(
                f,
                "{transport:?} agents cannot be held to the {} posture; spawn a Claude or Codex agent for it",
                posture.as_text()
            ),
            PostureUnenforceable::CliVersion { found, need } => match found {
                Some(found) => write!(f, "the locked postures need claude {need} or newer (found {found})"),
                None => write!(f, "the locked postures need claude {need} or newer (version unknown)"),
            },
            PostureUnenforceable::Spec(why) => write!(f, "posture could not be applied: {why}"),
        }
    }
}

impl std::error::Error for PostureUnenforceable {}

/// The Claude flags a posture adds (read by the stream-json arm).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePosture {
    /// `--tools`, joined with commas.
    pub tools_allow: Vec<String>,
    /// `--permission-prompts none`: anything that would prompt is denied.
    pub no_prompts: bool,
    /// `--strict-mcp-config`: only the host's `--mcp-config` servers.
    pub strict_mcp: bool,
}

/// Whether a Claude session's `system/init` shows its posture actually held:
/// the no-prompt mode, and no built-in tool outside the posture's allow-list.
/// MCP tools are not judged here — under `--strict-mcp-config` they can only
/// be the host's own servers. The flags were verified on one CLI build; this
/// catches a later build that ignores one of them, before the child acts.
pub fn claude_init_holds(posture: SpawnPosture, permission_mode: &str, tools: &[String]) -> Result<(), String> {
    if posture == SpawnPosture::Default {
        return Ok(());
    }
    if permission_mode != "dontAsk" {
        return Err(format!("the agent started in `{permission_mode}` mode, not `dontAsk`"));
    }
    let allowed = |t: &str| {
        t.starts_with("mcp__")
            || CLAUDE_REVIEW_TOOLS.contains(&t)
            || (posture == SpawnPosture::Verify && t == "Bash")
    };
    match tools.iter().find(|t| !allowed(t)) {
        Some(extra) => Err(format!("the agent started with the `{extra}` tool, outside its posture")),
        None => Ok(()),
    }
}

/// Apply `spec.posture` to `spec` for its transport. `claude_version` is asked
/// only for a locked Claude posture. Returns the Claude flags to add, if any.
pub fn apply(
    spec: &mut ConnectSpec,
    claude_version: impl FnOnce() -> Option<CliVersion>,
) -> Result<Option<ClaudePosture>, PostureUnenforceable> {
    let posture = spec.posture;
    if posture == SpawnPosture::Default {
        return Ok(None);
    }
    let unenforceable = || PostureUnenforceable::Adapter { transport: spec.transport, posture };
    match spec.transport {
        Transport::StreamJson => {
            let found = claude_version();
            if found.is_none_or(|v| v < CLAUDE_MIN_ORCH) {
                return Err(PostureUnenforceable::CliVersion { found, need: CLAUDE_MIN_ORCH });
            }
            spec.permission_mode = Some("dontAsk".to_string());
            let mut tools: Vec<String> = CLAUDE_REVIEW_TOOLS.iter().map(|t| t.to_string()).collect();
            if posture == SpawnPosture::Verify {
                tools.push("Bash".to_string());
                let allow: Vec<serde_json::Value> =
                    spec.posture_allow.iter().map(|c| serde_json::Value::from(c.as_str())).collect();
                spec.merge_settings(&serde_json::json!({ "permissions": { "allow": allow } }))
                    .map_err(|e| PostureUnenforceable::Spec(e.to_string()))?;
            }
            Ok(Some(ClaudePosture { tools_allow: tools, no_prompts: true, strict_mcp: true }))
        }
        Transport::AppServer => {
            let sandbox = match posture {
                SpawnPosture::Review => "read-only",
                _ => "workspace-write",
            };
            spec.codex_posture = Some(("never".to_string(), sandbox.to_string()));
            Ok(None)
        }
        Transport::Rpc if posture == SpawnPosture::Review => {
            let context_files = spec.pi_posture.as_ref().is_none_or(|p| p.context_files);
            spec.pi_posture = Some(PiPosture { tools: TOOLS_READ_ONLY.to_string(), context_files });
            Ok(None)
        }
        Transport::Rpc | Transport::Acp | Transport::OmpRpc => Err(unenforceable()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_is_review_then_verify_then_default() {
        assert!(SpawnPosture::Review < SpawnPosture::Verify);
        assert!(SpawnPosture::Verify < SpawnPosture::Default);
        assert!(SpawnPosture::Default.permits_child(SpawnPosture::Review));
        assert!(!SpawnPosture::Review.permits_child(SpawnPosture::Default));
        assert!(!SpawnPosture::Verify.permits_child(SpawnPosture::Default));
    }

    #[test]
    fn text_round_trips_and_unknown_is_narrowest() {
        for p in [SpawnPosture::Review, SpawnPosture::Verify, SpawnPosture::Default] {
            assert_eq!(SpawnPosture::from_text(p.as_text()), p);
        }
        assert_eq!(SpawnPosture::from_text("superuser"), SpawnPosture::Review);
    }

    fn spec(transport: Transport, posture: SpawnPosture) -> ConnectSpec {
        let mut backend = super::super::ChatBackend::stream_json();
        backend.transport = transport;
        let mut s = ConnectSpec::for_backend(&backend, "/tmp".into(), None, None, None, None);
        s.posture = posture;
        s
    }

    const NEW: fn() -> Option<CliVersion> = || Some(CLAUDE_MIN_ORCH);

    #[test]
    fn default_changes_nothing_on_any_adapter() {
        for t in [Transport::StreamJson, Transport::AppServer, Transport::Rpc, Transport::Acp, Transport::OmpRpc] {
            let mut s = spec(t, SpawnPosture::Default);
            assert_eq!(apply(&mut s, || None), Ok(None));
            assert_eq!(s.permission_mode, None);
            assert_eq!(s.codex_posture, None);
        }
    }

    #[test]
    fn claude_review_is_an_allow_list_with_no_prompts_and_strict_mcp() {
        let mut s = spec(Transport::StreamJson, SpawnPosture::Review);
        let flags = apply(&mut s, NEW).unwrap().unwrap();
        assert_eq!(flags.tools_allow, ["Read", "Grep", "Glob"]);
        assert!(flags.no_prompts && flags.strict_mcp);
        assert_eq!(s.permission_mode.as_deref(), Some("dontAsk"));
        assert_eq!(s.settings_json, None, "a reviewer gets no allow rules");
    }

    #[test]
    fn claude_verify_adds_bash_and_the_configured_commands() {
        let mut s = spec(Transport::StreamJson, SpawnPosture::Verify);
        s.posture_allow = vec!["Bash(cargo test:*)".into()];
        let flags = apply(&mut s, NEW).unwrap().unwrap();
        assert_eq!(flags.tools_allow, ["Read", "Grep", "Glob", "Bash"]);
        let v: serde_json::Value = serde_json::from_str(s.settings_json.as_deref().unwrap()).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(cargo test:*)");
    }

    #[test]
    fn an_old_or_unknown_claude_is_refused() {
        let mut s = spec(Transport::StreamJson, SpawnPosture::Review);
        let old = apply(&mut s, || Some(CliVersion(2, 1, 258)));
        assert!(matches!(old, Err(PostureUnenforceable::CliVersion { found: Some(_), .. })));
        assert!(matches!(apply(&mut s, || None), Err(PostureUnenforceable::CliVersion { found: None, .. })));
    }

    #[test]
    fn codex_maps_to_never_with_the_posture_sandbox() {
        let mut r = spec(Transport::AppServer, SpawnPosture::Review);
        apply(&mut r, || None).unwrap();
        assert_eq!(r.codex_posture, Some(("never".into(), "read-only".into())));
        let mut v = spec(Transport::AppServer, SpawnPosture::Verify);
        apply(&mut v, || None).unwrap();
        assert_eq!(v.codex_posture, Some(("never".into(), "workspace-write".into())));
    }

    #[test]
    fn pi_reviews_read_only_and_cannot_verify() {
        let mut r = spec(Transport::Rpc, SpawnPosture::Review);
        apply(&mut r, || None).unwrap();
        assert_eq!(r.pi_posture.unwrap().to_args(), ["--tools", "read,find,grep,ls"]);
        let mut v = spec(Transport::Rpc, SpawnPosture::Verify);
        assert!(matches!(apply(&mut v, || None), Err(PostureUnenforceable::Adapter { .. })));
    }

    #[test]
    fn acp_and_omp_refuse_every_locked_posture() {
        for t in [Transport::Acp, Transport::OmpRpc] {
            for p in [SpawnPosture::Review, SpawnPosture::Verify] {
                assert!(matches!(apply(&mut spec(t, p), || None), Err(PostureUnenforceable::Adapter { .. })));
            }
        }
    }

    #[test]
    fn an_init_that_ignored_the_posture_is_caught() {
        let tools = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let review = SpawnPosture::Review;
        assert!(claude_init_holds(review, "dontAsk", &tools(&["Glob", "Grep", "Read", "mcp__oximux__x"])).is_ok());
        assert!(claude_init_holds(review, "default", &tools(&["Read"])).is_err(), "wrong mode");
        assert!(claude_init_holds(review, "dontAsk", &tools(&["Read", "Bash"])).is_err(), "Bash leaked");
        assert!(claude_init_holds(SpawnPosture::Verify, "dontAsk", &tools(&["Read", "Bash"])).is_ok());
        assert!(claude_init_holds(SpawnPosture::Default, "default", &tools(&["Bash", "Edit"])).is_ok());
    }

    #[test]
    fn versions_parse_from_the_cli_banner() {
        assert_eq!(CliVersion::parse("2.1.310 (Claude Code)"), Some(CliVersion(2, 1, 310)));
        assert_eq!(CliVersion::parse("codex-cli 0.163.0"), Some(CliVersion(0, 163, 0)));
        assert_eq!(CliVersion::parse("no version here"), None);
        assert!(CliVersion(2, 1, 259) < CLAUDE_MIN_ORCH);
    }
}
